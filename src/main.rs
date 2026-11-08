use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use speedcraft::book::BookCache;
use speedcraft::config::{Config, Secrets, EXAMPLE_TOML};
use speedcraft::engine::Engine;
use speedcraft::ledger::{read_ledger, Ledger, LedgerEvent};
use speedcraft::replay::{read_quotes, QuoteWriter};
use speedcraft::runtime::{
    build_engine, build_live_exchanges, build_paper_exchanges, check_live_gates, run_engine, Mode,
};
use speedcraft::server::agents::AgentRegistry;
use speedcraft::server::api::generate_token;
use speedcraft::server::{router, AppState, Supervisor};
use speedcraft::types::TopOfBook;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "speedcraft",
    version,
    about = "Cross-exchange crypto arbitrage with hard risk limits. Paper mode by default."
)]
struct Cli {
    /// Path to config TOML.
    #[arg(short, long, default_value = "config.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run against live market data with simulated fills (no API keys needed). Default.
    Paper,
    /// Trade with real money. Requires `live = true` in config AND SPEEDCRAFT_I_ACCEPT_LOSSES=yes.
    Live {
        #[arg(long)]
        live: bool,
    },
    /// Validate config, credentials, fees and market rules, then exit.
    Check,
    /// Record live top-of-book quotes to a JSONL file for later replay.
    Record {
        #[arg(long, default_value = "quotes.jsonl")]
        out: PathBuf,
        /// Stop after this many seconds (0 = until Ctrl-C).
        #[arg(long, default_value_t = 0)]
        seconds: u64,
    },
    /// Replay a recorded quote file through the full pipeline with paper fills.
    Replay { file: PathBuf },
    /// Rank pairs in a quote recording by real cross-exchange edge at several fee tiers.
    Scan {
        file: PathBuf,
        /// Max ms between two venues' quotes to count as simultaneous.
        #[arg(long, default_value_t = 3000)]
        max_gap_ms: i64,
        /// Also write the full stats as JSON here.
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Summarise a ledger file.
    Report {
        #[arg(default_value = "ledger.jsonl")]
        file: PathBuf,
    },
    /// Print the example config.
    ExampleConfig,
    /// Run the local desktop UI (opens in your browser). Controls the local engine and any remote agents.
    Ui {
        #[arg(long, default_value = "127.0.0.1:7700")]
        listen: SocketAddr,
        /// Do not open the browser automatically.
        #[arg(long)]
        no_open: bool,
        /// Require this bearer token on the API (mandatory when not bound to localhost).
        #[arg(long)]
        token: Option<String>,
        /// Start an engine immediately: paper or live.
        #[arg(long)]
        autostart: Option<String>,
    },
    /// Run as a headless agent on a server/VM, controlled from the UI over HTTP.
    Agent {
        #[arg(long, default_value = "127.0.0.1:7777")]
        listen: SocketAddr,
        /// Display name shown in the UI.
        #[arg(long, default_value = "agent")]
        name: String,
        /// Bearer token. Takes precedence over --token-file.
        #[arg(long)]
        token: Option<String>,
        /// File holding the token; created with a random token if missing. Relative to the config directory.
        #[arg(long, default_value = "agent.token")]
        token_file: PathBuf,
        /// Start an engine immediately: paper or live.
        #[arg(long)]
        autostart: Option<String>,
    },
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,speedcraft=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn shutdown_signal() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(true);
    });
    rx
}

async fn run_cli_engine(cfg: Config, mode: Mode) -> Result<()> {
    let engine = build_engine(cfg, mode)?;
    let shutdown = shutdown_signal();
    run_engine(engine, shutdown, None, None).await
}

/// Resolve the agent token: explicit value, else a token file (created with
/// a fresh random token if missing).
fn resolve_token(
    token: Option<String>,
    token_file: Option<PathBuf>,
    cfg_path: &std::path::Path,
) -> Result<Option<String>> {
    if let Some(t) = token.filter(|t| !t.trim().is_empty()) {
        return Ok(Some(t.trim().to_string()));
    }
    let Some(path) = token_file else {
        return Ok(None);
    };
    let path = if path.is_absolute() {
        path
    } else {
        cfg_path.parent().map(|d| d.join(&path)).unwrap_or(path)
    };
    if path.exists() {
        let t = std::fs::read_to_string(&path)?.trim().to_string();
        if t.is_empty() {
            bail!("token file {} is empty", path.display());
        }
        return Ok(Some(t));
    }
    let t = generate_token();
    std::fs::write(&path, format!("{t}\n"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    eprintln!(
        "generated new agent token and saved it to {}",
        path.display()
    );
    Ok(Some(t))
}

async fn serve(
    mut state: AppState,
    listen: SocketAddr,
    open_browser: bool,
    autostart: Option<String>,
    allow_quit: bool,
) -> Result<()> {
    let (quit_tx, mut quit_rx) = mpsc::channel::<()>(1);
    if allow_quit {
        state.quit = Some(quit_tx);
    }
    if let Some(mode) = autostart {
        let m = Mode::parse(&mode)
            .ok_or_else(|| anyhow::anyhow!("--autostart must be paper or live"))?;
        // `--autostart live` on the command line is the explicit confirmation.
        state
            .sup
            .start(m, Some(speedcraft::runtime::LIVE_CONFIRM_PHRASE))
            .context("autostart failed")?;
    }
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding {listen}"))?;
    let url = format!("http://{}", listener.local_addr()?);
    tracing::info!("serving Speedcraft at {url}");
    println!("Speedcraft is running at {url}");
    if open_browser {
        let _ = open_in_browser(&url);
    }
    let sup = state.sup.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = quit_rx.recv() => { tracing::info!("quit requested from the panel"); }
            }
            tracing::info!("shutting down: stopping engine first");
            if sup.stop().is_ok() {
                sup.wait_stopped(std::time::Duration::from_secs(15)).await;
            }
        })
        .await?;
    Ok(())
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![url]);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let cmd = ("cmd", vec!["/C", "start", url]);
    std::process::Command::new(cmd.0)
        .args(cmd.1)
        .spawn()
        .map(|_| ())
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Paper);
    if let Cmd::ExampleConfig = cmd {
        print!("{EXAMPLE_TOML}");
        return Ok(());
    }
    let cfg = Config::load(&cli.config)?;
    match cmd {
        Cmd::ExampleConfig => unreachable!(),
        Cmd::Paper => {
            tracing::warn!("PAPER MODE: live market data, simulated fills, no real orders");
            run_cli_engine(cfg, Mode::Paper).await
        }
        Cmd::Live { live } => {
            check_live_gates(&cfg, live)?;
            tracing::error!(
                "LIVE MODE: real orders will be sent. Max daily loss {} quote",
                cfg.risk.max_daily_loss_quote
            );
            run_cli_engine(cfg, Mode::Live).await
        }
        Cmd::Ui {
            listen,
            no_open,
            token,
            autostart,
        } => {
            if !listen.ip().is_loopback()
                && token
                    .as_deref()
                    .map(|t| t.trim().is_empty())
                    .unwrap_or(true)
            {
                bail!("binding the UI to {listen} (not localhost) requires --token");
            }
            let cfg_path = std::fs::canonicalize(&cli.config)?;
            let registry = AgentRegistry::open(&cfg_path.with_file_name("agents.json"))?;
            let sup = Supervisor::new("local", cfg_path);
            let state = AppState {
                sup,
                agents: Some(Arc::new(registry)),
                token: token.filter(|t| !t.trim().is_empty()),
                http: speedcraft::exchanges::http_client(),
                quit: None,
            };
            serve(state, listen, !no_open, autostart, true).await
        }
        Cmd::Agent {
            listen,
            name,
            token,
            token_file,
            autostart,
        } => {
            let cfg_path = std::fs::canonicalize(&cli.config)?;
            let token = resolve_token(token, Some(token_file), &cfg_path)?;
            if !listen.ip().is_loopback() && token.is_none() {
                bail!("binding the agent to {listen} (not localhost) requires a token");
            }
            let sup = Supervisor::new(name, cfg_path);
            let state = AppState {
                sup,
                agents: None,
                token,
                http: speedcraft::exchanges::http_client(),
                quit: None,
            };
            serve(state, listen, false, autostart, false).await
        }
        Cmd::Check => {
            let secrets = Secrets::from_env();
            let mut problems = 0;
            for id in cfg.enabled_exchanges() {
                if secrets.has(id) {
                    println!("{id}: credentials present");
                } else {
                    println!("{id}: NO credentials (paper mode only)");
                }
            }
            let require = cfg.enabled_exchanges().iter().all(|e| secrets.has(*e));
            let exchanges = build_live_exchanges(&cfg, &secrets, false)?;
            for (id, ex) in &exchanges {
                for p in cfg.pairs() {
                    if cfg.market_symbol(*id, &p).is_none() {
                        continue;
                    }
                    match ex.fetch_market_rules(&p).await {
                        Ok(r) => println!(
                            "{id} {p}: qty_step={} tick={} min_qty={} min_notional={}",
                            r.qty_step, r.price_tick, r.min_qty, r.min_notional
                        ),
                        Err(e) => {
                            problems += 1;
                            println!("{id} {p}: market rules FAILED: {e}");
                        }
                    }
                }
                if secrets.has(*id) {
                    match ex.fetch_fees().await {
                        Ok(f) => {
                            let floor = cfg.fee_floor(*id);
                            println!("{id}: fees taker={}bps maker={}bps (config floor taker={}bps) -> using {}bps", f.taker_bps, f.maker_bps, floor.taker_bps, f.taker_bps.max(floor.taker_bps));
                        }
                        Err(e) => {
                            problems += 1;
                            println!("{id}: fee fetch FAILED: {e}");
                        }
                    }
                    match ex.fetch_balances().await {
                        Ok(b) => {
                            let mut keys: Vec<_> =
                                b.iter().filter(|(_, v)| **v > Decimal::ZERO).collect();
                            keys.sort_by(|a, b| a.0.cmp(b.0));
                            println!("{id}: balances {:?}", keys);
                        }
                        Err(e) => {
                            problems += 1;
                            println!("{id}: balance fetch FAILED: {e}");
                        }
                    }
                }
            }
            // Fee reality check.
            let ids = cfg.enabled_exchanges();
            for i in 0..ids.len() {
                for j in 0..ids.len() {
                    if i == j {
                        continue;
                    }
                    let rt = cfg.fee_floor(ids[i]).taker_bps
                        + cfg.fee_floor(ids[j]).taker_bps
                        + cfg.strategy.slippage_buffer_bps
                        + cfg.strategy.min_edge_bps;
                    println!("buy {} / sell {}: gross spread must exceed {} bps ({}%) before a trade is sent", ids[i], ids[j], rt, rt / dec!(100));
                }
            }
            if !require {
                println!("note: live mode needs credentials on every enabled exchange");
            }
            if problems > 0 {
                bail!("{problems} problem(s) found");
            }
            println!("check ok");
            Ok(())
        }
        Cmd::Record { out, seconds } => {
            let book = Arc::new(BookCache::new());
            let exchanges = build_live_exchanges(&cfg, &Secrets::default(), false)?;
            let ledger = Ledger::memory();
            let engine = Engine::new(cfg, exchanges, book, ledger, "record");
            let (tx, mut rx) = mpsc::channel::<TopOfBook>(4096);
            let handles = engine.spawn_streams(tx);
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&out)
                .with_context(|| format!("opening {}", out.display()))?;
            let mut w = QuoteWriter::new(std::io::BufWriter::new(file));
            let mut shutdown = shutdown_signal();
            let deadline = if seconds > 0 {
                Some(tokio::time::Instant::now() + std::time::Duration::from_secs(seconds))
            } else {
                None
            };
            let mut status = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    _ = status.tick() => { tracing::info!("recorded {} quotes to {}", w.count, out.display()); w.flush()?; }
                    _ = async { match deadline { Some(d) => tokio::time::sleep_until(d).await, None => std::future::pending().await } } => break,
                    q = rx.recv() => { let Some(q) = q else { break }; w.write(&q)?; }
                }
            }
            w.flush()?;
            for h in handles {
                h.abort();
            }
            println!("recorded {} quotes to {}", w.count, out.display());
            Ok(())
        }
        Cmd::Replay { file } => {
            let quotes = read_quotes(&file)?;
            if quotes.is_empty() {
                bail!("no quotes in {}", file.display());
            }
            let n = quotes.len();
            let book = Arc::new(BookCache::new());
            let exchanges = build_paper_exchanges(&cfg, &book, false, Some(0))?;
            let mut cfg = cfg;
            // Staleness and wall-clock cooldowns are meaningless in replay
            // (quotes are historical and arrive as fast as they can be read).
            cfg.strategy.max_quote_age_ms = 10_000;
            cfg.risk.pair_cooldown_ms = 0;
            cfg.ledger_path = format!("{}.replay-ledger.jsonl", file.display());
            let ledger = Ledger::open(&PathBuf::from(&cfg.ledger_path))?;
            let mut engine = Engine::new(cfg, exchanges, book, ledger, "replay");
            engine.preflight().await?;
            let (tx, rx) = mpsc::channel::<TopOfBook>(1024);
            tokio::spawn(async move {
                for q in quotes {
                    match q.into_top_of_book() {
                        Ok(t) => {
                            if tx.send(t).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "skipping bad quote"),
                    }
                }
            });
            let (_stx, srx) = watch::channel(false);
            engine.run(rx, srx).await?;
            println!("replayed {n} quotes: {}", engine.ledger.summary());
            for (p, e) in &engine.best_net_edge {
                println!("{p}: best net edge seen {} bps", e.round_dp(2));
            }
            println!("ledger: {}", engine.cfg.ledger_path);
            Ok(())
        }
        Cmd::Scan {
            file,
            max_gap_ms,
            json,
        } => {
            let quotes = read_quotes(&file)?;
            let tiers = vec![
                speedcraft::scan::FeeTier::retail(),
                speedcraft::scan::FeeTier::volume(),
                speedcraft::scan::FeeTier::uniform("zero", 0.0, 0.0, 0.0),
            ];
            let stats = speedcraft::scan::scan(&quotes, &tiers, max_gap_ms);
            println!("{} quotes, {} pairs. Columns: venue spread (bps of mid), best directed gross cross (bps), % of samples crossed,", quotes.len(), stats.len());
            println!("then per fee tier (ret=retail, vol=volume tier, zer=zero fees): taker-taker arbitrage net p95/max, maker-mode net p95 and % of samples positive.");
            println!("Ranked by maker-mode net p95 at retail fees.");
            println!("CAUTION: maker-mode edge assumes a resting quote at the venue's best level gets hit. On a thin market that may rarely happen; paper-run maker mode to measure fills.\n");
            print!("{}", speedcraft::scan::render_table(&stats, &tiers));
            if let Some(j) = json {
                std::fs::write(&j, serde_json::to_string_pretty(&stats)?)?;
                println!("\nwrote {}", j.display());
            }
            Ok(())
        }
        Cmd::Report { file } => {
            let events = read_ledger(&file)?;
            let mut trades = 0;
            let mut ok = 0;
            let mut pnl = Decimal::ZERO;
            let mut opps = 0;
            let mut rejected = 0;
            let mut halts = Vec::new();
            for e in &events {
                match e {
                    LedgerEvent::Opportunity { .. } => opps += 1,
                    LedgerEvent::Rejected { .. } => rejected += 1,
                    LedgerEvent::Trade {
                        realised_pnl_quote,
                        success,
                        ..
                    } => {
                        trades += 1;
                        if *success {
                            ok += 1;
                        }
                        pnl += *realised_pnl_quote;
                    }
                    LedgerEvent::Halt { reason, .. } => halts.push(reason.clone()),
                    _ => {}
                }
            }
            println!("events: {}", events.len());
            println!("opportunities: {opps} (rejected by risk: {rejected})");
            println!("trades: {trades} (both legs filled: {ok})");
            println!("realised pnl: {}", pnl.round_dp(4));
            if !halts.is_empty() {
                println!("halts: {halts:?}");
            }
            Ok(())
        }
    }
}
