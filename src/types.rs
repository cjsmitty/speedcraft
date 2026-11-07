//! Core value types. Money is always `rust_decimal::Decimal`; never f64.

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeId {
    Coinbase,
    Kraken,
    Binance,
    BinanceUs,
}

impl ExchangeId {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExchangeId::Coinbase => "coinbase",
            ExchangeId::Kraken => "kraken",
            ExchangeId::Binance => "binance",
            ExchangeId::BinanceUs => "binance_us",
        }
    }
    pub fn all() -> [ExchangeId; 4] {
        [
            ExchangeId::Coinbase,
            ExchangeId::Kraken,
            ExchangeId::Binance,
            ExchangeId::BinanceUs,
        ]
    }
}

impl fmt::Display for ExchangeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ExchangeId {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "coinbase" => Ok(ExchangeId::Coinbase),
            "kraken" => Ok(ExchangeId::Kraken),
            "binance" => Ok(ExchangeId::Binance),
            "binance_us" | "binanceus" => Ok(ExchangeId::BinanceUs),
            other => Err(format!("unknown exchange '{other}'")),
        }
    }
}

/// Canonical pair, e.g. base "BTC", quote "USD". Written as "BTC/USD".
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub struct Pair {
    pub base: String,
    pub quote: String,
}

impl Pair {
    pub fn parse(s: &str) -> Result<Pair, String> {
        let (b, q) = s
            .split_once('/')
            .ok_or_else(|| format!("pair '{s}' must look like BASE/QUOTE"))?;
        if b.is_empty() || q.is_empty() {
            return Err(format!("pair '{s}' must look like BASE/QUOTE"));
        }
        Ok(Pair {
            base: b.to_ascii_uppercase(),
            quote: q.to_ascii_uppercase(),
        })
    }
}

impl fmt::Display for Pair {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.base, self.quote)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Side::Buy => "buy",
            Side::Sell => "sell",
        })
    }
}

/// Best bid / best ask on one exchange for one pair.
#[derive(Clone, Debug, PartialEq)]
pub struct TopOfBook {
    pub exchange: ExchangeId,
    pub pair: Pair,
    pub bid: Decimal,
    pub bid_qty: Decimal,
    pub ask: Decimal,
    pub ask_qty: Decimal,
    /// Local monotonic receive time, used for staleness checks.
    pub received_at: Instant,
    /// Wall clock receive time (ms since epoch) for logging and replay.
    pub received_ms: i64,
}

impl TopOfBook {
    pub fn is_sane(&self) -> bool {
        self.bid > Decimal::ZERO
            && self.ask > Decimal::ZERO
            && self.bid_qty >= Decimal::ZERO
            && self.ask_qty >= Decimal::ZERO
            && self.ask >= self.bid
    }
}

/// Fees in basis points (1 bp = 0.01%). 10 bps = 0.10%.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fees {
    pub maker_bps: Decimal,
    pub taker_bps: Decimal,
}

impl Fees {
    pub fn taker_rate(&self) -> Decimal {
        self.taker_bps / Decimal::from(10_000)
    }
    pub fn maker_rate(&self) -> Decimal {
        self.maker_bps / Decimal::from(10_000)
    }
}

/// Exchange-specific rounding and minimums for a market.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MarketRules {
    /// Smallest base-quantity increment (e.g. 0.00001 BTC).
    pub qty_step: Decimal,
    /// Smallest price increment (e.g. 0.01 USD).
    pub price_tick: Decimal,
    /// Minimum base quantity.
    pub min_qty: Decimal,
    /// Minimum order value in quote currency.
    pub min_notional: Decimal,
}

impl MarketRules {
    pub fn round_qty_down(&self, qty: Decimal) -> Decimal {
        round_down_to_step(qty, self.qty_step)
    }
    pub fn round_price_down(&self, p: Decimal) -> Decimal {
        round_down_to_step(p, self.price_tick)
    }
    pub fn round_price_up(&self, p: Decimal) -> Decimal {
        if self.price_tick <= Decimal::ZERO {
            return p;
        }
        let down = round_down_to_step(p, self.price_tick);
        if down == p {
            p
        } else {
            down + self.price_tick
        }
    }
}

pub fn round_down_to_step(v: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return v;
    }
    let n = (v / step).floor();
    (n * step).normalize()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrderRequest {
    pub client_id: String,
    pub pair: Pair,
    pub side: Side,
    pub qty: Decimal,
    pub limit_price: Decimal,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Resting on the book (post-only maker orders only).
    Open,
    Filled,
    PartiallyFilled,
    Cancelled,
    Rejected,
    /// We could not determine the outcome (timeout + status lookup failed).
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OrderResult {
    pub client_id: String,
    pub exchange_order_id: Option<String>,
    pub status: OrderStatus,
    pub filled_qty: Decimal,
    /// Average fill price (quote per base); zero if nothing filled.
    pub avg_price: Decimal,
    /// Fee paid, expressed in quote currency.
    pub fee_quote: Decimal,
}

impl OrderResult {
    pub fn nothing(client_id: &str, status: OrderStatus) -> Self {
        OrderResult {
            client_id: client_id.to_string(),
            exchange_order_id: None,
            status,
            filled_qty: Decimal::ZERO,
            avg_price: Decimal::ZERO,
            fee_quote: Decimal::ZERO,
        }
    }
    pub fn filled_quote(&self) -> Decimal {
        self.filled_qty * self.avg_price
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn pair_parse_normalises_case() {
        let p = Pair::parse("btc/usd").unwrap();
        assert_eq!(p.base, "BTC");
        assert_eq!(p.quote, "USD");
        assert_eq!(p.to_string(), "BTC/USD");
        assert!(Pair::parse("BTCUSD").is_err());
        assert!(Pair::parse("/USD").is_err());
    }

    #[test]
    fn rounding_down_to_step() {
        assert_eq!(round_down_to_step(dec!(1.23456), dec!(0.001)), dec!(1.234));
        assert_eq!(round_down_to_step(dec!(0.00099), dec!(0.001)), dec!(0));
        assert_eq!(round_down_to_step(dec!(5), dec!(0)), dec!(5));
        let r = MarketRules {
            qty_step: dec!(0.01),
            price_tick: dec!(0.5),
            min_qty: dec!(0),
            min_notional: dec!(0),
        };
        assert_eq!(r.round_price_up(dec!(100.1)), dec!(100.5));
        assert_eq!(r.round_price_up(dec!(100.5)), dec!(100.5));
        assert_eq!(r.round_price_down(dec!(100.9)), dec!(100.5));
    }

    #[test]
    fn exchange_id_roundtrip() {
        for e in ExchangeId::all() {
            assert_eq!(e.as_str().parse::<ExchangeId>().unwrap(), e);
        }
        assert!("mtgox".parse::<ExchangeId>().is_err());
    }
}
