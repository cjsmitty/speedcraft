//! Building exchanges and running an engine, shared by the CLI, the local
//! UI and the headless agent.

use crate::book::BookCache;
use crate::config::{Config, Secrets};
use crate::engine::{Control, Engine, Shared};
use crate::exchanges::{
    binance::Binance, coinbase::Coinbase, kraken::Kraken, paper::PaperExchange, Exchange,
};
use crate::ledger::Ledger;
use crate::types::{ExchangeId, MarketRules, TopOfBook};
use anyhow::{bail, Context, Result};
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

pub const LIVE_ENV: &str = "SPEEDCRAFT_I_ACCEPT_LOSSES";
pub const LIVE_CONFIRM_PHRASE: &str = "I ACCEPT LOSSES";

pub fn live_env_set() -> bool {
    std::env::var(LIVE_ENV).map(|v| v == "yes").unwrap_or(false)
}

/// Build the real connectors (used directly in live mode, and as public data
/// sources inside paper exchanges).
pub fn build_live_exchanges(
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

pub fn default_rules() -> MarketRules {
    MarketRules {
        qty_step: dec!(0.00001),
        price_tick: dec!(0.01),
        min_qty: dec!(0.00001),
        min_notional: dec!(1),
    }
}

pub fn build_paper_exchanges(
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Paper,
    Live,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Paper => "paper",
            Mode::Live => "live",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "paper" => Some(Mode::Paper),
            "live" => Some(Mode::Live),
            _ => None,
        }
    }
}

/// The three independent live-mode gates. `cli_flag` is `--live` for the CLI
/// or the typed confirmation phrase for the UI.
pub fn check_live_gates(cfg: &Config, cli_flag: bool) -> Result<()> {
    let env_ok = live_env_set();
    if !(cli_flag && cfg.live && env_ok) {
        bail!(
            "refusing to trade live. All three are required: explicit confirmation ({cli_flag}), live = true in config ({}), {LIVE_ENV}=yes in the process environment ({env_ok}). Run paper and replay first; see docs/PLAN.md.",
            cfg.live
        );
    }
    Ok(())
}

/// Construct an engine for `mode`, with exchanges built accordingly.
pub fn build_engine(cfg: Config, mode: Mode) -> Result<Engine> {
    let book = Arc::new(BookCache::new());
    let exchanges = match mode {
        Mode::Paper => build_paper_exchanges(&cfg, &book, true, None)?,
        Mode::Live => build_live_exchanges(&cfg, &Secrets::from_env(), true)?,
    };
    let ledger = Ledger::open(&PathBuf::from(&cfg.ledger_path))?;
    Ok(Engine::new(cfg, exchanges, book, ledger, mode.as_str()))
}

/// Preflight, stream, run until shutdown. Used by the CLI and the supervisor.
pub async fn run_engine(
    mut engine: Engine,
    shutdown: watch::Receiver<bool>,
    shared: Option<Arc<Shared>>,
    control_rx: Option<mpsc::Receiver<Control>>,
) -> Result<()> {
    if let (Some(sh), Some(rx)) = (shared, control_rx) {
        engine.attach(sh, rx);
    }
    engine
        .preflight()
        .await
        .context("preflight failed; not starting")?;
    tracing::info!("preflight ok: {}", engine.status_line());
    let (tx, rx) = mpsc::channel::<TopOfBook>(4096);
    let handles = engine.spawn_streams(tx);
    let result = engine.run(rx, shutdown).await;
    for h in handles {
        h.abort();
    }
    result
}
