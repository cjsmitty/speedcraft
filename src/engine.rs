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
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Operator commands delivered to a running engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Control {
    Halt { reason: String },
    Resume,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BookView {
    pub exchange: ExchangeId,
    pub pair: String,
    pub bid: Decimal,
    pub bid_qty: Decimal,
    pub ask: Decimal,
    pub ask_qty: Decimal,
    pub age_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EdgeView {
    pub pair: String,
    pub buy_exchange: ExchangeId,
    pub sell_exchange: ExchangeId,
    pub gross_bps: Decimal,
    pub net_bps: Decimal,
    /// Both quotes are within the trading freshness window.
    pub fresh: bool,
    /// Fresh and net edge at or above the configured minimum.
    pub tradable: bool,
}

/// Everything a UI needs, published by the engine about once a second.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub mode: String,
    pub started_ms: i64,
    pub updated_ms: i64,
    pub quotes_seen: u64,
    pub quotes_per_sec: f64,
    pub opportunities: u32,
    pub trades: u32,
    pub successes: u32,
    pub realised_pnl: Decimal,
    pub pnl_today: Decimal,
    pub trades_today: u32,
    pub consecutive_failures: u32,
    pub halted: Option<String>,
    pub stop_file_present: bool,
    pub disabled_exchanges: Vec<ExchangeId>,
    pub books: Vec<BookView>,
    pub edges: Vec<EdgeView>,
    pub best_net_edge: HashMap<String, Decimal>,
    pub balances: HashMap<ExchangeId, HashMap<String, Decimal>>,
    pub fees_taker_bps: HashMap<ExchangeId, Decimal>,
    pub min_edge_bps: Decimal,
    /// Maker-mode resting quotes and inventory per pair (empty when disabled).
    #[serde(default)]
    pub maker: Vec<crate::maker::MakerView>,
    /// Futures basis carry quotes and paper positions.
    #[serde(default)]
    pub carry: crate::carry::CarryState,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StampedEvent {
    pub seq: u64,
    pub ts_ms: i64,
    #[serde(flatten)]
    pub event: LedgerEvent,
}

/// Fixed-size ring of recent ledger events with monotonically increasing
/// sequence numbers, so a UI can poll for "everything after N".
pub struct EventRing {
    buf: std::collections::VecDeque<StampedEvent>,
    cap: usize,
    next_seq: u64,
}

impl EventRing {
    pub fn new(cap: usize) -> Self {
        EventRing {
            buf: std::collections::VecDeque::with_capacity(cap),
            cap,
            next_seq: 1,
        }
    }
    pub fn push(&mut self, event: LedgerEvent) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.buf.len() == self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(StampedEvent {
            seq,
            ts_ms: now_ms(),
            event,
        });
        seq
    }
    pub fn since(&self, seq: u64, limit: usize) -> Vec<StampedEvent> {
        self.buf
            .iter()
            .filter(|e| e.seq > seq)
            .take(limit)
            .cloned()
            .collect()
    }
    pub fn latest_seq(&self) -> u64 {
        self.next_seq - 1
    }
}

/// State shared between a running engine and whatever supervises it.
/// An in-progress quote recording, toggled from the UI while an engine runs.
pub struct Recorder {
    pub path: std::path::PathBuf,
    pub started_ms: i64,
    pub count: u64,
    writer: crate::replay::QuoteWriter<std::io::BufWriter<std::fs::File>>,
}

impl Recorder {
    pub fn open(path: std::path::PathBuf) -> std::io::Result<Recorder> {
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        Ok(Recorder {
            path,
            started_ms: now_ms(),
            count: 0,
            writer: crate::replay::QuoteWriter::new(std::io::BufWriter::new(f)),
        })
    }
    pub fn write(&mut self, q: &TopOfBook) {
        if self.writer.write(q).is_ok() {
            self.count += 1;
        }
        if self.count % 500 == 0 {
            let _ = self.writer.flush();
        }
    }
    pub fn flush(&mut self) {
        let _ = self.writer.flush();
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RecorderStatus {
    pub active: bool,
    pub path: Option<String>,
    pub started_ms: i64,
    pub count: u64,
}

pub struct Shared {
    pub status: RwLock<StatusSnapshot>,
    pub events: Mutex<EventRing>,
    pub recorder: Mutex<Option<Recorder>>,
}

impl Shared {
    pub fn new() -> Arc<Shared> {
        Arc::new(Shared {
            status: RwLock::new(StatusSnapshot::default()),
            events: Mutex::new(EventRing::new(5000)),
            recorder: Mutex::new(None),
        })
    }
    pub fn recorder_status(&self) -> RecorderStatus {
        match self.recorder.lock().unwrap().as_ref() {
            Some(r) => RecorderStatus {
                active: true,
                path: Some(r.path.display().to_string()),
                started_ms: r.started_ms,
                count: r.count,
            },
            None => RecorderStatus::default(),
        }
    }
}

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
    pub shared: Option<Arc<Shared>>,
    pub control_rx: Option<mpsc::Receiver<Control>>,
    pub maker: Option<crate::maker::MakerManager>,
    pub carry: Option<crate::carry::SharedCarry>,
    started_ms: i64,
    last_publish: Instant,
    quotes_at_last_publish: u64,
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
        let maker = match &cfg.maker {
            Some(m) if m.enabled => match (
                exchanges.get(&m.quote_exchange),
                exchanges.get(&m.hedge_exchange),
            ) {
                (Some(q), Some(h)) => {
                    tracing::info!(quote = %m.quote_exchange, hedge = %m.hedge_exchange, edge_bps = %m.target_edge_bps, "maker mode enabled");
                    Some(crate::maker::MakerManager::new(
                        m.clone(),
                        cfg.strategy.slippage_buffer_bps,
                        q.clone(),
                        h.clone(),
                    ))
                }
                _ => {
                    tracing::error!(
                        "maker mode configured but its exchanges are not available; disabled"
                    );
                    None
                }
            },
            _ => None,
        };
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
            shared: None,
            control_rx: None,
            maker,
            carry: None,
            started_ms: now_ms(),
            last_publish: Instant::now(),
            quotes_at_last_publish: 0,
        }
    }

    /// Attach shared state and a control channel for a supervising UI/agent.
    pub fn attach(&mut self, shared: Arc<Shared>, control_rx: mpsc::Receiver<Control>) {
        self.shared = Some(shared);
        self.control_rx = Some(control_rx);
    }

    /// Write a ledger event to disk and to the shared event ring.
    pub fn emit(&mut self, ev: LedgerEvent) {
        self.ledger.write(&ev);
        // Quote place/cancel events are high-volume; they go to the ledger file
        // for audit but not to the UI ring (the UI shows live maker state).
        if matches!(ev, LedgerEvent::Quote { .. }) {
            return;
        }
        if let Some(sh) = &self.shared {
            sh.events.lock().unwrap().push(ev);
        }
    }

    pub fn snapshot(&self) -> StatusSnapshot {
        let risk = self.risk.snapshot();
        let max_age = Duration::from_millis(self.cfg.strategy.max_quote_age_ms);
        let mut books = Vec::new();
        let mut edges = Vec::new();
        for pair in self.cfg.pairs() {
            let fresh = self.book.snapshot(&pair, Duration::from_secs(3600));
            for b in &fresh {
                books.push(BookView {
                    exchange: b.exchange,
                    pair: pair.to_string(),
                    bid: b.bid,
                    bid_qty: b.bid_qty,
                    ask: b.ask,
                    ask_qty: b.ask_qty,
                    age_ms: b.received_at.elapsed().as_millis() as u64,
                });
            }
            // Display edges over a generous window; trade only on fresh ones.
            let shown: Vec<&TopOfBook> = fresh
                .iter()
                .filter(|b| b.received_at.elapsed() <= Duration::from_secs(15))
                .collect();
            for b in &shown {
                for sl in &shown {
                    if b.exchange == sl.exchange {
                        continue;
                    }
                    let (Some(bf), Some(sf)) =
                        (self.fees.get(&b.exchange), self.fees.get(&sl.exchange))
                    else {
                        continue;
                    };
                    let gross = (sl.bid - b.ask) / b.ask * Decimal::from(10_000);
                    let net = gross - bf.taker_bps - sf.taker_bps - self.params.slippage_buffer_bps;
                    let is_fresh =
                        b.received_at.elapsed() <= max_age && sl.received_at.elapsed() <= max_age;
                    edges.push(EdgeView {
                        pair: pair.to_string(),
                        buy_exchange: b.exchange,
                        sell_exchange: sl.exchange,
                        gross_bps: gross.round_dp(2),
                        net_bps: net.round_dp(2),
                        fresh: is_fresh,
                        tradable: is_fresh && net >= self.params.min_edge_bps,
                    });
                }
            }
        }
        books.sort_by_key(|b| (b.pair.clone(), b.exchange));
        edges.sort_by_key(|e| std::cmp::Reverse(e.net_bps));
        let elapsed = self.last_publish.elapsed().as_secs_f64().max(0.001);
        StatusSnapshot {
            mode: self.mode.clone(),
            started_ms: self.started_ms,
            updated_ms: now_ms(),
            quotes_seen: self.quotes_seen,
            quotes_per_sec: ((self.quotes_seen - self.quotes_at_last_publish) as f64 / elapsed
                * 10.0)
                .round()
                / 10.0,
            opportunities: self.ledger.opportunities,
            trades: self.ledger.trades,
            successes: self.ledger.successes,
            realised_pnl: self.ledger.realised_pnl,
            pnl_today: risk.realised_pnl_today,
            trades_today: risk.trades_today,
            consecutive_failures: risk.consecutive_failures,
            halted: risk.halted,
            stop_file_present: self.risk.stop_file_present(),
            disabled_exchanges: risk.disabled_exchanges,
            books,
            edges,
            best_net_edge: self
                .best_net_edge
                .iter()
                .map(|(p, e)| (p.to_string(), e.round_dp(2)))
                .collect(),
            balances: self.per_pair_balances.clone(),
            fees_taker_bps: self.fees.iter().map(|(e, f)| (*e, f.taker_bps)).collect(),
            min_edge_bps: self.params.min_edge_bps,
            maker: self.maker.as_ref().map(|m| m.views()).unwrap_or_default(),
            carry: self
                .carry
                .as_ref()
                .map(|c| c.lock().unwrap().clone())
                .unwrap_or_default(),
        }
    }

    fn publish(&mut self) {
        let snap = self.snapshot();
        self.last_publish = Instant::now();
        self.quotes_at_last_publish = self.quotes_seen;
        if let Some(sh) = &self.shared {
            *sh.status.write().unwrap() = snap;
        }
    }

    async fn apply_control(&mut self, c: Control) {
        match c {
            Control::Halt { reason } => {
                self.risk.halt(reason.clone());
                self.emit(LedgerEvent::Halt {
                    ts_ms: now_ms(),
                    reason: format!("operator: {reason}"),
                });
            }
            Control::Resume => {
                self.risk.resume();
                self.emit(LedgerEvent::Resume { ts_ms: now_ms() });
            }
        }
        self.publish();
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
        self.emit(LedgerEvent::Start {
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
        let traded = self.evaluate_arb(pair).await;
        if self.maker.is_some() {
            self.maker_tick(pair).await;
        }
        traded
    }

    /// Maker mode pass for one pair: fills are hedged and accounted here.
    async fn maker_tick(&mut self, pair: &Pair) {
        let Some(mut maker) = self.maker.take() else {
            return;
        };
        if !maker.handles(pair) {
            self.maker = Some(maker);
            return;
        }
        let allowed = self.risk.is_halted().is_none()
            && !self.risk.stop_file_present()
            && !self.risk.is_exchange_disabled(maker.cfg.quote_exchange)
            && !self.risk.is_exchange_disabled(maker.cfg.hedge_exchange);
        let get = |ex: ExchangeId, asset: &str| {
            self.per_pair_balances
                .get(&ex)
                .and_then(|b| b.get(asset))
                .copied()
                .unwrap_or(Decimal::ZERO)
        };
        let balances = crate::strategy::maker::MakerBalances {
            quote_venue_quote: get(maker.cfg.quote_exchange, &pair.quote),
            quote_venue_base: get(maker.cfg.quote_exchange, &pair.base),
            hedge_venue_quote: get(maker.cfg.hedge_exchange, &pair.quote),
            hedge_venue_base: get(maker.cfg.hedge_exchange, &pair.base),
        };
        let max_age = Duration::from_millis(self.cfg.strategy.max_quote_age_ms);
        let (events, fills) = maker
            .tick(
                pair,
                &self.book,
                &self.fees,
                &self.rules,
                balances,
                max_age,
                allowed,
                &self.cfg.risk,
            )
            .await;
        let quote_ex = maker.cfg.quote_exchange;
        let hedge_ex = maker.cfg.hedge_exchange;
        self.maker = Some(maker);
        for ev in events {
            self.emit(ev);
        }
        for f in fills {
            tracing::info!(pair = %pair, side = %f.side, qty = %f.qty, price = %f.price, pnl = %f.outcome.realised_pnl, success = f.outcome.success, "MAKER FILL hedged");
            for ex in &f.outcome.exchange_errors {
                self.risk.record_exchange_error(*ex);
            }
            self.emit(LedgerEvent::Trade {
                ts_ms: now_ms(),
                pair: pair.clone(),
                buy_exchange: if f.side == crate::types::Side::Buy {
                    quote_ex
                } else {
                    hedge_ex
                },
                sell_exchange: if f.side == crate::types::Side::Buy {
                    hedge_ex
                } else {
                    quote_ex
                },
                matched_qty: f.outcome.matched_qty,
                realised_pnl_quote: f.outcome.realised_pnl,
                expected_pnl_quote: Decimal::ZERO,
                success: f.outcome.success,
                note: format!("maker: {}", f.outcome.note),
            });
            // Maker fills bypass the arb in-flight lock; mark it so cooldown applies.
            let _ = self.risk.check_maker_slot(pair);
            self.risk
                .record_outcome(pair, f.outcome.realised_pnl, f.outcome.success);
            if let Some(h) = f.outcome.halt {
                self.risk.halt(h.clone());
                self.emit(LedgerEvent::Halt {
                    ts_ms: now_ms(),
                    reason: h,
                });
            }
            if let Err(e) = self.refresh_balances(Some(&[quote_ex, hedge_ex])).await {
                tracing::error!(error = %e, "balance refresh after maker fill failed");
            }
        }
    }

    /// Cancel all maker quotes (shutdown or halt).
    pub async fn maker_cancel_all(&mut self) {
        let Some(mut maker) = self.maker.take() else {
            return;
        };
        let events = maker.cancel_all(None).await;
        self.maker = Some(maker);
        for ev in events {
            self.emit(ev);
        }
    }

    async fn evaluate_arb(&mut self, pair: &Pair) -> bool {
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
        self.emit(LedgerEvent::Opportunity {
            ts_ms: now_ms(),
            op: op.clone(),
        });
        if let Err(reason) = self.risk.check(&op) {
            tracing::info!(%reason, pair = %pair, "opportunity rejected by risk");
            self.emit(LedgerEvent::Rejected {
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
        for leg in out.legs.clone() {
            self.emit(leg);
        }
        for ex in &out.exchange_errors {
            self.risk.record_exchange_error(*ex);
        }
        if out.exchange_errors.is_empty() {
            self.risk.record_exchange_ok(op.buy_exchange);
            self.risk.record_exchange_ok(op.sell_exchange);
        }
        self.emit(LedgerEvent::Trade {
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
            self.emit(LedgerEvent::Halt {
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
        let mut publish = tokio::time::interval(Duration::from_secs(1));
        publish.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_balance_refresh = Instant::now();
        let mut control_rx = self.control_rx.take();
        self.publish();
        loop {
            tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    tracing::info!("shutdown requested");
                    break;
                }
                c = async { match control_rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } } => {
                    match c {
                        Some(c) => self.apply_control(c).await,
                        None => control_rx = None,
                    }
                }
                _ = publish.tick() => {
                    self.publish();
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
                    if let Some(sh) = &self.shared {
                        if let Some(rec) = sh.recorder.lock().unwrap().as_mut() {
                            rec.write(&q);
                        }
                    }
                    let pair = q.pair.clone();
                    self.book.update(q);
                    self.evaluate_pair(&pair).await;
                }
            }
        }
        self.maker_cancel_all().await;
        self.emit(LedgerEvent::Stop { ts_ms: now_ms() });
        self.publish();
        tracing::info!("{}", self.status_line());
        Ok(())
    }

    fn exchange_ids(&self) -> Vec<ExchangeId> {
        self.exchanges.keys().copied().collect()
    }
}
