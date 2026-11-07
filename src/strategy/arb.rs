//! Pure arbitrage math. No I/O, no time, fully unit-tested.
//!
//! An opportunity exists when, for some ordered pair of exchanges (buy on A,
//! sell on B), the quantity we can actually trade yields positive profit
//! after taker fees on both legs and a slippage buffer.

use crate::types::{ExchangeId, Fees, MarketRules, Pair, TopOfBook};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct StrategyParams {
    pub min_edge_bps: Decimal,
    pub min_profit_quote: Decimal,
    pub slippage_buffer_bps: Decimal,
    pub depth_fraction: Decimal,
    pub max_notional_quote: Decimal,
}

/// Balances available for trading: quote per exchange, base per exchange.
#[derive(Clone, Debug, Default)]
pub struct Balances {
    pub quote: HashMap<ExchangeId, Decimal>,
    pub base: HashMap<ExchangeId, Decimal>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Opportunity {
    pub pair: Pair,
    pub buy_exchange: ExchangeId,
    pub sell_exchange: ExchangeId,
    pub buy_price: Decimal,
    pub sell_price: Decimal,
    pub qty: Decimal,
    pub gross_edge_bps: Decimal,
    pub net_edge_bps: Decimal,
    pub expected_profit_quote: Decimal,
    pub buy_fee_quote: Decimal,
    pub sell_fee_quote: Decimal,
}

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

fn bps(num: Decimal, den: Decimal) -> Decimal {
    if den.is_zero() {
        Decimal::ZERO
    } else {
        num / den * BPS
    }
}

/// Evaluate one directed pair: buy at `buy.ask`, sell at `sell.bid`.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_leg_pair(
    buy: &TopOfBook,
    sell: &TopOfBook,
    buy_fees: &Fees,
    sell_fees: &Fees,
    buy_rules: &MarketRules,
    sell_rules: &MarketRules,
    balances: &Balances,
    p: &StrategyParams,
) -> Option<Opportunity> {
    if buy.exchange == sell.exchange || buy.pair != sell.pair {
        return None;
    }
    if !buy.is_sane() || !sell.is_sane() {
        return None;
    }
    let gross = sell.bid - buy.ask;
    if gross <= Decimal::ZERO {
        return None;
    }

    // Size: a fraction of displayed depth on both sides, capped by notional
    // and by what we actually hold on each exchange.
    let mut qty = buy.ask_qty.min(sell.bid_qty) * p.depth_fraction;
    qty = qty.min(p.max_notional_quote / buy.ask);
    let quote_avail = balances
        .quote
        .get(&buy.exchange)
        .copied()
        .unwrap_or(Decimal::ZERO);
    let base_avail = balances
        .base
        .get(&sell.exchange)
        .copied()
        .unwrap_or(Decimal::ZERO);
    // Buying costs ask * (1 + fee); keep a little headroom.
    let buy_cost_per_unit = buy.ask * (Decimal::ONE + buy_fees.taker_rate());
    if buy_cost_per_unit <= Decimal::ZERO {
        return None;
    }
    qty = qty.min(quote_avail / buy_cost_per_unit);
    qty = qty.min(base_avail);

    // Round to the coarser of the two exchanges' quantity steps so the same
    // quantity is valid on both.
    let step = buy_rules.qty_step.max(sell_rules.qty_step);
    qty = crate::types::round_down_to_step(qty, step);
    if qty <= Decimal::ZERO {
        return None;
    }
    if qty < buy_rules.min_qty || qty < sell_rules.min_qty {
        return None;
    }
    let buy_notional = qty * buy.ask;
    let sell_notional = qty * sell.bid;
    if buy_notional < buy_rules.min_notional || sell_notional < sell_rules.min_notional {
        return None;
    }

    let buy_fee_quote = buy_notional * buy_fees.taker_rate();
    let sell_fee_quote = sell_notional * sell_fees.taker_rate();
    let slippage_quote = buy_notional * p.slippage_buffer_bps / BPS;
    let expected_profit =
        sell_notional - buy_notional - buy_fee_quote - sell_fee_quote - slippage_quote;
    let net_edge_bps = bps(expected_profit, buy_notional);
    let gross_edge_bps = bps(gross, buy.ask);

    if net_edge_bps < p.min_edge_bps || expected_profit < p.min_profit_quote {
        return None;
    }

    Some(Opportunity {
        pair: buy.pair.clone(),
        buy_exchange: buy.exchange,
        sell_exchange: sell.exchange,
        buy_price: buy.ask,
        sell_price: sell.bid,
        qty,
        gross_edge_bps: gross_edge_bps.round_dp(3),
        net_edge_bps: net_edge_bps.round_dp(3),
        expected_profit_quote: expected_profit.round_dp(6),
        buy_fee_quote: buy_fee_quote.round_dp(6),
        sell_fee_quote: sell_fee_quote.round_dp(6),
    })
}

/// Scan every ordered exchange pair for one canonical pair and return the
/// most profitable opportunity, if any clears the thresholds.
pub fn find_best_opportunity(
    books: &[TopOfBook],
    fees: &HashMap<ExchangeId, Fees>,
    rules: &HashMap<(ExchangeId, Pair), MarketRules>,
    balances: &Balances,
    p: &StrategyParams,
) -> Option<Opportunity> {
    let mut best: Option<Opportunity> = None;
    for buy in books {
        for sell in books {
            if buy.exchange == sell.exchange {
                continue;
            }
            let (Some(bf), Some(sf)) = (fees.get(&buy.exchange), fees.get(&sell.exchange)) else {
                continue;
            };
            let (Some(br), Some(sr)) = (
                rules.get(&(buy.exchange, buy.pair.clone())),
                rules.get(&(sell.exchange, sell.pair.clone())),
            ) else {
                continue;
            };
            if let Some(op) = evaluate_leg_pair(buy, sell, bf, sf, br, sr, balances, p) {
                let better = match &best {
                    None => true,
                    Some(b) => op.expected_profit_quote > b.expected_profit_quote,
                };
                if better {
                    best = Some(op);
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::time::Instant;

    fn tob(ex: ExchangeId, bid: Decimal, ask: Decimal, qty: Decimal) -> TopOfBook {
        TopOfBook {
            exchange: ex,
            pair: Pair::parse("BTC/USD").unwrap(),
            bid,
            bid_qty: qty,
            ask,
            ask_qty: qty,
            received_at: Instant::now(),
            received_ms: 0,
        }
    }
    fn fees(bps: Decimal) -> Fees {
        Fees {
            maker_bps: bps,
            taker_bps: bps,
        }
    }
    fn rules() -> MarketRules {
        MarketRules {
            qty_step: dec!(0.0001),
            price_tick: dec!(0.01),
            min_qty: dec!(0.0001),
            min_notional: dec!(1),
        }
    }
    fn params() -> StrategyParams {
        StrategyParams {
            min_edge_bps: dec!(5),
            min_profit_quote: dec!(0.5),
            slippage_buffer_bps: dec!(2),
            depth_fraction: dec!(0.5),
            max_notional_quote: dec!(500),
        }
    }
    fn balances(q: Decimal, b: Decimal) -> Balances {
        let mut bal = Balances::default();
        for e in ExchangeId::all() {
            bal.quote.insert(e, q);
            bal.base.insert(e, b);
        }
        bal
    }

    #[test]
    fn no_opportunity_when_books_do_not_cross() {
        let a = tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1));
        let b = tob(ExchangeId::Kraken, dec!(99.5), dec!(100.5), dec!(1));
        let op = evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(1e6), dec!(10)),
            &params(),
        );
        assert!(op.is_none());
    }

    #[test]
    fn fees_eat_a_small_gross_edge() {
        // 0.1% gross edge, but 0.6% + 0.4% taker fees.
        let a = tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1));
        let b = tob(ExchangeId::Kraken, dec!(100.10), dec!(100.2), dec!(1));
        let op = evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(60)),
            &fees(dec!(40)),
            &rules(),
            &rules(),
            &balances(dec!(1e6), dec!(10)),
            &params(),
        );
        assert!(op.is_none(), "retail fees must kill a 10 bps edge");
    }

    #[test]
    fn large_gross_edge_with_low_fees_is_taken_and_numbers_are_exact() {
        // Buy at 100 on Coinbase, sell at 101 on Kraken; 10 bps fees each side.
        let a = tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(2));
        let b = tob(ExchangeId::Kraken, dec!(101), dec!(102), dec!(2));
        let op = evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(1e6), dec!(10)),
            &params(),
        )
        .unwrap();
        // depth: min(2,2)*0.5 = 1; notional cap 500/100 = 5; so qty = 1
        assert_eq!(op.qty, dec!(1));
        assert_eq!(op.buy_fee_quote, dec!(0.1));
        assert_eq!(op.sell_fee_quote, dec!(0.101));
        // profit = 101 - 100 - 0.1 - 0.101 - (100 * 2/10000 = 0.02) = 0.779
        assert_eq!(op.expected_profit_quote, dec!(0.779));
        assert_eq!(op.net_edge_bps, dec!(77.9));
        assert_eq!(op.gross_edge_bps, dec!(100));
        assert_eq!(op.buy_exchange, ExchangeId::Coinbase);
        assert_eq!(op.sell_exchange, ExchangeId::Kraken);
    }

    #[test]
    fn size_is_capped_by_notional_depth_and_balances() {
        let a = tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(100));
        let b = tob(ExchangeId::Kraken, dec!(102), dec!(103), dec!(100));
        // notional cap 500 → 5 units.
        let op = evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(1e6), dec!(10)),
            &params(),
        )
        .unwrap();
        assert_eq!(op.qty, dec!(5));
        // base balance cap 0.5 on the selling exchange.
        let op = evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(1e6), dec!(0.5)),
            &params(),
        )
        .unwrap();
        assert_eq!(op.qty, dec!(0.5));
        // quote balance cap: 100.1 USD buys floor(100.1 / (100*1.001)) = 1.0 units
        let op = evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(100.1), dec!(10)),
            &params(),
        )
        .unwrap();
        assert_eq!(op.qty, dec!(1));
        // zero balance → nothing.
        assert!(evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(0), dec!(10)),
            &params()
        )
        .is_none());
    }

    #[test]
    fn qty_rounds_down_to_coarser_step_and_respects_minimums() {
        let a = tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(0.12345));
        let b = tob(ExchangeId::Kraken, dec!(102), dec!(103), dec!(0.12345));
        let coarse = MarketRules {
            qty_step: dec!(0.01),
            ..rules()
        };
        let small = StrategyParams {
            min_profit_quote: dec!(0.01),
            ..params()
        };
        let op = evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &coarse,
            &balances(dec!(1e6), dec!(10)),
            &small,
        )
        .unwrap();
        // 0.12345*0.5 = 0.061725 → 0.06
        assert_eq!(op.qty, dec!(0.06));
        let big_min = MarketRules {
            min_notional: dec!(1000),
            ..rules()
        };
        assert!(evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &big_min,
            &rules(),
            &balances(dec!(1e6), dec!(10)),
            &params()
        )
        .is_none());
    }

    #[test]
    fn min_profit_filters_dust() {
        let a = tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(0.001));
        let b = tob(ExchangeId::Kraken, dec!(102), dec!(103), dec!(0.001));
        // qty 0.0005 → profit ≈ 0.001; below 0.5 min profit.
        assert!(evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(1e6), dec!(10)),
            &params()
        )
        .is_none());
    }

    #[test]
    fn insane_books_are_ignored() {
        let mut a = tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1));
        a.ask = dec!(-1);
        let b = tob(ExchangeId::Kraken, dec!(102), dec!(103), dec!(1));
        assert!(evaluate_leg_pair(
            &a,
            &b,
            &fees(dec!(10)),
            &fees(dec!(10)),
            &rules(),
            &rules(),
            &balances(dec!(1e6), dec!(10)),
            &params()
        )
        .is_none());
    }

    #[test]
    fn find_best_picks_the_direction_with_the_most_profit() {
        let books = vec![
            tob(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1)),
            tob(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1)),
            tob(ExchangeId::BinanceUs, dec!(103), dec!(104), dec!(1)),
        ];
        let mut f = HashMap::new();
        for e in ExchangeId::all() {
            f.insert(e, fees(dec!(10)));
        }
        let mut r = HashMap::new();
        for e in ExchangeId::all() {
            r.insert((e, Pair::parse("BTC/USD").unwrap()), rules());
        }
        let small = StrategyParams {
            min_profit_quote: dec!(0.01),
            ..params()
        };
        let op =
            find_best_opportunity(&books, &f, &r, &balances(dec!(1e6), dec!(10)), &small).unwrap();
        assert_eq!(op.buy_exchange, ExchangeId::Coinbase);
        assert_eq!(op.sell_exchange, ExchangeId::BinanceUs);
        // With the default $0.50 minimum profit only the 3% spread qualifies.
        let op = find_best_opportunity(&books, &f, &r, &balances(dec!(1e6), dec!(10)), &params())
            .unwrap();
        assert_eq!(op.sell_exchange, ExchangeId::BinanceUs);
        // missing fees for an exchange → that exchange is skipped, not panicked.
        f.remove(&ExchangeId::BinanceUs);
        let op =
            find_best_opportunity(&books, &f, &r, &balances(dec!(1e6), dec!(10)), &small).unwrap();
        assert_eq!(op.sell_exchange, ExchangeId::Kraken);
    }
}
