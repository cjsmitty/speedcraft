//! Shared top-of-book cache. Writers are the exchange stream tasks; the
//! engine reads a per-pair snapshot on every update.

use crate::types::{ExchangeId, Pair, TopOfBook};
use dashmap::DashMap;
use std::time::Duration;

#[derive(Default)]
pub struct BookCache {
    books: DashMap<(ExchangeId, Pair), TopOfBook>,
}

impl BookCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&self, t: TopOfBook) {
        self.books.insert((t.exchange, t.pair.clone()), t);
    }

    /// All fresh, sane books for a pair.
    pub fn snapshot(&self, pair: &Pair, max_age: Duration) -> Vec<TopOfBook> {
        self.books
            .iter()
            .filter(|e| &e.key().1 == pair)
            .filter(|e| e.value().received_at.elapsed() <= max_age && e.value().is_sane())
            .map(|e| e.value().clone())
            .collect()
    }

    pub fn get(&self, ex: ExchangeId, pair: &Pair) -> Option<TopOfBook> {
        self.books.get(&(ex, pair.clone())).map(|v| v.clone())
    }

    pub fn len(&self) -> usize {
        self.books.len()
    }
    pub fn is_empty(&self) -> bool {
        self.books.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::time::Instant;

    #[test]
    fn stale_quotes_are_excluded() {
        let c = BookCache::new();
        let p = Pair::parse("BTC/USD").unwrap();
        c.update(TopOfBook {
            exchange: ExchangeId::Kraken,
            pair: p.clone(),
            bid: dec!(1),
            bid_qty: dec!(1),
            ask: dec!(2),
            ask_qty: dec!(1),
            received_at: Instant::now() - Duration::from_secs(5),
            received_ms: 0,
        });
        c.update(TopOfBook {
            exchange: ExchangeId::Coinbase,
            pair: p.clone(),
            bid: dec!(1),
            bid_qty: dec!(1),
            ask: dec!(2),
            ask_qty: dec!(1),
            received_at: Instant::now(),
            received_ms: 0,
        });
        let s = c.snapshot(&p, Duration::from_millis(500));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].exchange, ExchangeId::Coinbase);
    }
}
