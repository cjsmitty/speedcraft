//! Configuration: a TOML file for everything non-secret, environment
//! variables for secrets. The bot refuses to start on an invalid config.

use crate::types::{ExchangeId, Fees, Pair};
use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Must be true (in addition to `--live` and the env var) to trade for real.
    #[serde(default)]
    pub live: bool,
    pub strategy: StrategyConfig,
    pub risk: RiskConfig,
    #[serde(default)]
    pub exchanges: HashMap<ExchangeId, ExchangeConfig>,
    pub pairs: Vec<PairConfig>,
    #[serde(default = "default_ledger_path")]
    pub ledger_path: String,
    #[serde(default = "default_stop_file")]
    pub stop_file: String,
    #[serde(default)]
    pub paper: PaperConfig,
    #[serde(default)]
    pub maker: Option<MakerConfig>,
}

/// Maker mode: rest post-only quotes on `quote_exchange`, hedge fills with
/// taker orders on `hedge_exchange`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MakerConfig {
    #[serde(default)]
    pub enabled: bool,
    pub quote_exchange: ExchangeId,
    pub hedge_exchange: ExchangeId,
    /// Required net edge per round trip after both fees, bps.
    pub target_edge_bps: Decimal,
    /// Size of each resting quote in quote currency.
    pub quote_notional: Decimal,
    /// Cap on net base inventory accumulated on the quote venue, in quote currency.
    pub max_inventory_notional: Decimal,
    /// Re-post when the desired price moves more than this, bps.
    #[serde(default = "default_requote_bps")]
    pub requote_bps: Decimal,
    /// Join the best level or improve it by at most this, bps.
    #[serde(default = "default_improve_bps")]
    pub max_improve_bps: Decimal,
    /// How often to poll a resting order's status, ms.
    #[serde(default = "default_fill_poll_ms")]
    pub fill_poll_ms: u64,
    /// Do not cancel/replace a side more often than this, ms (rate limits).
    #[serde(default = "default_min_requote_ms")]
    pub min_requote_ms: u64,
    /// Restrict to these pairs (default: all configured pairs on both venues).
    #[serde(default)]
    pub pairs: Option<Vec<String>>,
}

fn default_requote_bps() -> Decimal {
    dec!(1)
}
fn default_improve_bps() -> Decimal {
    dec!(1)
}
fn default_fill_poll_ms() -> u64 {
    500
}
fn default_min_requote_ms() -> u64 {
    1000
}

fn default_ledger_path() -> String {
    "ledger.jsonl".into()
}
fn default_stop_file() -> String {
    "STOP".into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrategyConfig {
    /// Minimum net edge after fees and buffer, in basis points.
    pub min_edge_bps: Decimal,
    /// Minimum net profit per trade in quote currency; filters out dust.
    pub min_profit_quote: Decimal,
    /// Extra safety margin subtracted from the edge, in bps, to cover
    /// slippage between quote and fill.
    pub slippage_buffer_bps: Decimal,
    /// Fraction of displayed top-of-book quantity we are willing to take.
    pub depth_fraction: Decimal,
    /// Ignore quotes older than this.
    pub max_quote_age_ms: u64,
    /// Maximum notional per trade leg in quote currency.
    pub max_notional_quote: Decimal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RiskConfig {
    /// Halt for the day once realised PnL drops below minus this.
    pub max_daily_loss_quote: Decimal,
    /// Halt after this many consecutive failed/unwound trades.
    pub max_consecutive_failures: u32,
    /// Per-exchange: disable an exchange after this many consecutive API errors.
    pub max_consecutive_errors: u32,
    /// Minimum milliseconds between trades on the same pair.
    pub pair_cooldown_ms: u64,
    /// Hard cap on trades per UTC day.
    pub max_trades_per_day: u32,
    /// Timeout for each order request.
    pub order_timeout_ms: u64,
    /// When unwinding a one-legged fill, cross the book by this many bps to
    /// guarantee the IOC fills.
    pub unwind_cross_bps: Decimal,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ExchangeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Floor for fees. The bot uses max(config, fetched-from-exchange).
    pub taker_fee_bps: Decimal,
    #[serde(default)]
    pub maker_fee_bps: Decimal,
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PairConfig {
    /// Canonical "BASE/QUOTE".
    pub pair: String,
    /// Exchange-native market symbols, e.g. coinbase = "BTC-USD".
    pub markets: HashMap<ExchangeId, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaperConfig {
    /// Starting quote balance per exchange in paper mode.
    pub start_quote: Decimal,
    /// Starting base balance per exchange per pair in paper mode.
    pub start_base: Decimal,
    /// Simulated adverse slippage applied to paper fills, in bps.
    pub sim_slippage_bps: Decimal,
    /// Simulated order latency in ms.
    pub sim_latency_ms: u64,
    /// Probability (0..1) that a paper leg does not fill at all, to exercise
    /// the unwind path.
    pub sim_miss_probability: f64,
}

impl Default for PaperConfig {
    fn default() -> Self {
        PaperConfig {
            start_quote: dec!(10000),
            start_base: dec!(0.2),
            sim_slippage_bps: dec!(1),
            sim_latency_ms: 50,
            sim_miss_probability: 0.0,
        }
    }
}

/// Secrets. Read from environment only; never from the config file.
#[derive(Clone, Debug, Default)]
pub struct Secrets {
    pub coinbase_key_name: Option<String>,
    pub coinbase_key_pem: Option<String>,
    pub kraken_key: Option<String>,
    pub kraken_secret: Option<String>,
    pub binance_key: Option<String>,
    pub binance_secret: Option<String>,
    pub binance_us_key: Option<String>,
    pub binance_us_secret: Option<String>,
}

impl Secrets {
    pub fn from_env() -> Secrets {
        let g = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Secrets {
            coinbase_key_name: g("COINBASE_API_KEY_NAME"),
            coinbase_key_pem: g("COINBASE_API_PRIVATE_KEY").map(|s| s.replace("\\n", "\n")),
            kraken_key: g("KRAKEN_API_KEY"),
            kraken_secret: g("KRAKEN_API_SECRET"),
            binance_key: g("BINANCE_API_KEY"),
            binance_secret: g("BINANCE_API_SECRET"),
            binance_us_key: g("BINANCE_US_API_KEY"),
            binance_us_secret: g("BINANCE_US_API_SECRET"),
        }
    }

    pub fn has(&self, ex: ExchangeId) -> bool {
        match ex {
            ExchangeId::Coinbase => {
                self.coinbase_key_name.is_some() && self.coinbase_key_pem.is_some()
            }
            ExchangeId::Kraken => self.kraken_key.is_some() && self.kraken_secret.is_some(),
            ExchangeId::Binance => self.binance_key.is_some() && self.binance_secret.is_some(),
            ExchangeId::BinanceUs => {
                self.binance_us_key.is_some() && self.binance_us_secret.is_some()
            }
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config = toml::from_str(&text).context("parsing config TOML")?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let s = &self.strategy;
        if s.min_edge_bps < Decimal::ZERO {
            bail!("strategy.min_edge_bps must be >= 0");
        }
        if s.depth_fraction <= Decimal::ZERO || s.depth_fraction > Decimal::ONE {
            bail!("strategy.depth_fraction must be in (0, 1]");
        }
        if s.max_notional_quote <= Decimal::ZERO {
            bail!("strategy.max_notional_quote must be > 0");
        }
        if s.max_quote_age_ms == 0 || s.max_quote_age_ms > 10_000 {
            bail!("strategy.max_quote_age_ms must be in 1..=10000");
        }
        let r = &self.risk;
        if r.max_daily_loss_quote <= Decimal::ZERO {
            bail!("risk.max_daily_loss_quote must be > 0");
        }
        if r.max_consecutive_failures == 0 || r.max_trades_per_day == 0 {
            bail!("risk limits must be > 0");
        }
        if r.order_timeout_ms < 500 || r.order_timeout_ms > 30_000 {
            bail!("risk.order_timeout_ms must be in 500..=30000");
        }
        if r.unwind_cross_bps < Decimal::ZERO || r.unwind_cross_bps > dec!(500) {
            bail!("risk.unwind_cross_bps must be in 0..=500");
        }
        if self.pairs.is_empty() {
            bail!("at least one pair is required");
        }
        let enabled: Vec<ExchangeId> = self.enabled_exchanges();
        if enabled.len() < 2 {
            bail!("at least two enabled exchanges are required for arbitrage");
        }
        for (ex, ec) in &self.exchanges {
            if ec.taker_fee_bps < Decimal::ZERO || ec.taker_fee_bps > dec!(500) {
                bail!("exchanges.{ex}.taker_fee_bps must be in 0..=500");
            }
        }
        for p in &self.pairs {
            Pair::parse(&p.pair).map_err(anyhow::Error::msg)?;
            let on: Vec<_> = p.markets.keys().filter(|e| enabled.contains(e)).collect();
            if on.len() < 2 {
                bail!(
                    "pair {} must be mapped on at least two enabled exchanges",
                    p.pair
                );
            }
            for (ex, m) in &p.markets {
                if m.trim().is_empty() {
                    bail!("pair {} has an empty market symbol for {ex}", p.pair);
                }
            }
        }
        if self.paper.sim_miss_probability < 0.0 || self.paper.sim_miss_probability > 1.0 {
            bail!("paper.sim_miss_probability must be in 0..=1");
        }
        if let Some(m) = &self.maker {
            if m.enabled {
                if m.quote_exchange == m.hedge_exchange {
                    bail!("maker.quote_exchange and maker.hedge_exchange must differ");
                }
                for e in [m.quote_exchange, m.hedge_exchange] {
                    if !enabled.contains(&e) {
                        bail!("maker: exchange {e} is not enabled");
                    }
                }
                if m.quote_notional <= Decimal::ZERO
                    || m.max_inventory_notional <= Decimal::ZERO
                    || m.target_edge_bps < Decimal::ZERO
                {
                    bail!("maker: quote_notional and max_inventory_notional must be > 0, target_edge_bps >= 0");
                }
                if m.fill_poll_ms < 100 {
                    bail!("maker.fill_poll_ms must be >= 100");
                }
                if let Some(ps) = &m.pairs {
                    for p in ps {
                        let pair = Pair::parse(p).map_err(anyhow::Error::msg)?;
                        if self.market_symbol(m.quote_exchange, &pair).is_none()
                            || self.market_symbol(m.hedge_exchange, &pair).is_none()
                        {
                            bail!("maker: pair {p} is not mapped on both maker exchanges");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn enabled_exchanges(&self) -> Vec<ExchangeId> {
        let mut v: Vec<ExchangeId> = self
            .exchanges
            .iter()
            .filter(|(_, c)| c.enabled)
            .map(|(e, _)| *e)
            .collect();
        v.sort();
        v
    }

    pub fn fee_floor(&self, ex: ExchangeId) -> Fees {
        match self.exchanges.get(&ex) {
            Some(c) => Fees {
                maker_bps: c.maker_fee_bps,
                taker_bps: c.taker_fee_bps,
            },
            None => Fees {
                maker_bps: dec!(100),
                taker_bps: dec!(100),
            },
        }
    }

    pub fn pairs(&self) -> Vec<Pair> {
        self.pairs
            .iter()
            .filter_map(|p| Pair::parse(&p.pair).ok())
            .collect()
    }

    pub fn market_symbol(&self, ex: ExchangeId, pair: &Pair) -> Option<String> {
        self.pairs
            .iter()
            .find(|p| Pair::parse(&p.pair).ok().as_ref() == Some(pair))
            .and_then(|p| p.markets.get(&ex).cloned())
    }

    /// Mapping from exchange-native symbol back to the canonical pair.
    pub fn symbol_to_pair(&self, ex: ExchangeId) -> HashMap<String, Pair> {
        let mut m = HashMap::new();
        for p in &self.pairs {
            if let (Ok(pair), Some(sym)) = (Pair::parse(&p.pair), p.markets.get(&ex)) {
                m.insert(sym.clone(), pair);
            }
        }
        m
    }
}

pub const EXAMPLE_TOML: &str = include_str!("../config/example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_is_valid_and_paper_by_default() {
        let cfg: Config = toml::from_str(EXAMPLE_TOML).unwrap();
        cfg.validate().unwrap();
        assert!(!cfg.live, "example config must not be live");
        assert!(cfg.enabled_exchanges().len() >= 2);
    }

    #[test]
    fn rejects_single_exchange() {
        let mut cfg: Config = toml::from_str(EXAMPLE_TOML).unwrap();
        let keep = cfg.enabled_exchanges()[0];
        for (e, c) in cfg.exchanges.iter_mut() {
            if *e != keep {
                c.enabled = false;
            }
        }
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_bad_depth_fraction() {
        let mut cfg: Config = toml::from_str(EXAMPLE_TOML).unwrap();
        cfg.strategy.depth_fraction = dec!(1.5);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn symbol_mapping_roundtrip() {
        let cfg: Config = toml::from_str(EXAMPLE_TOML).unwrap();
        let btc = Pair::parse("BTC/USD").unwrap();
        let sym = cfg.market_symbol(ExchangeId::Coinbase, &btc).unwrap();
        assert_eq!(sym, "BTC-USD");
        let back = cfg.symbol_to_pair(ExchangeId::Coinbase);
        assert_eq!(back.get("BTC-USD"), Some(&btc));
    }
}
