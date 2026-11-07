//! Rank pairs in a quote recording by how much cross-exchange edge actually
//! existed, at several fee assumptions. This answers "which coin would have
//! made money" with data instead of opinion.

use crate::replay::QuoteRecord;
use crate::types::ExchangeId;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use std::collections::HashMap;

/// A fee scenario: taker on both legs (arbitrage) or maker on one leg plus
/// taker hedge on the other (market making).
#[derive(Clone, Debug, Serialize)]
pub struct FeeTier {
    pub name: String,
    pub taker_bps: HashMap<ExchangeId, f64>,
    pub maker_bps: HashMap<ExchangeId, f64>,
    pub buffer_bps: f64,
}

impl FeeTier {
    pub fn uniform(name: &str, taker: f64, maker: f64, buffer: f64) -> FeeTier {
        let mut t = HashMap::new();
        let mut m = HashMap::new();
        for e in ExchangeId::all() {
            t.insert(e, taker);
            m.insert(e, maker);
        }
        FeeTier {
            name: name.into(),
            taker_bps: t,
            maker_bps: m,
            buffer_bps: buffer,
        }
    }
    pub fn retail() -> FeeTier {
        let mut f = FeeTier::uniform("retail", 10.0, 10.0, 2.0);
        f.taker_bps.insert(ExchangeId::Coinbase, 60.0);
        f.maker_bps.insert(ExchangeId::Coinbase, 40.0);
        f.taker_bps.insert(ExchangeId::Kraken, 40.0);
        f.maker_bps.insert(ExchangeId::Kraken, 25.0);
        f.taker_bps.insert(ExchangeId::BinanceUs, 2.0);
        f.maker_bps.insert(ExchangeId::BinanceUs, 0.0);
        f
    }
    pub fn volume() -> FeeTier {
        let mut f = FeeTier::uniform("volume", 10.0, 5.0, 2.0);
        f.taker_bps.insert(ExchangeId::Coinbase, 25.0);
        f.maker_bps.insert(ExchangeId::Coinbase, 10.0);
        f.taker_bps.insert(ExchangeId::Kraken, 20.0);
        f.maker_bps.insert(ExchangeId::Kraken, 10.0);
        f.taker_bps.insert(ExchangeId::BinanceUs, 2.0);
        f.maker_bps.insert(ExchangeId::BinanceUs, 0.0);
        f
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PairStats {
    pub pair: String,
    pub samples: usize,
    /// Average quoted bid-ask spread on each venue, bps of mid.
    pub venue_spread_bps: HashMap<ExchangeId, f64>,
    /// Best directed gross cross (sell.bid - buy.ask) in bps: mean, p95, max.
    pub gross_mean_bps: f64,
    pub gross_p95_bps: f64,
    pub gross_max_bps: f64,
    /// Fraction of samples where the books crossed at all.
    pub crossed_frac: f64,
    /// Taker-taker arbitrage net edge (p95 and max) per fee tier.
    pub arb_net_p95: HashMap<String, f64>,
    pub arb_net_max: HashMap<String, f64>,
    /// Fraction of samples with positive taker-taker net per tier.
    pub arb_positive_frac: HashMap<String, f64>,
    /// Maker-on-one-venue, taker-hedge-on-the-other net edge (mean and p95) per tier.
    pub maker_net_mean: HashMap<String, f64>,
    pub maker_net_p95: HashMap<String, f64>,
    pub maker_positive_frac: HashMap<String, f64>,
}

#[derive(Clone, Copy)]
struct Q {
    bid: f64,
    ask: f64,
    ts: i64,
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let i = ((v.len() as f64 - 1.0) * p).round() as usize;
    v[i.min(v.len() - 1)]
}

/// Scan quotes (any order; sorted internally). `max_gap_ms` is how far apart
/// two venues' quotes may be to count as simultaneous.
pub fn scan(quotes: &[QuoteRecord], tiers: &[FeeTier], max_gap_ms: i64) -> Vec<PairStats> {
    let mut sorted: Vec<&QuoteRecord> = quotes.iter().collect();
    sorted.sort_by_key(|q| q.ts_ms);
    let mut last: HashMap<(String, ExchangeId), Q> = HashMap::new();
    // per pair: gross samples, per-tier arb net samples, per-tier maker net samples, venue spreads
    let mut gross: HashMap<String, Vec<f64>> = HashMap::new();
    let mut arb: HashMap<(String, String), Vec<f64>> = HashMap::new();
    let mut maker: HashMap<(String, String), Vec<f64>> = HashMap::new();
    let mut spreads: HashMap<(String, ExchangeId), (f64, usize)> = HashMap::new();

    for q in sorted {
        let (Some(bid), Some(ask)) = (q.bid.to_f64(), q.ask.to_f64()) else {
            continue;
        };
        if bid <= 0.0 || ask <= 0.0 || ask < bid {
            continue;
        }
        let e = spreads
            .entry((q.pair.clone(), q.exchange))
            .or_insert((0.0, 0));
        e.0 += (ask - bid) / ((ask + bid) / 2.0) * 1e4;
        e.1 += 1;
        last.insert(
            (q.pair.clone(), q.exchange),
            Q {
                bid,
                ask,
                ts: q.ts_ms,
            },
        );
        // Evaluate this pair across all venues with a fresh quote.
        let venues: Vec<(ExchangeId, Q)> = last
            .iter()
            .filter(|((p, _), v)| *p == q.pair && q.ts_ms - v.ts <= max_gap_ms)
            .map(|((_, e), v)| (*e, *v))
            .collect();
        if venues.len() < 2 {
            continue;
        }
        let mut best_gross = f64::NEG_INFINITY;
        let mut best_arb: HashMap<String, f64> = HashMap::new();
        let mut best_maker: HashMap<String, f64> = HashMap::new();
        for (be, b) in &venues {
            for (se, s) in &venues {
                if be == se {
                    continue;
                }
                // Taker-taker: buy at b.ask, sell at s.bid.
                let g = (s.bid - b.ask) / b.ask * 1e4;
                best_gross = best_gross.max(g);
                for t in tiers {
                    let net = g - t.taker_bps[be] - t.taker_bps[se] - t.buffer_bps;
                    let e = best_arb.entry(t.name.clone()).or_insert(f64::NEG_INFINITY);
                    *e = e.max(net);
                    // Maker on `be` (join its best bid), taker hedge selling on `se` at s.bid.
                    let gm_buy = (s.bid - b.bid) / b.bid * 1e4;
                    // Maker on `be` selling at its best ask, taker hedge buying on `se` at s.ask.
                    let gm_sell = (b.ask - s.ask) / s.ask * 1e4;
                    let net_m =
                        gm_buy.max(gm_sell) - t.maker_bps[be] - t.taker_bps[se] - t.buffer_bps;
                    let e = best_maker
                        .entry(t.name.clone())
                        .or_insert(f64::NEG_INFINITY);
                    *e = e.max(net_m);
                }
            }
        }
        gross.entry(q.pair.clone()).or_default().push(best_gross);
        for (t, v) in best_arb {
            arb.entry((q.pair.clone(), t)).or_default().push(v);
        }
        for (t, v) in best_maker {
            maker.entry((q.pair.clone(), t)).or_default().push(v);
        }
    }

    let mut out = Vec::new();
    for (pair, mut g) in gross {
        let n = g.len();
        let mut st = PairStats {
            pair: pair.clone(),
            samples: n,
            ..Default::default()
        };
        st.gross_mean_bps = g.iter().sum::<f64>() / n as f64;
        st.crossed_frac = g.iter().filter(|x| **x > 0.0).count() as f64 / n as f64;
        st.gross_p95_bps = pct(&mut g, 0.95);
        st.gross_max_bps = *g.last().unwrap();
        for ((p, e), (sum, c)) in &spreads {
            if *p == pair {
                st.venue_spread_bps.insert(*e, sum / *c as f64);
            }
        }
        for t in tiers {
            if let Some(v) = arb.get_mut(&(pair.clone(), t.name.clone())) {
                st.arb_positive_frac.insert(
                    t.name.clone(),
                    v.iter().filter(|x| **x > 0.0).count() as f64 / v.len() as f64,
                );
                st.arb_net_p95.insert(t.name.clone(), pct(v, 0.95));
                st.arb_net_max.insert(t.name.clone(), *v.last().unwrap());
            }
            if let Some(v) = maker.get_mut(&(pair.clone(), t.name.clone())) {
                st.maker_positive_frac.insert(
                    t.name.clone(),
                    v.iter().filter(|x| **x > 0.0).count() as f64 / v.len() as f64,
                );
                st.maker_net_mean
                    .insert(t.name.clone(), v.iter().sum::<f64>() / v.len() as f64);
                st.maker_net_p95.insert(t.name.clone(), pct(v, 0.95));
            }
        }
        out.push(st);
    }
    // Rank by the realistic question: maker-mode net at retail tier, p95.
    out.sort_by(|a, b| {
        let ka = a
            .maker_net_p95
            .get("retail")
            .copied()
            .unwrap_or(f64::NEG_INFINITY);
        let kb = b
            .maker_net_p95
            .get("retail")
            .copied()
            .unwrap_or(f64::NEG_INFINITY);
        kb.partial_cmp(&ka).unwrap()
    });
    out
}

pub fn render_table(stats: &[PairStats], tiers: &[FeeTier]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{:<9}{:>7}{:>9}{:>9}{:>9}{:>9}{:>8}",
        "pair", "n", "spreadCB", "spreadKR", "grossAvg", "grossP95", "cross%"
    ));
    for t in tiers {
        s.push_str(&format!(
            "{:>11}{:>11}{:>11}{:>9}",
            format!("arbP95:{}", &t.name[..3]),
            format!("arbMax:{}", &t.name[..3]),
            format!("mkrP95:{}", &t.name[..3]),
            format!("mkr+%:{}", &t.name[..3])
        ));
    }
    s.push('\n');
    for p in stats {
        s.push_str(&format!(
            "{:<9}{:>7}{:>9.1}{:>9.1}{:>9.1}{:>9.1}{:>7.0}%",
            p.pair,
            p.samples,
            p.venue_spread_bps
                .get(&ExchangeId::Coinbase)
                .copied()
                .unwrap_or(f64::NAN),
            p.venue_spread_bps
                .get(&ExchangeId::Kraken)
                .copied()
                .unwrap_or(f64::NAN),
            p.gross_mean_bps,
            p.gross_p95_bps,
            p.crossed_frac * 100.0
        ));
        for t in tiers {
            s.push_str(&format!(
                "{:>11.1}{:>11.1}{:>11.1}{:>8.0}%",
                p.arb_net_p95.get(&t.name).copied().unwrap_or(f64::NAN),
                p.arb_net_max.get(&t.name).copied().unwrap_or(f64::NAN),
                p.maker_net_p95.get(&t.name).copied().unwrap_or(f64::NAN),
                p.maker_positive_frac.get(&t.name).copied().unwrap_or(0.0) * 100.0
            ));
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn q(ts: i64, ex: ExchangeId, pair: &str, bid: f64, ask: f64) -> QuoteRecord {
        QuoteRecord {
            ts_ms: ts,
            exchange: ex,
            pair: pair.into(),
            bid: rust_decimal::Decimal::try_from(bid).unwrap(),
            bid_qty: dec!(1),
            ask: rust_decimal::Decimal::try_from(ask).unwrap(),
            ask_qty: dec!(1),
        }
    }

    #[test]
    fn scan_computes_gross_and_net_edges() {
        let quotes = vec![
            q(0, ExchangeId::Coinbase, "SOL/USD", 100.0, 100.1),
            q(10, ExchangeId::Kraken, "SOL/USD", 101.0, 101.2), // sell kraken bid 101 vs buy cb ask 100.1 → +89.9 bps
            q(20, ExchangeId::Kraken, "SOL/USD", 100.0, 100.1), // flat
            q(5000, ExchangeId::Coinbase, "SOL/USD", 100.0, 100.1), // kraken quote now 4980ms old → still within 5000
            q(20000, ExchangeId::Coinbase, "SOL/USD", 100.0, 100.1), // too old → ignored
        ];
        let tiers = vec![FeeTier::uniform("zero", 0.0, 0.0, 0.0), FeeTier::retail()];
        let st = scan(&quotes, &tiers, 5000);
        assert_eq!(st.len(), 1);
        let p = &st[0];
        assert_eq!(p.pair, "SOL/USD");
        assert_eq!(p.samples, 3);
        assert!(
            (p.gross_max_bps - 89.91).abs() < 0.05,
            "{}",
            p.gross_max_bps
        );
        assert!((p.arb_net_max["zero"] - 89.91).abs() < 0.05);
        // retail: 60 + 40 + 2 = 102 bps of cost → -12.1
        assert!(
            (p.arb_net_max["retail"] - (89.91 - 102.0)).abs() < 0.05,
            "{}",
            p.arb_net_max["retail"]
        );
        // maker: best direction is selling at Kraken's ask 101.2 and hedging a buy on Coinbase at 100.1:
        // (101.2-100.1)/100.1 = 109.9 bps gross, minus Kraken maker 25 + Coinbase taker 60 + 2 = 22.9
        assert!(
            (p.maker_net_p95["retail"] - 22.9).abs() < 0.2,
            "{}",
            p.maker_net_p95["retail"]
        );
        assert!(
            p.venue_spread_bps[&ExchangeId::Coinbase] > 9.0
                && p.venue_spread_bps[&ExchangeId::Coinbase] < 11.0
        );
        let table = render_table(&st, &tiers);
        assert!(table.contains("SOL/USD"));
    }
}
