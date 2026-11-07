//! Maker-mode math: where to rest post-only quotes on the "quote" venue so
//! that, if filled, an immediate taker hedge on the "hedge" venue still nets
//! the target edge after both fees. Pure functions, no I/O.
//!
//! Buying side: we post a bid on the quote venue. If it fills we sell on the
//! hedge venue at its bid (taker). So our bid must satisfy
//!   hedge.bid * (1 - taker_hedge) >= our_bid * (1 + maker_quote) * (1 + edge)
//! Selling side mirrors it against hedge.ask.

use crate::types::{Fees, MarketRules, Side, TopOfBook};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

#[derive(Clone, Debug)]
pub struct MakerParams {
    /// Net edge we require per round trip, in bps of notional.
    pub target_edge_bps: Decimal,
    /// Extra margin for hedge slippage, bps.
    pub slippage_buffer_bps: Decimal,
    /// Size of each resting quote, in quote currency.
    pub quote_notional: Decimal,
    /// Do not accumulate more than this much base inventory (either sign),
    /// in quote-currency terms, on the quote venue.
    pub max_inventory_notional: Decimal,
    /// Re-post only when the desired price moves by more than this, bps.
    pub requote_bps: Decimal,
    /// Never post a bid above the venue's best bid * (1 + this), bps: we join
    /// the book or improve it slightly, we do not chase.
    pub max_improve_bps: Decimal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesiredQuote {
    pub side: Side,
    pub price: Decimal,
    pub qty: Decimal,
    /// Expected net edge in bps if filled and hedged at the current hedge book.
    pub expected_edge_bps: Decimal,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DesiredQuotes {
    pub bid: Option<DesiredQuote>,
    pub ask: Option<DesiredQuote>,
}

fn rate(bps: Decimal) -> Decimal {
    bps / BPS
}

/// Balances that bound quote sizes: what the quote venue can pay/deliver and
/// what the hedge venue can sell/buy against a fill.
#[derive(Clone, Copy, Debug, Default)]
pub struct MakerBalances {
    pub quote_venue_quote: Decimal,
    pub quote_venue_base: Decimal,
    pub hedge_venue_quote: Decimal,
    pub hedge_venue_base: Decimal,
}

/// Compute the two desired resting quotes. `inventory_base` is our current net
/// base position on the quote venue from maker fills (positive = long).
#[allow(clippy::too_many_arguments)]
pub fn desired_quotes(
    quote_book: &TopOfBook,
    hedge_book: &TopOfBook,
    quote_fees: &Fees,
    hedge_fees: &Fees,
    quote_rules: &MarketRules,
    inventory_base: Decimal,
    bal: MakerBalances,
    p: &MakerParams,
) -> DesiredQuotes {
    let quote_balance_quote = bal.quote_venue_quote;
    let quote_balance_base = bal.quote_venue_base;
    if !quote_book.is_sane() || !hedge_book.is_sane() {
        return DesiredQuotes::default();
    }
    let maker = rate(quote_fees.maker_bps);
    let taker = rate(hedge_fees.taker_bps);
    let edge = rate(p.target_edge_bps + p.slippage_buffer_bps);
    let mid = (quote_book.bid + quote_book.ask) / Decimal::TWO;
    let inv_notional = inventory_base * mid;

    // ---- bid (we buy on quote venue, hedge by selling on hedge venue) ----
    let mut bid = None;
    if inv_notional < p.max_inventory_notional {
        // Max price we can pay: hedge.bid*(1-taker) / ((1+maker)(1+edge))
        let max_bid = hedge_book.bid * (Decimal::ONE - taker)
            / ((Decimal::ONE + maker) * (Decimal::ONE + edge));
        // Never cross the quote venue's ask (post-only would reject) and do not
        // improve the best bid by more than max_improve_bps.
        let cap = quote_book.bid * (Decimal::ONE + rate(p.max_improve_bps));
        let mut px = max_bid.min(cap);
        if px >= quote_book.ask {
            px = quote_book.ask - quote_rules.price_tick;
        }
        px = quote_rules.round_price_down(px);
        if px > Decimal::ZERO {
            let mut qty = quote_rules.round_qty_down(p.quote_notional / px);
            // Cannot spend more quote than we have (with fee headroom).
            let affordable =
                quote_rules.round_qty_down(quote_balance_quote / (px * (Decimal::ONE + maker)));
            qty = qty.min(affordable);
            // A fill is hedged by SELLING on the hedge venue: it must hold the base.
            qty = qty.min(quote_rules.round_qty_down(bal.hedge_venue_base));
            if qty >= quote_rules.min_qty
                && qty * px >= quote_rules.min_notional
                && qty > Decimal::ZERO
            {
                let net = hedge_book.bid * (Decimal::ONE - taker) - px * (Decimal::ONE + maker);
                let edge_bps = (net / px * BPS).round_dp(2);
                bid = Some(DesiredQuote {
                    side: Side::Buy,
                    price: px,
                    qty,
                    expected_edge_bps: edge_bps,
                });
            }
        }
    }

    // ---- ask (we sell on quote venue, hedge by buying on hedge venue) ----
    let mut ask = None;
    if inv_notional > -p.max_inventory_notional {
        let min_ask = hedge_book.ask * (Decimal::ONE + taker) * (Decimal::ONE + edge)
            / (Decimal::ONE - maker);
        let floor = quote_book.ask * (Decimal::ONE - rate(p.max_improve_bps));
        let mut px = min_ask.max(floor);
        if px <= quote_book.bid {
            px = quote_book.bid + quote_rules.price_tick;
        }
        px = quote_rules.round_price_up(px);
        if px > Decimal::ZERO {
            let mut qty = quote_rules.round_qty_down(p.quote_notional / px);
            qty = qty.min(quote_rules.round_qty_down(quote_balance_base));
            // A fill is hedged by BUYING on the hedge venue: it must hold the quote.
            let hedge_affordable = quote_rules
                .round_qty_down(bal.hedge_venue_quote / (hedge_book.ask * (Decimal::ONE + taker)));
            qty = qty.min(hedge_affordable);
            if qty >= quote_rules.min_qty
                && qty * px >= quote_rules.min_notional
                && qty > Decimal::ZERO
            {
                let net = px * (Decimal::ONE - maker) - hedge_book.ask * (Decimal::ONE + taker);
                let edge_bps = (net / px * BPS).round_dp(2);
                ask = Some(DesiredQuote {
                    side: Side::Sell,
                    price: px,
                    qty,
                    expected_edge_bps: edge_bps,
                });
            }
        }
    }
    DesiredQuotes { bid, ask }
}

/// Should a resting order at `current` be replaced by `desired`?
pub fn needs_requote(
    current: Option<&DesiredQuote>,
    desired: Option<&DesiredQuote>,
    requote_bps: Decimal,
) -> bool {
    match (current, desired) {
        (None, None) => false,
        (None, Some(_)) | (Some(_), None) => true,
        (Some(c), Some(d)) => {
            if c.qty != d.qty {
                return true;
            }
            let diff = (c.price - d.price).abs() / c.price * BPS;
            diff > requote_bps
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExchangeId, Pair};
    use rust_decimal_macros::dec;
    use std::time::Instant;

    fn tob(ex: ExchangeId, bid: Decimal, ask: Decimal) -> TopOfBook {
        TopOfBook {
            exchange: ex,
            pair: Pair::parse("ATOM/USD").unwrap(),
            bid,
            bid_qty: dec!(100),
            ask,
            ask_qty: dec!(100),
            received_at: Instant::now(),
            received_ms: 0,
        }
    }
    fn rules() -> MarketRules {
        MarketRules {
            qty_step: dec!(0.01),
            price_tick: dec!(0.0001),
            min_qty: dec!(0.1),
            min_notional: dec!(1),
        }
    }
    fn params() -> MakerParams {
        MakerParams {
            target_edge_bps: dec!(5),
            slippage_buffer_bps: dec!(2),
            quote_notional: dec!(100),
            max_inventory_notional: dec!(500),
            requote_bps: dec!(1),
            max_improve_bps: dec!(2),
        }
    }
    fn fees(m: Decimal, t: Decimal) -> Fees {
        Fees {
            maker_bps: m,
            taker_bps: t,
        }
    }
    /// Same balances on both venues.
    fn bal(quote: Decimal, base: Decimal) -> MakerBalances {
        MakerBalances {
            quote_venue_quote: quote,
            quote_venue_base: base,
            hedge_venue_quote: quote,
            hedge_venue_base: base,
        }
    }

    #[test]
    fn quotes_sit_behind_the_hedge_price_by_fees_and_edge() {
        // Quote venue (Binance.US, maker 0) and hedge venue (Kraken, taker 40).
        let q = tob(ExchangeId::BinanceUs, dec!(1.3800), dec!(1.3900));
        let h = tob(ExchangeId::Kraken, dec!(1.3900), dec!(1.3910));
        let d = desired_quotes(
            &q,
            &h,
            &fees(dec!(0), dec!(2)),
            &fees(dec!(25), dec!(40)),
            &rules(),
            dec!(0),
            bal(dec!(10000), dec!(1000)),
            &params(),
        );
        let bid = d.bid.unwrap();
        // max bid = 1.39*(1-0.004)/(1*(1+0.0007)) = 1.38347... capped at 1.38*(1+0.0002)=1.380276 → rounded down 1.3802
        assert_eq!(bid.price, dec!(1.3802));
        assert_eq!(bid.qty, dec!(72.45)); // 100 / 1.3802 = 72.453 rounded down to 0.01
                                          // edge if filled: hedge 1.39*(1-0.004) - 1.3802 = 1.38444 - 1.3802 = 0.00424 → 30.7 bps
        assert!(
            bid.expected_edge_bps > dec!(30) && bid.expected_edge_bps < dec!(31),
            "{}",
            bid.expected_edge_bps
        );
        let ask = d.ask.unwrap();
        // min ask = 1.391*(1.004)*(1.0007)/1 = 1.39754...; floor = 1.39*(1-0.0002) = 1.389722 → px = 1.3976 (rounded up)
        assert_eq!(ask.price, dec!(1.3976));
        assert!(
            ask.expected_edge_bps >= dec!(7),
            "{}",
            ask.expected_edge_bps
        );
        assert_eq!(ask.side, Side::Sell);
    }

    #[test]
    fn expensive_hedge_means_bid_far_below_market_but_still_posted() {
        // Quote on Coinbase (maker 40), hedge on Kraken (taker 40): bid sits ~87 bps under the hedge bid.
        let q = tob(ExchangeId::Coinbase, dec!(100.00), dec!(100.02));
        let h = tob(ExchangeId::Kraken, dec!(100.00), dec!(100.02));
        let d = desired_quotes(
            &q,
            &h,
            &fees(dec!(40), dec!(60)),
            &fees(dec!(25), dec!(40)),
            &rules(),
            dec!(0),
            bal(dec!(10000), dec!(10)),
            &params(),
        );
        let bid = d.bid.unwrap();
        assert!(
            bid.price < dec!(99.2) && bid.price > dec!(99.0),
            "{}",
            bid.price
        );
        assert!(bid.expected_edge_bps >= dec!(7));
    }

    #[test]
    fn inventory_limits_disable_one_side() {
        let q = tob(ExchangeId::BinanceUs, dec!(1.38), dec!(1.39));
        let h = tob(ExchangeId::Kraken, dec!(1.39), dec!(1.391));
        // Long 400 base ≈ 554 notional > 500 → no more bids, asks still allowed.
        let d = desired_quotes(
            &q,
            &h,
            &fees(dec!(0), dec!(2)),
            &fees(dec!(25), dec!(40)),
            &rules(),
            dec!(400),
            bal(dec!(10000), dec!(1000)),
            &params(),
        );
        assert!(d.bid.is_none());
        assert!(d.ask.is_some());
        let d = desired_quotes(
            &q,
            &h,
            &fees(dec!(0), dec!(2)),
            &fees(dec!(25), dec!(40)),
            &rules(),
            dec!(-400),
            bal(dec!(10000), dec!(1000)),
            &params(),
        );
        assert!(d.bid.is_some());
        assert!(d.ask.is_none());
    }

    #[test]
    fn balances_cap_quote_sizes_and_tiny_sizes_are_dropped() {
        let q = tob(ExchangeId::BinanceUs, dec!(1.38), dec!(1.39));
        let h = tob(ExchangeId::Kraken, dec!(1.39), dec!(1.391));
        // Quote venue is poor (5 quote, 0.05 base); hedge venue is rich.
        let b = MakerBalances {
            quote_venue_quote: dec!(5),
            quote_venue_base: dec!(0.05),
            hedge_venue_quote: dec!(10000),
            hedge_venue_base: dec!(1000),
        };
        let d = desired_quotes(
            &q,
            &h,
            &fees(dec!(0), dec!(2)),
            &fees(dec!(25), dec!(40)),
            &rules(),
            dec!(0),
            b,
            &params(),
        );
        assert_eq!(d.bid.unwrap().qty, dec!(3.62)); // 5 / 1.3802
        assert!(d.ask.is_none(), "0.05 base is below min_qty 0.1");
    }

    #[test]
    fn hedge_venue_balances_cap_sizes() {
        let q = tob(ExchangeId::BinanceUs, dec!(1.38), dec!(1.39));
        let h = tob(ExchangeId::Kraken, dec!(1.39), dec!(1.391));
        // Hedge venue holds only 2 base and 3 quote: bid ≤ 2, ask ≤ 3/(1.391*1.004) ≈ 2.14.
        let b = MakerBalances {
            quote_venue_quote: dec!(10000),
            quote_venue_base: dec!(1000),
            hedge_venue_quote: dec!(3),
            hedge_venue_base: dec!(2),
        };
        let d = desired_quotes(
            &q,
            &h,
            &fees(dec!(0), dec!(2)),
            &fees(dec!(25), dec!(40)),
            &rules(),
            dec!(0),
            b,
            &params(),
        );
        assert_eq!(d.bid.unwrap().qty, dec!(2));
        assert_eq!(d.ask.unwrap().qty, dec!(2.14));
    }

    #[test]
    fn never_crosses_the_book() {
        // Hedge venue far above quote venue: naive max bid would exceed quote ask.
        let q = tob(ExchangeId::BinanceUs, dec!(1.00), dec!(1.001));
        let h = tob(ExchangeId::Kraken, dec!(1.10), dec!(1.101));
        let d = desired_quotes(
            &q,
            &h,
            &fees(dec!(0), dec!(2)),
            &fees(dec!(25), dec!(40)),
            &rules(),
            dec!(0),
            bal(dec!(10000), dec!(1000)),
            &params(),
        );
        let bid = d.bid.unwrap();
        assert!(bid.price < q.ask);
        assert!(bid.price <= dec!(1.0002)); // join/improve cap
    }

    #[test]
    fn requote_logic() {
        let a = DesiredQuote {
            side: Side::Buy,
            price: dec!(100),
            qty: dec!(1),
            expected_edge_bps: dec!(5),
        };
        let b = DesiredQuote {
            side: Side::Buy,
            price: dec!(100.005),
            qty: dec!(1),
            expected_edge_bps: dec!(5),
        };
        let c = DesiredQuote {
            side: Side::Buy,
            price: dec!(100.5),
            qty: dec!(1),
            expected_edge_bps: dec!(5),
        };
        assert!(!needs_requote(Some(&a), Some(&b), dec!(1)));
        assert!(needs_requote(Some(&a), Some(&c), dec!(1)));
        assert!(needs_requote(None, Some(&a), dec!(1)));
        assert!(needs_requote(Some(&a), None, dec!(1)));
        assert!(!needs_requote(None, None, dec!(1)));
    }
}
