//! The event loop: quotes in, trades (rarely) out.

use crate::book::BookCache;
use crate::config::Config;
use crate::exchanges::{Exchange, ExchangeError};
use crate::executor::Executor;
use crate::ledger::{Ledger, LedgerEvent};
use crate::risk::RiskManager;
use crate::strategy::{find_best_opportunity, Balances, StrategyParams};
use crate::types::{now_ms, ExchangeId, Fees, MarketRules, Pair, TopOfBook};
use anyhow::{bail, Context, Result};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub struct Engine {
    pub cfg: Config,
    pub exchanges: HashMap<ExchangeId, Arc<dyn Exchange>>,
    pub book: Arc<BookCache>,
    pub fees: HashMap<ExchangeId, Fees>,
    pub rules: HashMap<(ExchangeId, Pair), MarketRules>,
    pub balances: Balances,
    /// Raw per-exchange asset balances, for multi-quote evaluation.
    pub per_pair_balances: HashMap<ExchangeId, crate::exchanges::Balances>,
    pub risk: RiskManager,
    pub ledger: Ledger,
    pub params: StrategyParams,
    /// Best net edge observed per pair, for the status report.
    pub best_net_edge: HashMap<Pair, Decimal>,
    pub quotes_seen: u64,
    pub mode: String,
}

impl Engine {
    pub fn new(
        cfg: Config,
        exchanges: HashMap<ExchangeId, Arc<dyn Exchange>>,
        book: Arc<BookCache>,
        ledger: Ledger,
        mode: &str,
    ) -> Engine {
        let params = StrategyParams {
            min_edge_bps: cfg.strategy.min_edge_bps,
            min_profit_quote: cfg.strategy.min_profit_quote,
            slippage_buffer_bps: cfg.strategy.slippage_buffer_bps,
            depth_fraction: cfg.strategy.depth_fraction,
            max_notional_quote: cfg.strategy.max_notional_quote,
        };
        let risk = RiskManager::new(cfg.risk.clone(), std::path::PathBuf::from(&cfg.stop_file));
        Engine {
            cfg,
            exchanges,
            book,
            fees: HashMap::new(),
            rules: HashMap::new(),
            balances: Balances::default(),
            per_pair_balances: HashMap::new(),
            risk,
            ledger,
            params,
            best_net_edge: HashMap::new(),
            quotes_seen: 0,
            mode: mode.to_string(),
        }
    }

    /// Fetch fees, rules and balances from every exchange. Refuses to run if
    /// any mapped market is invalid or any exchange is unreachable.
    pub async fn preflight(&mut self) -> Result<()> {
        let pairs = self.cfg.pairs();
        for (id, ex) in &self.exchanges {
            let fetched = ex
                .fetch_fees()
                .await
                .with_context(|| format!("{id}: fetching fees"))?;
            let floor = self.cfg.fee_floor(*id);
            let fees = Fees {
                maker_bps: fetched.maker_bps.max(floor.maker_bps),
                taker_bps: fetched.taker_bps.max(floor.taker_bps),
            };
            tracing::info!(exchange = %id, fetched_taker_bps = %fetched.taker_bps, floor_taker_bps = %floor.taker_bps, used_taker_bps = %fees.taker_bps, "fees");
            self.fees.insert(*id, fees);
            for p in &pairs {
                if self.cfg.market_symbol(*id, p).is_none() {
                    continue;
                }
                let r = ex
                    .fetch_market_rules(p)
                    .await
                    .with_context(|| format!("{id}: market rules for {p}"))?;
                if r.qty_step <= Decimal::ZERO || r.price_tick <= Decimal::ZERO {
                    bail!("{id}: nonsense market rules for {p}: {r:?}");
                }
                tracing::info!(exchange = %id, pair = %p, ?r, "market rules");
                self.rules.insert((*id, p.clone()), r);
            }
        }
        self.refresh_balances(None).await?;
        self.ledger.write(&LedgerEvent::Start {
            ts_ms: now_ms(),
            mode: self.mode.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
        });
        Ok(())
    }

    pub async fn refresh_balances(&mut self, only: Option<&[ExchangeId]>) -> Result<()> {
        let pairs = self.cfg.pairs();
        for (id, ex) in &self.exchanges {
            if let Some(o) = only {
                if !o.contains(id) {
                    continue;
                }
            }
            match ex.fetch_balances().await {
                Ok(b) => {
                    self.risk.record_exchange_ok(*id);
                    // One quote balance per exchange (first pair's quote asset) and
                    // base balances keyed by exchange. Multi-quote setups are
                    // handled per pair at evaluation time.
                    for p in &pairs {
                        let q = b.get(&p.quote).copied().unwrap_or(Decimal::ZERO);
                        let base = b.get(&p.base).copied().unwrap_or(Decimal::ZERO);
                        self.balances.quote.insert(*id, q);
                        self.balances.base.insert(*id, base);
                        tracing::debug!(exchange = %id, pair = %p, quote = %q, base = %base, "balance");
                    }
                    self.per_pair_balances.insert(*id, b);
                }
                Err(e) => {
                    self.risk.record_exchange_error(*id);
                    tracing::error!(exchange = %id, error = %e, "balance fetch failed");
                    if only.is_none() {
                        return Err(e.into());
                    }
                }
            }
        }
        Ok(())
    }

    fn balances_for(&self, pair: &Pair) -> Balances {
        let mut out = Balances::default();
        for (id, b) in &self.per_pair_balances {
            out.quote
                .insert(*id, b.get(&pair.quote).copied().unwrap_or(Decimal::ZERO));
            out.base
                .insert(*id, b.get(&pair.base).copied().unwrap_or(Decimal::ZERO));
        }
        out
    }

    /// Evaluate one pair; execute if the risk manager allows. Returns true
    /// if a trade was attempted.
    pub async fn evaluate_pair(&mut self, pair: &Pair) -> bool {
        let books = self.book.snapshot(
            pair,
            Duration::from_millis(self.cfg.strategy.max_quote_age_ms),
        );
        if books.len() < 2 {
            return false;
        }
        // Track how far from profitable we are, even when no trade happens.
        self.track_best_edge(pair, &books);
        let bal = self.balances_for(pair);
        let Some(op) = find_best_opportunity(&books, &self.fees, &self.rules, &bal, &self.params)
        else {
            return false;
        };
        self.ledger.write(&LedgerEvent::Opportunity {
            ts_ms: now_ms(),
            op: op.clone(),
        });
        if let Err(reason) = self.risk.check(&op) {
            tracing::info!(%reason, pair = %pair, "opportunity rejected by risk");
            self.ledger.write(&LedgerEvent::Rejected {
                ts_ms: now_ms(),
                op,
                reason: reason.to_string(),
            });
            return false;
        }
        tracing::info!(
            pair = %pair, buy = %op.buy_exchange, sell = %op.sell_exchange, qty = %op.qty,
            net_edge_bps = %op.net_edge_bps, expected = %op.expected_profit_quote, "EXECUTING"
        );
        let executor = Executor {
            risk: self.cfg.risk.clone(),
            slippage_buffer_bps: self.cfg.strategy.slippage_buffer_bps,
            exchanges: self.exchanges.clone(),
            rules: self.rules.clone(),
            book: self.book.clone(),
        };
        let out = executor.execute(&op).await;
        for leg in &out.legs {
            self.ledger.write(leg);
        }
        for ex in &out.exchange_errors {
            self.risk.record_exchange_error(*ex);
        }
        if out.exchange_errors.is_empty() {
            self.risk.record_exchange_ok(op.buy_exchange);
            self.risk.record_exchange_ok(op.sell_exchange);
        }
        self.ledger.write(&LedgerEvent::Trade {
            ts_ms: now_ms(),
            pair: pair.clone(),
            buy_exchange: op.buy_exchange,
            sell_exchange: op.sell_exchange,
            matched_qty: out.matched_qty,
            realised_pnl_quote: out.realised_pnl,
            expected_pnl_quote: op.expected_profit_quote,
            success: out.success,
            note: out.note.clone(),
        });
        tracing::info!(pnl = %out.realised_pnl, success = out.success, note = %out.note, "trade result");
        // "Nothing filled" is not a loss and not a failure streak event unless
        // it keeps happening; count it as a failure so a dead venue trips the breaker.
        self.risk
            .record_outcome(pair, out.realised_pnl, out.success);
        if let Some(h) = out.halt {
            self.risk.halt(h.clone());
            self.ledger.write(&LedgerEvent::Halt {
                ts_ms: now_ms(),
                reason: h,
            });
        }
        let touched = [op.buy_exchange, op.sell_exchange];
        if let Err(e) = self.refresh_balances(Some(&touched)).await {
            tracing::error!(error = %e, "balance refresh after trade failed");
        }
        true
    }

    fn track_best_edge(&mut self, pair: &Pair, books: &[TopOfBook]) {
        let mut best: Option<Decimal> = None;
        for b in books {
            for s in books {
                if b.exchange == s.exchange {
                    continue;
                }
                let (Some(bf), Some(sf)) = (self.fees.get(&b.exchange), self.fees.get(&s.exchange))
                else {
                    continue;
                };
                let gross = (s.bid - b.ask) / b.ask * Decimal::from(10_000);
                let net = gross - bf.taker_bps - sf.taker_bps - self.params.slippage_buffer_bps;
                best = Some(best.map_or(net, |x: Decimal| x.max(net)));
            }
        }
        if let Some(n) = best {
            let e = self.best_net_edge.entry(pair.clone()).or_insert(n);
            if n > *e {
                *e = n;
            }
        }
    }

    pub fn status_line(&self) -> String {
        let edges: Vec<String> = self
            .best_net_edge
            .iter()
            .map(|(p, e)| format!("{p}: best net edge {} bps", e.round_dp(2)))
            .collect();
        let snap = self.risk.snapshot();
        format!(
            "[{}] quotes={} books={} {} | pnl_today={} trades_today={} halted={:?} disabled={:?} | {}",
            self.mode,
            self.quotes_seen,
            self.book.len(),
            self.ledger.summary(),
            snap.realised_pnl_today.round_dp(4),
            snap.trades_today,
            snap.halted,
            snap.disabled_exchanges,
            if edges.is_empty() { "no cross-exchange quotes yet".to_string() } else { edges.join("; ") }
        )
    }

    /// Spawn a supervised market-data task per exchange.
    pub fn spawn_streams(&self, tx: mpsc::Sender<TopOfBook>) -> Vec<tokio::task::JoinHandle<()>> {
        let pairs = self.cfg.pairs();
        let mut handles = Vec::new();
        for (id, ex) in &self.exchanges {
            let my_pairs: Vec<Pair> = pairs
                .iter()
                .filter(|p| self.cfg.market_symbol(*id, p).is_some())
                .cloned()
                .collect();
            if my_pairs.is_empty() {
                continue;
            }
            let ex = ex.clone();
            let tx = tx.clone();
            let id = *id;
            handles.push(tokio::spawn(async move {
                let mut backoff = 1u64;
                loop {
                    let started = Instant::now();
                    match ex.stream_top_of_book(my_pairs.clone(), tx.clone()).await {
                        Ok(()) => tracing::warn!(exchange = %id, "stream ended; reconnecting"),
                        Err(ExchangeError::Network(m)) => tracing::warn!(exchange = %id, msg = %m, "stream network error; reconnecting"),
                        Err(e) => tracing::error!(exchange = %id, error = %e, "stream error; reconnecting"),
                    }
                    if started.elapsed() > Duration::from_secs(60) {
                        backoff = 1;
                    }
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }));
        }
        handles
    }

    /// Main loop. Returns when `rx` closes or a shutdown signal arrives.
    pub async fn run(
        &mut self,
        mut rx: mpsc::Receiver<TopOfBook>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let mut status = tokio::time::interval(Duration::from_secs(30));
        status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_balance_refresh = Instant::now();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    tracing::info!("shutdown requested");
                    break;
                }
                _ = status.tick() => {
                    tracing::info!("{}", self.status_line());
                    if last_balance_refresh.elapsed() > Duration::from_secs(300) {
                        let _ = self.refresh_balances(Some(&self.exchange_ids())).await;
                        last_balance_refresh = Instant::now();
                    }
                    if self.risk.stop_file_present() {
                        tracing::warn!("STOP file present: trading paused (remove it to resume)");
                    }
                }
                q = rx.recv() => {
                    let Some(q) = q else { break };
                    self.quotes_seen += 1;
                    let pair = q.pair.clone();
                    self.book.update(q);
                    self.evaluate_pair(&pair).await;
                }
            }
        }
        self.ledger.write(&LedgerEvent::Stop { ts_ms: now_ms() });
        tracing::info!("{}", self.status_line());
        Ok(())
    }

    fn exchange_ids(&self) -> Vec<ExchangeId> {
        self.exchanges.keys().copied().collect()
    }
}
