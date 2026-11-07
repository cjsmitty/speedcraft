//! Maker mode: rest post-only quotes on a cheap-maker venue priced off the
//! hedge venue's book, and hedge every fill immediately with a taker order
//! on the hedge venue. Earns the spread instead of paying it; the cost is
//! adverse selection, which shows up honestly in realised PnL.

use crate::book::BookCache;
use crate::config::MakerConfig;
use crate::exchanges::Exchange;
use crate::executor::{Executor, TradeOutcome};
use crate::ledger::LedgerEvent;
use crate::strategy::maker::{
    desired_quotes, needs_requote, DesiredQuote, DesiredQuotes, MakerBalances, MakerParams,
};
use crate::types::{now_ms, ExchangeId, Fees, MarketRules, OrderRequest, OrderStatus, Pair, Side};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct Resting {
    client_id: String,
    exchange_order_id: Option<String>,
    quote: DesiredQuote,
    last_poll: Instant,
}

#[derive(Default)]
struct PairState {
    bid: Option<Resting>,
    ask: Option<Resting>,
    inventory_base: Decimal,
    fills: u32,
    realised_pnl: Decimal,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MakerView {
    pub pair: String,
    pub quote_exchange: ExchangeId,
    pub hedge_exchange: ExchangeId,
    pub bid: Option<DesiredQuote>,
    pub ask: Option<DesiredQuote>,
    pub inventory_base: Decimal,
    pub fills: u32,
    pub realised_pnl: Decimal,
}

/// What happened in one tick that the engine must account for.
pub struct FillReport {
    pub pair: Pair,
    pub side: Side,
    pub qty: Decimal,
    pub price: Decimal,
    pub outcome: TradeOutcome,
    pub legs: Vec<LedgerEvent>,
}

pub struct MakerManager {
    pub cfg: MakerConfig,
    params: MakerParams,
    state: HashMap<Pair, PairState>,
    quote_ex: Arc<dyn Exchange>,
    hedge_ex: Arc<dyn Exchange>,
}

fn cid(prefix: &str) -> String {
    format!("mk-{prefix}-{}", uuid::Uuid::new_v4().simple())
}

impl MakerManager {
    pub fn new(
        cfg: MakerConfig,
        slippage_buffer_bps: Decimal,
        quote_ex: Arc<dyn Exchange>,
        hedge_ex: Arc<dyn Exchange>,
    ) -> Self {
        let params = MakerParams {
            target_edge_bps: cfg.target_edge_bps,
            slippage_buffer_bps,
            quote_notional: cfg.quote_notional,
            max_inventory_notional: cfg.max_inventory_notional,
            requote_bps: cfg.requote_bps,
            max_improve_bps: cfg.max_improve_bps,
        };
        MakerManager {
            cfg,
            params,
            state: HashMap::new(),
            quote_ex,
            hedge_ex,
        }
    }

    pub fn handles(&self, pair: &Pair) -> bool {
        match &self.cfg.pairs {
            Some(list) => list
                .iter()
                .any(|p| Pair::parse(p).ok().as_ref() == Some(pair)),
            None => true,
        }
    }

    pub fn views(&self) -> Vec<MakerView> {
        let mut v: Vec<MakerView> = self
            .state
            .iter()
            .map(|(p, s)| MakerView {
                pair: p.to_string(),
                quote_exchange: self.cfg.quote_exchange,
                hedge_exchange: self.cfg.hedge_exchange,
                bid: s.bid.as_ref().map(|r| r.quote.clone()),
                ask: s.ask.as_ref().map(|r| r.quote.clone()),
                inventory_base: s.inventory_base,
                fills: s.fills,
                realised_pnl: s.realised_pnl,
            })
            .collect();
        v.sort_by(|a, b| a.pair.cmp(&b.pair));
        v
    }

    /// Cancel everything (shutdown, halt, stale data).
    pub async fn cancel_all(&mut self, pair_filter: Option<&Pair>) -> Vec<LedgerEvent> {
        let mut events = Vec::new();
        let pairs: Vec<Pair> = self
            .state
            .keys()
            .filter(|p| pair_filter.map(|f| f == *p).unwrap_or(true))
            .cloned()
            .collect();
        for pair in pairs {
            for side in [Side::Buy, Side::Sell] {
                if let Some(ev) = self.cancel_side(&pair, side).await {
                    events.push(ev);
                }
            }
        }
        events
    }

    async fn cancel_side(&mut self, pair: &Pair, side: Side) -> Option<LedgerEvent> {
        let st = self.state.entry(pair.clone()).or_default();
        let slot = match side {
            Side::Buy => &mut st.bid,
            Side::Sell => &mut st.ask,
        };
        let r = slot.take()?;
        if let Err(e) = self
            .quote_ex
            .cancel_order(pair, &r.client_id, r.exchange_order_id.as_deref())
            .await
        {
            tracing::warn!(error = %e, client_id = %r.client_id, "cancel failed; will re-check on next poll");
            // Put it back so a fill that raced the cancel is still detected.
            let st = self.state.entry(pair.clone()).or_default();
            match side {
                Side::Buy => st.bid = Some(r),
                Side::Sell => st.ask = Some(r),
            }
            return None;
        }
        Some(LedgerEvent::Quote {
            ts_ms: now_ms(),
            exchange: self.cfg.quote_exchange,
            pair: pair.clone(),
            side,
            price: r.quote.price,
            qty: r.quote.qty,
            expected_edge_bps: r.quote.expected_edge_bps,
            action: "cancel".into(),
        })
    }

    /// One pass for a pair: detect fills, hedge them, then re-quote.
    /// `allowed` is false when risk says no new quotes (then everything is pulled).
    #[allow(clippy::too_many_arguments)]
    pub async fn tick(
        &mut self,
        pair: &Pair,
        book: &Arc<BookCache>,
        fees: &HashMap<ExchangeId, Fees>,
        rules: &HashMap<(ExchangeId, Pair), MarketRules>,
        balances: MakerBalances,
        max_quote_age: Duration,
        allowed: bool,
        risk_cfg: &crate::config::RiskConfig,
    ) -> (Vec<LedgerEvent>, Vec<FillReport>) {
        let mut events = Vec::new();
        let mut fills = Vec::new();
        self.state.entry(pair.clone()).or_default();

        // 1. Fill detection on resting orders.
        for side in [Side::Buy, Side::Sell] {
            if let Some(f) = self
                .poll_side(pair, side, book, fees, rules, risk_cfg)
                .await
            {
                events.extend(f.legs.iter().cloned());
                fills.push(f);
            }
        }

        // 2. Decide desired quotes.
        let q = book.get(self.cfg.quote_exchange, pair);
        let h = book.get(self.cfg.hedge_exchange, pair);
        let fresh = |t: &crate::types::TopOfBook| {
            t.received_at.elapsed() <= max_quote_age.max(Duration::from_secs(3))
        };
        let desired = match (q, h) {
            (Some(q), Some(h)) if allowed && fresh(&q) && fresh(&h) => {
                let (Some(qf), Some(hf)) = (
                    fees.get(&self.cfg.quote_exchange),
                    fees.get(&self.cfg.hedge_exchange),
                ) else {
                    return (events, fills);
                };
                let Some(qr) = rules.get(&(self.cfg.quote_exchange, pair.clone())) else {
                    return (events, fills);
                };
                let inv = self.state[pair].inventory_base;
                desired_quotes(&q, &h, qf, hf, qr, inv, balances, &self.params)
            }
            _ => DesiredQuotes::default(),
        };

        // 3. Reconcile each side.
        for (side, want) in [(Side::Buy, desired.bid), (Side::Sell, desired.ask)] {
            let current = {
                let st = &self.state[pair];
                match side {
                    Side::Buy => st.bid.as_ref().map(|r| r.quote.clone()),
                    Side::Sell => st.ask.as_ref().map(|r| r.quote.clone()),
                }
            };
            if !needs_requote(current.as_ref(), want.as_ref(), self.params.requote_bps) {
                continue;
            }
            if current.is_some() {
                if let Some(ev) = self.cancel_side(pair, side).await {
                    events.push(ev);
                } else {
                    continue; // cancel failed; do not stack a second order
                }
            }
            let Some(want) = want else { continue };
            let req = OrderRequest {
                client_id: cid(match side {
                    Side::Buy => "bid",
                    Side::Sell => "ask",
                }),
                pair: pair.clone(),
                side,
                qty: want.qty,
                limit_price: want.price,
            };
            match self.quote_ex.place_post_only(&req).await {
                Ok(r) if r.status == OrderStatus::Open => {
                    let st = self.state.get_mut(pair).unwrap();
                    let resting = Resting {
                        client_id: req.client_id.clone(),
                        exchange_order_id: r.exchange_order_id,
                        quote: want.clone(),
                        last_poll: Instant::now(),
                    };
                    match side {
                        Side::Buy => st.bid = Some(resting),
                        Side::Sell => st.ask = Some(resting),
                    }
                    events.push(LedgerEvent::Quote {
                        ts_ms: now_ms(),
                        exchange: self.cfg.quote_exchange,
                        pair: pair.clone(),
                        side,
                        price: want.price,
                        qty: want.qty,
                        expected_edge_bps: want.expected_edge_bps,
                        action: "place".into(),
                    });
                }
                Ok(r) => {
                    tracing::debug!(pair = %pair, %side, status = ?r.status, "post-only not resting (would cross or rejected)");
                }
                Err(e) => {
                    tracing::warn!(pair = %pair, %side, error = %e, "post-only placement failed");
                }
            }
        }
        (events, fills)
    }

    async fn poll_side(
        &mut self,
        pair: &Pair,
        side: Side,
        book: &Arc<BookCache>,
        fees: &HashMap<ExchangeId, Fees>,
        rules: &HashMap<(ExchangeId, Pair), MarketRules>,
        risk_cfg: &crate::config::RiskConfig,
    ) -> Option<FillReport> {
        let (client_id, xid, quote, due) = {
            let st = self.state.get_mut(pair)?;
            let slot = match side {
                Side::Buy => st.bid.as_mut(),
                Side::Sell => st.ask.as_mut(),
            }?;
            // Poll when the throttle allows, or at once if the quote venue's book
            // indicates our level was traded through.
            let touched = book
                .get(self.cfg.quote_exchange, pair)
                .map(|q| match side {
                    Side::Buy => q.ask <= slot.quote.price,
                    Side::Sell => q.bid >= slot.quote.price,
                })
                .unwrap_or(false);
            let due =
                touched || slot.last_poll.elapsed() >= Duration::from_millis(self.cfg.fill_poll_ms);
            if due {
                slot.last_poll = Instant::now();
            }
            (
                slot.client_id.clone(),
                slot.exchange_order_id.clone(),
                slot.quote.clone(),
                due,
            )
        };
        if !due {
            return None;
        }
        let r = match self
            .quote_ex
            .fetch_order(pair, &client_id, xid.as_deref())
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "maker order status check failed");
                return None;
            }
        };
        match r.status {
            OrderStatus::Open | OrderStatus::Unknown => None,
            OrderStatus::Cancelled | OrderStatus::Rejected if r.filled_qty.is_zero() => {
                self.clear_slot(pair, side);
                None
            }
            _ => {
                // Filled or partially filled (then cancelled): hedge what filled.
                self.clear_slot(pair, side);
                let filled = r.filled_qty;
                if filled.is_zero() {
                    return None;
                }
                let px = if r.avg_price.is_zero() {
                    quote.price
                } else {
                    r.avg_price
                };
                let fee = if r.fee_quote.is_zero() {
                    filled
                        * px
                        * fees
                            .get(&self.cfg.quote_exchange)
                            .map(|f| f.maker_rate())
                            .unwrap_or(Decimal::ZERO)
                } else {
                    r.fee_quote
                };
                let mut out = TradeOutcome {
                    matched_qty: filled,
                    realised_pnl: Decimal::ZERO,
                    success: false,
                    halt: None,
                    note: String::new(),
                    legs: Vec::new(),
                    exchange_errors: Vec::new(),
                };
                // Maker leg cash flow.
                out.realised_pnl += match side {
                    Side::Buy => -(filled * px) - fee,
                    Side::Sell => filled * px - fee,
                };
                out.legs.push(LedgerEvent::Leg {
                    ts_ms: now_ms(),
                    exchange: self.cfg.quote_exchange,
                    pair: pair.clone(),
                    side,
                    requested_qty: quote.qty,
                    limit_price: quote.price,
                    result: r.clone(),
                    unwind: false,
                });
                // Hedge on the other venue by crossing its book.
                let mut exchanges: HashMap<ExchangeId, Arc<dyn Exchange>> = HashMap::new();
                exchanges.insert(self.cfg.hedge_exchange, self.hedge_ex.clone());
                let ex = Executor {
                    risk: risk_cfg.clone(),
                    slippage_buffer_bps: self.params.slippage_buffer_bps,
                    exchanges,
                    rules: rules.clone(),
                    book: book.clone(),
                };
                let hedge_side = side.opposite();
                let ref_px = book
                    .get(self.cfg.hedge_exchange, pair)
                    .map(|h| {
                        if hedge_side == Side::Sell {
                            h.bid
                        } else {
                            h.ask
                        }
                    })
                    .unwrap_or(px);
                let leftover = ex
                    .hedge(
                        self.cfg.hedge_exchange,
                        pair,
                        hedge_side,
                        filled,
                        ref_px,
                        &mut out,
                    )
                    .await;
                out.success = leftover.is_zero() && out.halt.is_none();
                out.note = if out.success {
                    format!("maker {side} {filled} @ {px} hedged")
                } else {
                    format!("maker {side} {filled} @ {px}; {leftover} left unhedged")
                };
                let st = self.state.get_mut(pair).unwrap();
                st.fills += 1;
                st.realised_pnl += out.realised_pnl;
                st.inventory_base += match side {
                    Side::Buy => filled,
                    Side::Sell => -filled,
                };
                let legs = out.legs.clone();
                Some(FillReport {
                    pair: pair.clone(),
                    side,
                    qty: filled,
                    price: px,
                    outcome: out,
                    legs,
                })
            }
        }
    }

    fn clear_slot(&mut self, pair: &Pair, side: Side) {
        if let Some(st) = self.state.get_mut(pair) {
            match side {
                Side::Buy => st.bid = None,
                Side::Sell => st.ask = None,
            }
        }
    }
}
