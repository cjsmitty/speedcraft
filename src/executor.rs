//! Two-leg execution. Both IOC legs are sent at the same time; whatever does
//! not match is unwound immediately. The executor never decides whether to
//! trade: that is the strategy's and the risk manager's job.

use crate::book::BookCache;
use crate::config::RiskConfig;
use crate::exchanges::{Exchange, ExchangeError};
use crate::ledger::LedgerEvent;
use crate::strategy::Opportunity;
use crate::types::{
    now_ms, ExchangeId, MarketRules, OrderRequest, OrderResult, OrderStatus, Pair, Side,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const BPS: Decimal = dec!(10000);

#[derive(Clone, Debug)]
pub struct TradeOutcome {
    pub matched_qty: Decimal,
    pub realised_pnl: Decimal,
    /// Both legs matched fully as planned (no unwind, no unknowns).
    pub success: bool,
    /// Something is in a state a human must look at (unknown fill, failed unwind).
    pub halt: Option<String>,
    pub note: String,
    pub legs: Vec<LedgerEvent>,
    pub exchange_errors: Vec<ExchangeId>,
}

pub struct Executor {
    pub risk: RiskConfig,
    pub slippage_buffer_bps: Decimal,
    pub exchanges: HashMap<ExchangeId, Arc<dyn Exchange>>,
    pub rules: HashMap<(ExchangeId, Pair), MarketRules>,
    pub book: Arc<BookCache>,
}

fn client_id(prefix: &str) -> String {
    format!("sc-{prefix}-{}", uuid::Uuid::new_v4().simple())
}

impl Executor {
    fn rules_for(&self, ex: ExchangeId, pair: &Pair) -> MarketRules {
        self.rules
            .get(&(ex, pair.clone()))
            .cloned()
            .unwrap_or(MarketRules {
                qty_step: dec!(0.00000001),
                price_tick: dec!(0.01),
                min_qty: Decimal::ZERO,
                min_notional: Decimal::ZERO,
            })
    }

    /// Place with timeout; on timeout/error, try to learn the real outcome.
    async fn place(
        &self,
        ex: ExchangeId,
        req: &OrderRequest,
        errors: &mut Vec<ExchangeId>,
    ) -> OrderResult {
        let Some(x) = self.exchanges.get(&ex) else {
            errors.push(ex);
            return OrderResult::nothing(&req.client_id, OrderStatus::Rejected);
        };
        let t = Duration::from_millis(self.risk.order_timeout_ms);
        let first = tokio::time::timeout(t, x.place_ioc_limit(req)).await;
        match first {
            Ok(Ok(r)) => r,
            Ok(Err(ExchangeError::Rejected(m))) => {
                tracing::warn!(exchange = %ex, msg = %m, "order rejected");
                OrderResult::nothing(&req.client_id, OrderStatus::Rejected)
            }
            Ok(Err(e)) => {
                tracing::warn!(exchange = %ex, error = %e, "order error; checking status");
                errors.push(ex);
                self.lookup(x.as_ref(), req).await
            }
            Err(_) => {
                tracing::warn!(exchange = %ex, "order timed out; checking status");
                errors.push(ex);
                self.lookup(x.as_ref(), req).await
            }
        }
    }

    async fn lookup(&self, x: &dyn Exchange, req: &OrderRequest) -> OrderResult {
        for attempt in 0..4u32 {
            tokio::time::sleep(Duration::from_millis(250 * (attempt as u64 + 1))).await;
            match tokio::time::timeout(
                Duration::from_secs(5),
                x.fetch_order(&req.pair, &req.client_id, None),
            )
            .await
            {
                Ok(Ok(r)) => return r,
                Ok(Err(ExchangeError::BadResponse(m)))
                    if m.contains("not found") || m.contains("unknown") =>
                {
                    // The exchange never saw it: nothing filled.
                    return OrderResult::nothing(&req.client_id, OrderStatus::Cancelled);
                }
                _ => continue,
            }
        }
        OrderResult::nothing(&req.client_id, OrderStatus::Unknown)
    }

    fn leg_event(ex: ExchangeId, req: &OrderRequest, r: &OrderResult, unwind: bool) -> LedgerEvent {
        LedgerEvent::Leg {
            ts_ms: now_ms(),
            exchange: ex,
            pair: req.pair.clone(),
            side: req.side,
            requested_qty: req.qty,
            limit_price: req.limit_price,
            result: r.clone(),
            unwind,
        }
    }

    fn cash_flow(side: Side, r: &OrderResult) -> Decimal {
        match side {
            Side::Buy => -r.filled_quote() - r.fee_quote,
            Side::Sell => r.filled_quote() - r.fee_quote,
        }
    }

    /// Cross the book on `ex` for `qty` of base (used to unwind a one-legged
    /// arbitrage fill and to hedge a maker fill). Returns leftover quantity.
    pub async fn hedge(
        &self,
        ex: ExchangeId,
        pair: &Pair,
        side: Side,
        mut qty: Decimal,
        ref_price: Decimal,
        out: &mut TradeOutcome,
    ) -> Decimal {
        let rules = self.rules_for(ex, pair);
        for attempt in 1..=3u32 {
            qty = rules.round_qty_down(qty);
            if qty <= Decimal::ZERO || qty < rules.min_qty {
                return Decimal::ZERO;
            }
            let cross = self.risk.unwind_cross_bps * Decimal::from(attempt) / BPS;
            let quote = self.book.get(ex, pair);
            let base_px = match (side, &quote) {
                (Side::Sell, Some(q)) => q.bid,
                (Side::Buy, Some(q)) => q.ask,
                _ => ref_price,
            };
            let limit = match side {
                Side::Sell => rules.round_price_down(base_px * (Decimal::ONE - cross)),
                Side::Buy => rules.round_price_up(base_px * (Decimal::ONE + cross)),
            };
            let req = OrderRequest {
                client_id: client_id("unwind"),
                pair: pair.clone(),
                side,
                qty,
                limit_price: limit,
            };
            tracing::warn!(exchange = %ex, %side, %qty, %limit, attempt, "unwinding one-legged fill");
            let r = self.place(ex, &req, &mut out.exchange_errors).await;
            out.legs.push(Self::leg_event(ex, &req, &r, true));
            out.realised_pnl += Self::cash_flow(side, &r);
            if r.status == OrderStatus::Unknown {
                out.halt = Some(format!(
                    "unwind on {ex} has unknown status; inventory may be open"
                ));
                return qty;
            }
            qty -= r.filled_qty;
            if qty <= Decimal::ZERO {
                return Decimal::ZERO;
            }
        }
        qty
    }

    pub async fn execute(&self, op: &Opportunity) -> TradeOutcome {
        let mut out = TradeOutcome {
            matched_qty: Decimal::ZERO,
            realised_pnl: Decimal::ZERO,
            success: false,
            halt: None,
            note: String::new(),
            legs: Vec::new(),
            exchange_errors: Vec::new(),
        };
        let pair = &op.pair;
        let half = self.slippage_buffer_bps / dec!(2) / BPS;
        let buy_rules = self.rules_for(op.buy_exchange, pair);
        let sell_rules = self.rules_for(op.sell_exchange, pair);
        let buy_req = OrderRequest {
            client_id: client_id("buy"),
            pair: pair.clone(),
            side: Side::Buy,
            qty: op.qty,
            limit_price: buy_rules.round_price_up(op.buy_price * (Decimal::ONE + half)),
        };
        let sell_req = OrderRequest {
            client_id: client_id("sell"),
            pair: pair.clone(),
            side: Side::Sell,
            qty: op.qty,
            limit_price: sell_rules.round_price_down(op.sell_price * (Decimal::ONE - half)),
        };
        // Invariant: even at the limit prices, the trade is still profitable
        // after fees. Otherwise the strategy and executor disagree: refuse.
        if sell_req.limit_price <= buy_req.limit_price {
            out.note = "limit prices cross the wrong way; refusing".into();
            return out;
        }

        let mut errs_a = Vec::new();
        let mut errs_b = Vec::new();
        let (buy, sell) = tokio::join!(
            self.place(op.buy_exchange, &buy_req, &mut errs_a),
            self.place(op.sell_exchange, &sell_req, &mut errs_b)
        );
        out.exchange_errors.extend(errs_a);
        out.exchange_errors.extend(errs_b);
        out.legs
            .push(Self::leg_event(op.buy_exchange, &buy_req, &buy, false));
        out.legs
            .push(Self::leg_event(op.sell_exchange, &sell_req, &sell, false));
        out.realised_pnl += Self::cash_flow(Side::Buy, &buy) + Self::cash_flow(Side::Sell, &sell);

        if buy.status == OrderStatus::Unknown || sell.status == OrderStatus::Unknown {
            out.halt = Some(
                "a leg has unknown fill status; refusing to continue until a human checks".into(),
            );
            out.note = "unknown leg status".into();
            return out;
        }

        let matched = buy.filled_qty.min(sell.filled_qty);
        out.matched_qty = matched;
        let excess_long = buy.filled_qty - matched; // bought more than sold: sell it on the buy exchange
        let excess_short = sell.filled_qty - matched; // sold more than bought: buy it back on the sell exchange

        let mut leftover = Decimal::ZERO;
        if excess_long > Decimal::ZERO {
            leftover += self
                .hedge(
                    op.buy_exchange,
                    pair,
                    Side::Sell,
                    excess_long,
                    op.buy_price,
                    &mut out,
                )
                .await;
        }
        if excess_short > Decimal::ZERO {
            leftover += self
                .hedge(
                    op.sell_exchange,
                    pair,
                    Side::Buy,
                    excess_short,
                    op.sell_price,
                    &mut out,
                )
                .await;
        }

        if leftover > Decimal::ZERO && out.halt.is_none() {
            out.halt = Some(format!(
                "{leftover} {} left unhedged after unwind attempts",
                pair.base
            ));
        }
        let full = matched == op.qty && excess_long.is_zero() && excess_short.is_zero();
        out.success = full && out.halt.is_none();
        out.note = if full {
            "both legs filled".into()
        } else if matched.is_zero() && excess_long.is_zero() && excess_short.is_zero() {
            "nothing filled".into()
        } else {
            format!("partial: matched {matched}, unwound long {excess_long} short {excess_short}, leftover {leftover}")
        };
        if !full && matched.is_zero() && excess_long.is_zero() && excess_short.is_zero() {
            // Nothing happened: not a success, but not a loss either.
            out.success = false;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PaperConfig;
    use crate::exchanges::paper::PaperExchange;
    use crate::types::{Fees, TopOfBook};
    use std::time::Instant;

    fn rules() -> MarketRules {
        MarketRules {
            qty_step: dec!(0.0001),
            price_tick: dec!(0.01),
            min_qty: dec!(0.0001),
            min_notional: dec!(0),
        }
    }
    fn risk() -> RiskConfig {
        RiskConfig {
            max_daily_loss_quote: dec!(25),
            max_consecutive_failures: 3,
            max_consecutive_errors: 3,
            pair_cooldown_ms: 0,
            max_trades_per_day: 100,
            order_timeout_ms: 1000,
            unwind_cross_bps: dec!(20),
        }
    }
    fn quote(ex: ExchangeId, pair: &Pair, bid: Decimal, ask: Decimal, qty: Decimal) -> TopOfBook {
        TopOfBook {
            exchange: ex,
            pair: pair.clone(),
            bid,
            bid_qty: qty,
            ask,
            ask_qty: qty,
            received_at: Instant::now(),
            received_ms: 0,
        }
    }
    fn setup(miss_prob: f64, sell_depth: Decimal) -> (Executor, Arc<BookCache>, Opportunity) {
        let book = Arc::new(BookCache::new());
        let pair = Pair::parse("BTC/USD").unwrap();
        let cfg = PaperConfig {
            sim_latency_ms: 0,
            sim_slippage_bps: dec!(0),
            sim_miss_probability: miss_prob,
            ..Default::default()
        };
        let fees = Fees {
            maker_bps: dec!(10),
            taker_bps: dec!(10),
        };
        let cb: Arc<dyn Exchange> = Arc::new(PaperExchange::new(
            ExchangeId::Coinbase,
            None,
            book.clone(),
            fees,
            cfg.clone(),
            std::slice::from_ref(&pair),
            rules(),
        ));
        let kr: Arc<dyn Exchange> = Arc::new(PaperExchange::new(
            ExchangeId::Kraken,
            None,
            book.clone(),
            fees,
            cfg,
            std::slice::from_ref(&pair),
            rules(),
        ));
        book.update(quote(
            ExchangeId::Coinbase,
            &pair,
            dec!(99),
            dec!(100),
            dec!(5),
        ));
        book.update(quote(
            ExchangeId::Kraken,
            &pair,
            dec!(101),
            dec!(102),
            sell_depth,
        ));
        let mut exchanges: HashMap<ExchangeId, Arc<dyn Exchange>> = HashMap::new();
        exchanges.insert(ExchangeId::Coinbase, cb);
        exchanges.insert(ExchangeId::Kraken, kr);
        let mut r = HashMap::new();
        r.insert((ExchangeId::Coinbase, pair.clone()), rules());
        r.insert((ExchangeId::Kraken, pair.clone()), rules());
        let ex = Executor {
            risk: risk(),
            slippage_buffer_bps: dec!(2),
            exchanges,
            rules: r,
            book: book.clone(),
        };
        let op = Opportunity {
            pair,
            buy_exchange: ExchangeId::Coinbase,
            sell_exchange: ExchangeId::Kraken,
            buy_price: dec!(100),
            sell_price: dec!(101),
            qty: dec!(0.1),
            gross_edge_bps: dec!(100),
            net_edge_bps: dec!(78),
            expected_profit_quote: dec!(0.078),
            buy_fee_quote: dec!(0.01),
            sell_fee_quote: dec!(0.0101),
        };
        (ex, book, op)
    }

    #[tokio::test]
    async fn both_legs_fill_and_pnl_is_exact() {
        let (ex, _b, op) = setup(0.0, dec!(5));
        let out = ex.execute(&op).await;
        assert!(out.success, "{}", out.note);
        assert_eq!(out.matched_qty, dec!(0.1));
        // buy 0.1 @ 100 = 10, fee 0.01; sell 0.1 @ 101 = 10.1, fee 0.0101
        assert_eq!(
            out.realised_pnl,
            dec!(10.1) - dec!(0.0101) - dec!(10) - dec!(0.01)
        );
        assert!(out.halt.is_none());
        assert_eq!(out.legs.len(), 2);
    }

    #[tokio::test]
    async fn one_leg_partial_is_unwound_and_flagged_not_success() {
        // Kraken shows only 0.04 so the sell partially fills; the extra 0.06
        // bought on Coinbase must be sold back there.
        let (ex, _b, op) = setup(0.0, dec!(0.04));
        let out = ex.execute(&op).await;
        assert!(!out.success);
        assert_eq!(out.matched_qty, dec!(0.04));
        assert!(out.halt.is_none(), "unwind should succeed: {:?}", out.halt);
        let unwinds: Vec<_> = out
            .legs
            .iter()
            .filter(|l| matches!(l, LedgerEvent::Leg { unwind: true, .. }))
            .collect();
        assert_eq!(unwinds.len(), 1);
        // matched part profit: 0.04*(101-100) - fees(0.004+0.00404) = 0.03196
        // unwound 0.06 bought at 100, sold at 99 (bid) → -0.06, fees 0.006 + 0.00594
        let expected = dec!(0.04) * dec!(1)
            - dec!(0.004)
            - dec!(0.00404)
            - dec!(0.06)
            - dec!(0.006)
            - dec!(0.00594);
        assert_eq!(out.realised_pnl, expected);
        assert!(out.note.starts_with("partial"));
    }

    #[tokio::test]
    async fn missed_legs_never_panic_and_nothing_filled_is_flat() {
        let (ex, _b, op) = setup(1.0, dec!(5));
        let out = ex.execute(&op).await;
        assert!(!out.success);
        assert_eq!(out.realised_pnl, dec!(0));
        assert_eq!(out.note, "nothing filled");
        assert!(out.halt.is_none());
    }

    #[tokio::test]
    async fn refuses_when_limits_cross_wrong_way() {
        let (ex, _b, mut op) = setup(0.0, dec!(5));
        op.sell_price = dec!(100);
        op.buy_price = dec!(100);
        let out = ex.execute(&op).await;
        assert!(out.legs.is_empty());
        assert!(out.note.contains("refusing"));
    }
}
