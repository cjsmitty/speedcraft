//! Paper exchange: simulated fills against real (or synthetic) quotes with
//! fees, adverse slippage, latency and optional random misses. Market data
//! and market rules are delegated to an inner real exchange when present so
//! paper mode runs against live prices with no API keys.

use super::{Balances, Exchange, ExchangeError};
use crate::book::BookCache;
use crate::config::PaperConfig;
use crate::types::{
    ExchangeId, Fees, MarketRules, OrderRequest, OrderResult, OrderStatus, Pair, Side, TopOfBook,
};
use async_trait::async_trait;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub struct PaperExchange {
    id: ExchangeId,
    inner: Option<Arc<dyn Exchange>>,
    book: Arc<BookCache>,
    fees: Fees,
    cfg: PaperConfig,
    rules: HashMap<Pair, MarketRules>,
    balances: Mutex<Balances>,
    orders: Mutex<HashMap<String, OrderResult>>,
    /// Resting post-only orders: client_id -> request.
    resting: Mutex<HashMap<String, OrderRequest>>,
}

impl PaperExchange {
    pub fn new(
        id: ExchangeId,
        inner: Option<Arc<dyn Exchange>>,
        book: Arc<BookCache>,
        fees: Fees,
        cfg: PaperConfig,
        pairs: &[Pair],
        default_rules: MarketRules,
    ) -> Self {
        let mut balances = HashMap::new();
        let mut rules = HashMap::new();
        for p in pairs {
            balances.insert(p.quote.clone(), cfg.start_quote);
            balances.insert(p.base.clone(), cfg.start_base);
            rules.insert(p.clone(), default_rules.clone());
        }
        PaperExchange {
            id,
            inner,
            book,
            fees,
            cfg,
            rules,
            balances: Mutex::new(balances),
            orders: Mutex::new(HashMap::new()),
            resting: Mutex::new(HashMap::new()),
        }
    }

    /// A resting bid fills when the venue's best ask trades down to or
    /// through its price; a resting ask fills when the best bid rises to or
    /// through it. That is when real counterparties would have hit us.
    fn settle_resting(&self, client_id: &str) {
        let req = { self.resting.lock().unwrap().get(client_id).cloned() };
        let Some(req) = req else { return };
        let Some(q) = self.book.get(self.id, &req.pair) else {
            return;
        };
        let crossed = match req.side {
            Side::Buy => q.ask <= req.limit_price,
            Side::Sell => q.bid >= req.limit_price,
        };
        if !crossed {
            return;
        }
        // Fill at our limit price (maker gets their price), full size.
        let px = req.limit_price;
        let notional = req.qty * px;
        let fee = notional * self.fees.maker_rate();
        let mut bal = self.balances.lock().unwrap();
        let base = bal.entry(req.pair.base.clone()).or_insert(Decimal::ZERO);
        match req.side {
            Side::Buy => *base += req.qty,
            Side::Sell => *base -= req.qty,
        }
        let quote_bal = bal.entry(req.pair.quote.clone()).or_insert(Decimal::ZERO);
        match req.side {
            Side::Buy => *quote_bal -= notional + fee,
            Side::Sell => *quote_bal += notional - fee,
        }
        drop(bal);
        self.resting.lock().unwrap().remove(client_id);
        self.orders.lock().unwrap().insert(
            client_id.to_string(),
            OrderResult {
                client_id: client_id.to_string(),
                exchange_order_id: Some(format!("paper-{client_id}")),
                status: OrderStatus::Filled,
                filled_qty: req.qty,
                avg_price: px,
                fee_quote: fee,
            },
        );
    }

    pub fn set_rules(&mut self, pair: Pair, rules: MarketRules) {
        self.rules.insert(pair, rules);
    }

    pub fn balances_snapshot(&self) -> Balances {
        self.balances.lock().unwrap().clone()
    }

    fn simulate_fill(&self, req: &OrderRequest, quote: Option<TopOfBook>) -> OrderResult {
        let Some(q) = quote else {
            return OrderResult::nothing(&req.client_id, OrderStatus::Cancelled);
        };
        let slip = self.cfg.sim_slippage_bps / dec!(10000);
        let (avail_qty, px) = match req.side {
            Side::Buy => (q.ask_qty, q.ask * (Decimal::ONE + slip)),
            Side::Sell => (q.bid_qty, q.bid * (Decimal::ONE - slip)),
        };
        let crosses = match req.side {
            Side::Buy => px <= req.limit_price,
            Side::Sell => px >= req.limit_price,
        };
        if !crosses || avail_qty <= Decimal::ZERO {
            return OrderResult::nothing(&req.client_id, OrderStatus::Cancelled);
        }
        let qty = req.qty.min(avail_qty);
        let notional = qty * px;
        let fee = notional * self.fees.taker_rate();
        let mut bal = self.balances.lock().unwrap();
        let base = bal.entry(req.pair.base.clone()).or_insert(Decimal::ZERO);
        match req.side {
            Side::Buy => *base += qty,
            Side::Sell => {
                if *base < qty {
                    return OrderResult::nothing(&req.client_id, OrderStatus::Rejected);
                }
                *base -= qty;
            }
        }
        let quote_bal = bal.entry(req.pair.quote.clone()).or_insert(Decimal::ZERO);
        match req.side {
            Side::Buy => *quote_bal -= notional + fee,
            Side::Sell => *quote_bal += notional - fee,
        }
        OrderResult {
            client_id: req.client_id.clone(),
            exchange_order_id: Some(format!("paper-{}", req.client_id)),
            status: if qty == req.qty {
                OrderStatus::Filled
            } else {
                OrderStatus::PartiallyFilled
            },
            filled_qty: qty,
            avg_price: px,
            fee_quote: fee,
        }
    }
}

#[async_trait]
impl Exchange for PaperExchange {
    fn id(&self) -> ExchangeId {
        self.id
    }

    async fn fetch_fees(&self) -> Result<Fees, ExchangeError> {
        Ok(self.fees)
    }

    async fn fetch_balances(&self) -> Result<Balances, ExchangeError> {
        Ok(self.balances_snapshot())
    }

    async fn fetch_market_rules(&self, pair: &Pair) -> Result<MarketRules, ExchangeError> {
        if let Some(inner) = &self.inner {
            if let Ok(r) = inner.fetch_market_rules(pair).await {
                return Ok(r);
            }
        }
        self.rules
            .get(pair)
            .cloned()
            .ok_or_else(|| ExchangeError::Unsupported(format!("no rules for {pair}")))
    }

    async fn place_ioc_limit(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        if self.cfg.sim_latency_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.cfg.sim_latency_ms)).await;
        }
        if self.cfg.sim_miss_probability > 0.0
            && rand::random::<f64>() < self.cfg.sim_miss_probability
        {
            let r = OrderResult::nothing(&req.client_id, OrderStatus::Cancelled);
            self.orders
                .lock()
                .unwrap()
                .insert(req.client_id.clone(), r.clone());
            return Ok(r);
        }
        let q = self.book.get(self.id, &req.pair);
        let r = self.simulate_fill(req, q);
        self.orders
            .lock()
            .unwrap()
            .insert(req.client_id.clone(), r.clone());
        Ok(r)
    }

    async fn place_post_only(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        if self.cfg.sim_latency_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.cfg.sim_latency_ms)).await;
        }
        let q = self.book.get(self.id, &req.pair);
        let would_cross = match (&q, req.side) {
            (Some(q), Side::Buy) => req.limit_price >= q.ask,
            (Some(q), Side::Sell) => req.limit_price <= q.bid,
            (None, _) => false,
        };
        if would_cross {
            let r = OrderResult::nothing(&req.client_id, OrderStatus::Rejected);
            self.orders
                .lock()
                .unwrap()
                .insert(req.client_id.clone(), r.clone());
            return Ok(r);
        }
        if req.side == Side::Sell {
            let bal = self.balances.lock().unwrap();
            if bal.get(&req.pair.base).copied().unwrap_or(Decimal::ZERO) < req.qty {
                return Err(ExchangeError::Rejected("insufficient base balance".into()));
            }
        }
        let r = OrderResult {
            client_id: req.client_id.clone(),
            exchange_order_id: Some(format!("paper-{}", req.client_id)),
            status: OrderStatus::Open,
            filled_qty: Decimal::ZERO,
            avg_price: Decimal::ZERO,
            fee_quote: Decimal::ZERO,
        };
        self.resting
            .lock()
            .unwrap()
            .insert(req.client_id.clone(), req.clone());
        self.orders
            .lock()
            .unwrap()
            .insert(req.client_id.clone(), r.clone());
        Ok(r)
    }

    async fn cancel_order(
        &self,
        _pair: &Pair,
        client_id: &str,
        _xid: Option<&str>,
    ) -> Result<(), ExchangeError> {
        // Settle first: a fill that happened before the cancel arrived stands.
        self.settle_resting(client_id);
        if self.resting.lock().unwrap().remove(client_id).is_some() {
            if let Some(o) = self.orders.lock().unwrap().get_mut(client_id) {
                o.status = OrderStatus::Cancelled;
            }
        }
        Ok(())
    }

    async fn fetch_order(
        &self,
        _pair: &Pair,
        client_id: &str,
        _xid: Option<&str>,
    ) -> Result<OrderResult, ExchangeError> {
        self.settle_resting(client_id);
        self.orders
            .lock()
            .unwrap()
            .get(client_id)
            .cloned()
            .ok_or_else(|| ExchangeError::BadResponse(format!("unknown paper order {client_id}")))
    }

    async fn stream_top_of_book(
        &self,
        pairs: Vec<Pair>,
        tx: mpsc::Sender<TopOfBook>,
    ) -> Result<(), ExchangeError> {
        match &self.inner {
            Some(inner) => inner.stream_top_of_book(pairs, tx).await,
            None => {
                // Synthetic mode: quotes are injected directly into the cache.
                std::future::pending::<()>().await;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn setup(slip_bps: Decimal) -> (PaperExchange, Arc<BookCache>, Pair) {
        let book = Arc::new(BookCache::new());
        let pair = Pair::parse("BTC/USD").unwrap();
        let cfg = PaperConfig {
            sim_slippage_bps: slip_bps,
            sim_latency_ms: 0,
            ..Default::default()
        };
        let rules = MarketRules {
            qty_step: dec!(0.0001),
            price_tick: dec!(0.01),
            min_qty: dec!(0),
            min_notional: dec!(0),
        };
        let ex = PaperExchange::new(
            ExchangeId::Kraken,
            None,
            book.clone(),
            Fees {
                maker_bps: dec!(10),
                taker_bps: dec!(10),
            },
            cfg,
            std::slice::from_ref(&pair),
            rules,
        );
        book.update(TopOfBook {
            exchange: ExchangeId::Kraken,
            pair: pair.clone(),
            bid: dec!(100),
            bid_qty: dec!(0.5),
            ask: dec!(101),
            ask_qty: dec!(2),
            received_at: Instant::now(),
            received_ms: 0,
        });
        (ex, book, pair)
    }

    #[tokio::test]
    async fn buy_fills_at_ask_with_fee_and_updates_balances() {
        let (ex, _b, pair) = setup(dec!(0));
        let r = ex
            .place_ioc_limit(&OrderRequest {
                client_id: "a".into(),
                pair: pair.clone(),
                side: Side::Buy,
                qty: dec!(1),
                limit_price: dec!(101),
            })
            .await
            .unwrap();
        assert_eq!(r.status, OrderStatus::Filled);
        assert_eq!(r.avg_price, dec!(101));
        assert_eq!(r.fee_quote, dec!(0.101));
        let bal = ex.balances_snapshot();
        assert_eq!(bal["BTC"], dec!(1.2));
        assert_eq!(bal["USD"], dec!(10000) - dec!(101) - dec!(0.101));
    }

    #[tokio::test]
    async fn ioc_does_not_fill_when_limit_is_not_crossed_or_slippage_exceeds_limit() {
        let (ex, _b, pair) = setup(dec!(0));
        let r = ex
            .place_ioc_limit(&OrderRequest {
                client_id: "a".into(),
                pair: pair.clone(),
                side: Side::Buy,
                qty: dec!(1),
                limit_price: dec!(100.5),
            })
            .await
            .unwrap();
        assert_eq!(r.status, OrderStatus::Cancelled);
        assert_eq!(r.filled_qty, dec!(0));
        let (ex, _b, pair) = setup(dec!(50));
        let r = ex
            .place_ioc_limit(&OrderRequest {
                client_id: "a".into(),
                pair: pair.clone(),
                side: Side::Buy,
                qty: dec!(1),
                limit_price: dec!(101),
            })
            .await
            .unwrap();
        assert_eq!(r.status, OrderStatus::Cancelled);
    }

    #[tokio::test]
    async fn post_only_rests_then_fills_when_market_trades_through() {
        let (ex, book, pair) = setup(dec!(0));
        // Bid at 100.5 while ask is 101: rests.
        let req = OrderRequest {
            client_id: "m1".into(),
            pair: pair.clone(),
            side: Side::Buy,
            qty: dec!(0.1),
            limit_price: dec!(100.5),
        };
        let r = ex.place_post_only(&req).await.unwrap();
        assert_eq!(r.status, OrderStatus::Open);
        assert_eq!(
            ex.fetch_order(&pair, "m1", None).await.unwrap().status,
            OrderStatus::Open
        );
        // A bid at or above the ask would cross: rejected, nothing rests.
        let bad = OrderRequest {
            client_id: "m2".into(),
            pair: pair.clone(),
            side: Side::Buy,
            qty: dec!(0.1),
            limit_price: dec!(101),
        };
        assert_eq!(
            ex.place_post_only(&bad).await.unwrap().status,
            OrderStatus::Rejected
        );
        // Market trades down through our bid: filled at our price with maker fee.
        book.update(TopOfBook {
            exchange: ExchangeId::Kraken,
            pair: pair.clone(),
            bid: dec!(100.0),
            bid_qty: dec!(1),
            ask: dec!(100.4),
            ask_qty: dec!(1),
            received_at: Instant::now(),
            received_ms: 0,
        });
        let f = ex.fetch_order(&pair, "m1", None).await.unwrap();
        assert_eq!(f.status, OrderStatus::Filled);
        assert_eq!(f.avg_price, dec!(100.5));
        assert_eq!(f.fee_quote, dec!(0.1) * dec!(100.5) * dec!(0.001));
        assert_eq!(ex.balances_snapshot()["BTC"], dec!(0.3));
        // Cancel of a filled order is a no-op; cancel of a resting one removes it.
        ex.cancel_order(&pair, "m1", None).await.unwrap();
        assert_eq!(
            ex.fetch_order(&pair, "m1", None).await.unwrap().status,
            OrderStatus::Filled
        );
        let ask = OrderRequest {
            client_id: "m3".into(),
            pair: pair.clone(),
            side: Side::Sell,
            qty: dec!(0.1),
            limit_price: dec!(101.5),
        };
        assert_eq!(
            ex.place_post_only(&ask).await.unwrap().status,
            OrderStatus::Open
        );
        ex.cancel_order(&pair, "m3", None).await.unwrap();
        assert_eq!(
            ex.fetch_order(&pair, "m3", None).await.unwrap().status,
            OrderStatus::Cancelled
        );
        assert_eq!(ex.balances_snapshot()["BTC"], dec!(0.3));
    }

    #[tokio::test]
    async fn sell_partially_fills_against_displayed_depth() {
        let (ex, _b, pair) = setup(dec!(0));
        let r = ex
            .place_ioc_limit(&OrderRequest {
                client_id: "s".into(),
                pair: pair.clone(),
                side: Side::Sell,
                qty: dec!(1),
                limit_price: dec!(100),
            })
            .await
            .unwrap();
        // only 0.5 displayed, and we hold 0.2 → rejected? No: we hold 0.2 and try to sell 0.5 → Rejected.
        assert_eq!(r.status, OrderStatus::Rejected);
        let r = ex
            .place_ioc_limit(&OrderRequest {
                client_id: "s2".into(),
                pair: pair.clone(),
                side: Side::Sell,
                qty: dec!(0.1),
                limit_price: dec!(100),
            })
            .await
            .unwrap();
        assert_eq!(r.status, OrderStatus::Filled);
        assert_eq!(ex.balances_snapshot()["BTC"], dec!(0.1));
        let fetched = ex.fetch_order(&pair, "s2", None).await.unwrap();
        assert_eq!(fetched, r);
    }
}
