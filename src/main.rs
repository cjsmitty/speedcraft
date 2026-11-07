use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use speedcraft::book::BookCache;
use speedcraft::config::{Config, Secrets, EXAMPLE_TOML};
use speedcraft::engine::Engine;
use speedcraft::exchanges::{
    binance::Binance, coinbase::Coinbase, kraken::Kraken, paper::PaperExchange, Exchange,
};
use speedcraft::ledger::{read_ledger, Ledger, LedgerEvent};
use speedcraft::replay::{read_quotes, QuoteWriter};
use speedcraft::types::{ExchangeId, MarketRules, TopOfBook};
use std::collections::HashMap;
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
    /// Summarise a ledger file.
    Report {
        #[arg(default_value = "ledger.jsonl")]
        file: PathBuf,
    },
    /// Print the example config.
    ExampleConfig,
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,speedcraft=info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

/// Build the real connectors (used directly in live mode, and as public data
/// sources inside paper exchanges).
fn build_live_exchanges(
    cfg: &Config,
    secrets: &Secrets,
    require_keys: bool,
) -> Result<HashMap<ExchangeId, Arc<dyn Exchange>>> {
    let mut out: HashMap<ExchangeId, Arc<dyn Exchange>> = HashMap::new();
    for id in cfg.enabled_exchanges() {
        let markets = cfg.symbol_to_pair(id);
        if require_keys && !secrets.has(id) {
            bail!("live mode: missing API credentials for {id} (see README for the env var names)");
        }
        let ex: Arc<dyn Exchange> = match id {
            ExchangeId::Coinbase => Arc::new(Coinbase::new(
                secrets.coinbase_key_name.clone(),
                secrets.coinbase_key_pem.clone(),
                markets,
            )?),
            ExchangeId::Kraken => Arc::new(Kraken::new(
                secrets.kraken_key.clone(),
                secrets.kraken_secret.clone(),
                markets,
            )),
            ExchangeId::Binance => Arc::new(Binance::new(
                id,
                secrets.binance_key.clone(),
                secrets.binance_secret.clone(),
                markets,
            )),
            ExchangeId::BinanceUs => Arc::new(Binance::new(
                id,
                secrets.binance_us_key.clone(),
                secrets.binance_us_secret.clone(),
                markets,
            )),
        };
        out.insert(id, ex);
    }
    Ok(out)
}

fn default_rules() -> MarketRules {
    MarketRules {
        qty_step: dec!(0.00001),
        price_tick: dec!(0.01),
        min_qty: dec!(0.00001),
        min_notional: dec!(1),
    }
}

fn build_paper_exchanges(
    cfg: &Config,
    book: &Arc<BookCache>,
    with_live_data: bool,
    latency_override: Option<u64>,
) -> Result<HashMap<ExchangeId, Arc<dyn Exchange>>> {
    let inner = if with_live_data {
        build_live_exchanges(cfg, &Secrets::default(), false)?
    } else {
        HashMap::new()
    };
    let pairs = cfg.pairs();
    let mut out: HashMap<ExchangeId, Arc<dyn Exchange>> = HashMap::new();
    let mut pcfg = cfg.paper.clone();
    if let Some(l) = latency_override {
        pcfg.sim_latency_ms = l;
    }
    for id in cfg.enabled_exchanges() {
        let my_pairs: Vec<_> = pairs
            .iter()
            .filter(|p| cfg.market_symbol(id, p).is_some())
            .cloned()
            .collect();
        let ex = PaperExchange::new(
            id,
            inner.get(&id).cloned(),
            book.clone(),
            cfg.fee_floor(id),
            pcfg.clone(),
            &my_pairs,
            default_rules(),
        );
        out.insert(id, Arc::new(ex));
    }
    Ok(out)
}

fn shutdown_signal() -> watch::Receiver<bool> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = tx.send(true);
    });
    rx
}

async fn run_engine(
    cfg: Config,
    exchanges: HashMap<ExchangeId, Arc<dyn Exchange>>,
    book: Arc<BookCache>,
    mode: &str,
) -> Result<()> {
    let ledger = Ledger::open(&PathBuf::from(&cfg.ledger_path))?;
    let mut engine = Engine::new(cfg, exchanges, book, ledger, mode);
    engine
        .preflight()
        .await
        .context("preflight failed; not starting")?;
    tracing::info!("preflight ok: {}", engine.status_line());
    let (tx, rx) = mpsc::channel::<TopOfBook>(4096);
    let handles = engine.spawn_streams(tx);
    let shutdown = shutdown_signal();
    engine.run(rx, shutdown).await?;
    for h in handles {
        h.abort();
    }
    Ok(())
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
            let book = Arc::new(BookCache::new());
            let exchanges = build_paper_exchanges(&cfg, &book, true, None)?;
            run_engine(cfg, exchanges, book, "paper").await
        }
        Cmd::Live { live } => {
            let env_ok = std::env::var("SPEEDCRAFT_I_ACCEPT_LOSSES")
                .map(|v| v == "yes")
                .unwrap_or(false);
            if !(live && cfg.live && env_ok) {
                bail!(
                    "refusing to trade live. All three are required: --live flag ({live}), live = true in config ({}), SPEEDCRAFT_I_ACCEPT_LOSSES=yes ({env_ok}). \
                     Run `paper` and `replay` first; see docs/PLAN.md.",
                    cfg.live
                );
            }
            tracing::error!(
                "LIVE MODE: real orders will be sent. Max daily loss {} {}",
                cfg.risk.max_daily_loss_quote,
                "quote"
            );
            let secrets = Secrets::from_env();
            let exchanges = build_live_exchanges(&cfg, &secrets, true)?;
            let book = Arc::new(BookCache::new());
            run_engine(cfg, exchanges, book, "live").await
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
