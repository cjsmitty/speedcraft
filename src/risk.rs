//! Risk manager: the gatekeeper between "we found an opportunity" and "we
//! send an order". It is a plain state machine so it is easy to test.

use crate::config::RiskConfig;
use crate::strategy::Opportunity;
use crate::types::{ExchangeId, Pair};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reject {
    Halted(String),
    StopFile,
    ExchangeDisabled(ExchangeId),
    PairCooldown,
    PairInFlight,
    DailyTradeCap,
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reject::Halted(r) => write!(f, "halted: {r}"),
            Reject::StopFile => write!(f, "STOP file present"),
            Reject::ExchangeDisabled(e) => write!(f, "exchange {e} disabled by circuit breaker"),
            Reject::PairCooldown => write!(f, "pair in cooldown"),
            Reject::PairInFlight => write!(f, "pair already has a trade in flight"),
            Reject::DailyTradeCap => write!(f, "daily trade cap reached"),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct RiskSnapshot {
    pub halted: Option<String>,
    pub day: String,
    pub realised_pnl_today: Decimal,
    pub trades_today: u32,
    pub consecutive_failures: u32,
    pub disabled_exchanges: Vec<ExchangeId>,
}

pub struct RiskManager {
    cfg: RiskConfig,
    halted: Option<String>,
    day: String,
    realised_pnl_today: Decimal,
    trades_today: u32,
    consecutive_failures: u32,
    last_trade: HashMap<Pair, Instant>,
    in_flight: HashMap<Pair, bool>,
    exchange_errors: HashMap<ExchangeId, u32>,
    disabled: Vec<ExchangeId>,
    stop_file: PathBuf,
}

fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

impl RiskManager {
    pub fn new(cfg: RiskConfig, stop_file: PathBuf) -> Self {
        RiskManager {
            cfg,
            halted: None,
            day: today_utc(),
            realised_pnl_today: Decimal::ZERO,
            trades_today: 0,
            consecutive_failures: 0,
            last_trade: HashMap::new(),
            in_flight: HashMap::new(),
            exchange_errors: HashMap::new(),
            disabled: Vec::new(),
            stop_file,
        }
    }

    fn roll_day(&mut self) {
        let t = today_utc();
        if t != self.day {
            self.day = t;
            self.realised_pnl_today = Decimal::ZERO;
            self.trades_today = 0;
            // A daily-loss halt clears at the UTC day boundary. A failure
            // halt does not: it needs a human to look.
            if matches!(&self.halted, Some(r) if r.starts_with("daily loss")) {
                self.halted = None;
            }
        }
    }

    pub fn is_halted(&self) -> Option<&str> {
        self.halted.as_deref()
    }

    pub fn halt(&mut self, reason: impl Into<String>) {
        let r = reason.into();
        tracing::error!(reason = %r, "RISK HALT");
        self.halted = Some(r);
    }

    pub fn stop_file_present(&self) -> bool {
        self.stop_file.exists()
    }

    /// Decide whether `op` may be executed. On Ok the pair is marked in-flight
    /// and the caller must call `record_outcome` exactly once.
    pub fn check(&mut self, op: &Opportunity) -> Result<(), Reject> {
        self.roll_day();
        if let Some(r) = &self.halted {
            return Err(Reject::Halted(r.clone()));
        }
        if self.stop_file_present() {
            return Err(Reject::StopFile);
        }
        for ex in [op.buy_exchange, op.sell_exchange] {
            if self.disabled.contains(&ex) {
                return Err(Reject::ExchangeDisabled(ex));
            }
        }
        if self.trades_today >= self.cfg.max_trades_per_day {
            return Err(Reject::DailyTradeCap);
        }
        if self.in_flight.get(&op.pair).copied().unwrap_or(false) {
            return Err(Reject::PairInFlight);
        }
        if let Some(t) = self.last_trade.get(&op.pair) {
            if t.elapsed() < Duration::from_millis(self.cfg.pair_cooldown_ms) {
                return Err(Reject::PairCooldown);
            }
        }
        self.in_flight.insert(op.pair.clone(), true);
        Ok(())
    }

    /// Record the realised result of a trade attempt that passed `check`.
    /// `success` means both legs matched as intended (no unwind, no unknown).
    pub fn record_outcome(&mut self, pair: &Pair, realised_pnl: Decimal, success: bool) {
        self.roll_day();
        self.in_flight.insert(pair.clone(), false);
        self.last_trade.insert(pair.clone(), Instant::now());
        self.trades_today += 1;
        self.realised_pnl_today += realised_pnl;
        if success {
            self.consecutive_failures = 0;
        } else {
            self.consecutive_failures += 1;
            if self.consecutive_failures >= self.cfg.max_consecutive_failures {
                self.halt(format!(
                    "{} consecutive failed trades",
                    self.consecutive_failures
                ));
            }
        }
        if self.realised_pnl_today <= -self.cfg.max_daily_loss_quote {
            self.halt(format!("daily loss limit hit: {}", self.realised_pnl_today));
        }
    }

    /// An exchange call errored (network, 5xx, auth...). Enough in a row
    /// disables that exchange until restart.
    pub fn record_exchange_error(&mut self, ex: ExchangeId) {
        let n = self.exchange_errors.entry(ex).or_insert(0);
        *n += 1;
        if *n >= self.cfg.max_consecutive_errors && !self.disabled.contains(&ex) {
            tracing::error!(exchange = %ex, "circuit breaker: exchange disabled");
            self.disabled.push(ex);
        }
    }

    pub fn record_exchange_ok(&mut self, ex: ExchangeId) {
        self.exchange_errors.insert(ex, 0);
    }

    pub fn is_exchange_disabled(&self, ex: ExchangeId) -> bool {
        self.disabled.contains(&ex)
    }

    pub fn snapshot(&self) -> RiskSnapshot {
        RiskSnapshot {
            halted: self.halted.clone(),
            day: self.day.clone(),
            realised_pnl_today: self.realised_pnl_today,
            trades_today: self.trades_today,
            consecutive_failures: self.consecutive_failures,
            disabled_exchanges: self.disabled.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn cfg() -> RiskConfig {
        RiskConfig {
            max_daily_loss_quote: dec!(25),
            max_consecutive_failures: 3,
            max_consecutive_errors: 2,
            pair_cooldown_ms: 50,
            max_trades_per_day: 100,
            order_timeout_ms: 1000,
            unwind_cross_bps: dec!(20),
        }
    }
    fn op() -> Opportunity {
        Opportunity {
            pair: Pair::parse("BTC/USD").unwrap(),
            buy_exchange: ExchangeId::Coinbase,
            sell_exchange: ExchangeId::Kraken,
            buy_price: dec!(100),
            sell_price: dec!(101),
            qty: dec!(1),
            gross_edge_bps: dec!(100),
            net_edge_bps: dec!(80),
            expected_profit_quote: dec!(0.8),
            buy_fee_quote: dec!(0.1),
            sell_fee_quote: dec!(0.1),
        }
    }
    fn rm() -> RiskManager {
        RiskManager::new(cfg(), PathBuf::from("/nonexistent/STOP-file-for-tests"))
    }

    #[test]
    fn in_flight_blocks_second_trade_until_recorded() {
        let mut r = rm();
        r.check(&op()).unwrap();
        assert_eq!(r.check(&op()), Err(Reject::PairInFlight));
        r.record_outcome(&op().pair, dec!(0.5), true);
        assert_eq!(r.check(&op()), Err(Reject::PairCooldown));
        std::thread::sleep(Duration::from_millis(60));
        r.record_outcome(&op().pair, dec!(0), true); // clear in-flight from check above
        std::thread::sleep(Duration::from_millis(60));
        assert!(r.check(&op()).is_ok());
    }

    #[test]
    fn daily_loss_halts() {
        let mut r = rm();
        r.check(&op()).unwrap();
        r.record_outcome(&op().pair, dec!(-10), true);
        assert!(r.is_halted().is_none());
        std::thread::sleep(Duration::from_millis(60));
        r.check(&op()).unwrap();
        r.record_outcome(&op().pair, dec!(-15), true);
        assert!(r.is_halted().unwrap().starts_with("daily loss"));
        assert!(matches!(r.check(&op()), Err(Reject::Halted(_))));
    }

    #[test]
    fn consecutive_failures_halt_and_success_resets() {
        let mut r = rm();
        for _ in 0..2 {
            r.check(&op()).unwrap();
            r.record_outcome(&op().pair, dec!(-0.1), false);
            std::thread::sleep(Duration::from_millis(60));
        }
        r.check(&op()).unwrap();
        r.record_outcome(&op().pair, dec!(0.1), true);
        assert_eq!(r.snapshot().consecutive_failures, 0);
        std::thread::sleep(Duration::from_millis(60));
        for _ in 0..3 {
            if r.check(&op()).is_ok() {
                r.record_outcome(&op().pair, dec!(-0.1), false);
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        assert!(r.is_halted().unwrap().contains("consecutive"));
    }

    #[test]
    fn daily_cap() {
        let mut c = cfg();
        c.max_trades_per_day = 5;
        let mut r = RiskManager::new(c, PathBuf::from("/nonexistent/STOP-file-for-tests"));
        for _ in 0..5 {
            r.check(&op()).unwrap();
            r.record_outcome(&op().pair, dec!(0.1), true);
            std::thread::sleep(Duration::from_millis(60));
        }
        assert_eq!(r.check(&op()), Err(Reject::DailyTradeCap));
    }

    #[test]
    fn circuit_breaker_disables_exchange() {
        let mut r = rm();
        r.record_exchange_error(ExchangeId::Kraken);
        assert!(!r.is_exchange_disabled(ExchangeId::Kraken));
        r.record_exchange_error(ExchangeId::Kraken);
        assert!(r.is_exchange_disabled(ExchangeId::Kraken));
        assert_eq!(
            r.check(&op()),
            Err(Reject::ExchangeDisabled(ExchangeId::Kraken))
        );
    }

    #[test]
    fn stop_file_blocks() {
        let dir = std::env::temp_dir().join(format!("speedcraft-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stop = dir.join("STOP");
        let mut r = RiskManager::new(cfg(), stop.clone());
        assert!(r.check(&op()).is_ok());
        r.record_outcome(&op().pair, dec!(0), true);
        std::fs::write(&stop, "").unwrap();
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(r.check(&op()), Err(Reject::StopFile));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
