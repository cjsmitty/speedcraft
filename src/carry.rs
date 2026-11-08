//! Futures basis carry ("cash and carry") on Coinbase Derivatives.
//!
//! Buy the coin on the spot market, sell a dated future on it, hold to
//! expiry: the premium the future trades at over spot is locked in on the day
//! you enter, net of fees. Price moves cancel out because the two legs are
//! opposite. What is left: the margin you must post for the short leg (a cash
//! requirement, not a cost), exchange risk, and the fees. For the
//! perpetual-style contracts (2089 expiry) the public feed does not expose the
//! funding rate, so only their basis is shown.

use crate::book::BookCache;
use crate::config::CarryConfig;
use crate::exchanges::{http_client, parse_dec};
use crate::types::{now_ms, ExchangeId, Pair};
use anyhow::{Context, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    pub product_id: String,
    pub underlying: String,
    pub display: String,
    pub contract_size: Decimal,
    pub expiry_ms: i64,
    /// 2089-expiry contracts are Coinbase's perpetual-style futures.
    pub perpetual_style: bool,
    pub price_increment: Decimal,
}

/// Parse the public products listing (product_type=FUTURE) into contracts for
/// the wanted underlyings.
pub fn parse_contracts(products: &Value, underlyings: &[String]) -> Vec<Contract> {
    let mut out = Vec::new();
    for p in products
        .get("products")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(f) = p.get("future_product_details") else {
            continue;
        };
        let root = f
            .get("contract_root_unit")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_uppercase();
        if !underlyings.iter().any(|u| u.eq_ignore_ascii_case(&root)) {
            continue;
        }
        if p.get("product_venue").and_then(Value::as_str) != Some("FCM") {
            continue;
        }
        if f.get("non_crypto")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let expiry = f
            .get("contract_expiry")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp_millis());
        let Some(expiry_ms) = expiry else { continue };
        let size = f
            .get("contract_size")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ONE);
        let inc = p
            .get("price_increment")
            .and_then(Value::as_str)
            .and_then(|s| s.parse::<Decimal>().ok())
            .unwrap_or(Decimal::ONE);
        let display = f
            .get("contract_display_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let perpetual_style = display.contains("PERP") || expiry_ms > 4_000_000_000_000;
        out.push(Contract {
            product_id: p
                .get("product_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            underlying: root,
            display,
            contract_size: size,
            expiry_ms,
            perpetual_style,
            price_increment: inc,
        });
    }
    out.sort_by(|a, b| {
        (a.underlying.clone(), a.expiry_ms).cmp(&(b.underlying.clone(), b.expiry_ms))
    });
    out
}

#[derive(Clone, Debug)]
pub struct CarryParams {
    pub notional: Decimal,
    pub spot_fee_bps: Decimal,
    pub futures_fee_per_contract: Decimal,
    pub initial_margin_pct: Decimal,
    pub min_annualized_pct: Decimal,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CarryQuote {
    pub product_id: String,
    pub underlying: String,
    pub display: String,
    pub perpetual_style: bool,
    pub days_to_expiry: f64,
    pub spot_ask: Decimal,
    pub future_bid: Decimal,
    pub future_ask: Decimal,
    pub contracts: u64,
    /// Spot dollars actually deployed (whole contracts).
    pub spot_notional: Decimal,
    /// Premium of the future over spot at entry, bps of spot.
    pub basis_bps: Decimal,
    pub gross_annualized_pct: f64,
    /// Locked profit in quote currency if held to expiry, after both fees.
    pub locked_profit: Decimal,
    pub fees_total: Decimal,
    pub net_annualized_pct: f64,
    /// Cash to post as margin for the short future, on top of the spot cost.
    pub margin_required: Decimal,
    /// Net profit over all capital tied up (spot + margin), annualized.
    pub return_on_capital_pct: f64,
    pub tradable: bool,
    pub updated_ms: i64,
}

/// The core math. `spot_ask` is where we buy, `future_bid` where we sell.
pub fn evaluate(
    c: &Contract,
    spot_ask: Decimal,
    future_bid: Decimal,
    future_ask: Decimal,
    now_ms_: i64,
    p: &CarryParams,
) -> Option<CarryQuote> {
    if spot_ask <= Decimal::ZERO || future_bid <= Decimal::ZERO {
        return None;
    }
    let days = (c.expiry_ms - now_ms_) as f64 / 86_400_000.0;
    if days <= 0.0 {
        return None;
    }
    // Whole contracts that fit the notional.
    let per_contract_notional = spot_ask * c.contract_size;
    let contracts = (p.notional / per_contract_notional)
        .floor()
        .to_u64()
        .unwrap_or(0);
    if contracts == 0 {
        return None;
    }
    let qty = c.contract_size * Decimal::from(contracts);
    let spot_cost = qty * spot_ask;
    let basis = future_bid - spot_ask;
    let basis_bps = basis / spot_ask * Decimal::from(10_000);
    let spot_fee = spot_cost * p.spot_fee_bps / Decimal::from(10_000);
    // Futures: fee per contract on entry; settlement at expiry has no taker fee.
    let fut_fee = p.futures_fee_per_contract * Decimal::from(contracts);
    let fees_total = spot_fee + fut_fee;
    let gross = basis * qty;
    let locked = gross - fees_total;
    let margin = spot_cost * p.initial_margin_pct / Decimal::from(100);
    let capital = spot_cost + margin;
    let ann = |profit: Decimal, base: Decimal| -> f64 {
        if base.is_zero() || c.perpetual_style {
            0.0
        } else {
            (profit / base).to_f64().unwrap_or(0.0) * 365.0 / days * 100.0
        }
    };
    let gross_ann = ann(gross, spot_cost);
    let net_ann = ann(locked, spot_cost);
    let roc = ann(locked, capital);
    Some(CarryQuote {
        product_id: c.product_id.clone(),
        underlying: c.underlying.clone(),
        display: c.display.clone(),
        perpetual_style: c.perpetual_style,
        days_to_expiry: (days * 10.0).round() / 10.0,
        spot_ask,
        future_bid,
        future_ask,
        contracts,
        spot_notional: spot_cost.round_dp(2),
        basis_bps: basis_bps.round_dp(2),
        gross_annualized_pct: (gross_ann * 100.0).round() / 100.0,
        locked_profit: locked.round_dp(2),
        fees_total: fees_total.round_dp(2),
        net_annualized_pct: (net_ann * 100.0).round() / 100.0,
        margin_required: margin.round_dp(2),
        return_on_capital_pct: (roc * 100.0).round() / 100.0,
        tradable: !c.perpetual_style
            && locked > Decimal::ZERO
            && net_ann >= p.min_annualized_pct.to_f64().unwrap_or(0.0),
        updated_ms: now_ms_,
    })
}

/// A paper position: entered at the quoted prices, settles at expiry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CarryPosition {
    pub product_id: String,
    pub underlying: String,
    pub opened_ms: i64,
    pub expiry_ms: i64,
    pub contracts: u64,
    pub qty: Decimal,
    pub spot_entry: Decimal,
    pub future_entry: Decimal,
    pub locked_profit: Decimal,
    pub margin_required: Decimal,
    /// Current mark-to-market of the pair (spot gain minus future loss), before fees.
    pub mark_pnl: Decimal,
    pub settled: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CarryState {
    pub quotes: Vec<CarryQuote>,
    pub positions: Vec<CarryPosition>,
    pub realised_pnl: Decimal,
    pub last_error: Option<String>,
    pub updated_ms: i64,
}

pub type SharedCarry = Arc<Mutex<CarryState>>;

async fn fetch_json(http: &reqwest::Client, url: &str) -> Result<Value> {
    Ok(http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

async fn best_bid_ask(http: &reqwest::Client, product_id: &str) -> Result<(Decimal, Decimal)> {
    let v = fetch_json(http, &format!("https://api.coinbase.com/api/v3/brokerage/market/product_book?product_id={product_id}&limit=1")).await?;
    let pb = v.get("pricebook").context("no pricebook")?;
    let bid = pb
        .get("bids")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|b| b.get("price"))
        .and_then(Value::as_str)
        .context("no bid")?;
    let ask = pb
        .get("asks")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|b| b.get("price"))
        .and_then(Value::as_str)
        .context("no ask")?;
    Ok((parse_dec(bid)?, parse_dec(ask)?))
}

/// Background monitor: discovers contracts, polls their books, updates the
/// shared state, and manages paper positions. Runs until `shutdown` flips.
pub async fn run(
    cfg: CarryConfig,
    spot_fee_bps: Decimal,
    book: Arc<BookCache>,
    state: SharedCarry,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let http = http_client();
    let params = CarryParams {
        notional: cfg.notional,
        spot_fee_bps,
        futures_fee_per_contract: cfg.futures_fee_per_contract,
        initial_margin_pct: cfg.initial_margin_pct,
        min_annualized_pct: cfg.min_annualized_pct,
    };
    let mut contracts: Vec<Contract> = Vec::new();
    let mut last_discovery = 0i64;
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.poll_secs.max(5)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            _ = tick.tick() => {}
        }
        let now = now_ms();
        if contracts.is_empty() || now - last_discovery > 6 * 3_600_000 {
            match fetch_json(&http, "https://api.coinbase.com/api/v3/brokerage/market/products?product_type=FUTURE&limit=250").await {
                Ok(v) => {
                    contracts = parse_contracts(&v, &cfg.underlyings);
                    last_discovery = now;
                    tracing::info!(n = contracts.len(), "carry: discovered futures contracts");
                }
                Err(e) => {
                    state.lock().unwrap().last_error = Some(format!("contract discovery failed: {e}"));
                    continue;
                }
            }
        }
        let mut quotes = Vec::new();
        let mut spot_cache: HashMap<String, Decimal> = HashMap::new();
        for c in &contracts {
            // Spot ask: from the live book if the pair is watched, else REST.
            let pair = Pair {
                base: c.underlying.clone(),
                quote: "USD".into(),
            };
            let spot_ask = if let Some(b) = book.get(ExchangeId::Coinbase, &pair) {
                b.ask
            } else if let Some(s) = spot_cache.get(&c.underlying) {
                *s
            } else {
                match best_bid_ask(&http, &format!("{}-USD", c.underlying)).await {
                    Ok((_, a)) => {
                        spot_cache.insert(c.underlying.clone(), a);
                        a
                    }
                    Err(_) => continue,
                }
            };
            let Ok((fb, fa)) = best_bid_ask(&http, &c.product_id).await else {
                continue;
            };
            if let Some(q) = evaluate(c, spot_ask, fb, fa, now, &params) {
                quotes.push(q);
            }
        }
        quotes.sort_by(|a, b| {
            b.net_annualized_pct
                .partial_cmp(&a.net_annualized_pct)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut st = state.lock().unwrap();
        // Paper positions: open the best tradable contract per underlying once; mark and settle.
        if cfg.paper_positions {
            for q in &quotes {
                if q.tradable
                    && !st
                        .positions
                        .iter()
                        .any(|p| p.underlying == q.underlying && !p.settled)
                {
                    let c = contracts
                        .iter()
                        .find(|c| c.product_id == q.product_id)
                        .unwrap();
                    st.positions.push(CarryPosition {
                        product_id: q.product_id.clone(),
                        underlying: q.underlying.clone(),
                        opened_ms: now,
                        expiry_ms: c.expiry_ms,
                        contracts: q.contracts,
                        qty: c.contract_size * Decimal::from(q.contracts),
                        spot_entry: q.spot_ask,
                        future_entry: q.future_bid,
                        locked_profit: q.locked_profit,
                        margin_required: q.margin_required,
                        mark_pnl: Decimal::ZERO,
                        settled: false,
                    });
                    tracing::info!(product = %q.product_id, locked = %q.locked_profit, "carry: opened paper position");
                }
            }
            for p in st.positions.iter_mut().filter(|p| !p.settled) {
                if let Some(q) = quotes.iter().find(|q| q.product_id == p.product_id) {
                    // Spot leg marks at bid (we would sell), future leg at ask (we would buy back).
                    let spot_bid = book
                        .get(
                            ExchangeId::Coinbase,
                            &Pair {
                                base: p.underlying.clone(),
                                quote: "USD".into(),
                            },
                        )
                        .map(|b| b.bid)
                        .unwrap_or(q.spot_ask);
                    p.mark_pnl =
                        ((spot_bid - p.spot_entry) + (p.future_entry - q.future_ask)) * p.qty;
                }
                if now >= p.expiry_ms {
                    p.settled = true;
                    p.mark_pnl = p.locked_profit;
                }
            }
            st.realised_pnl = st
                .positions
                .iter()
                .filter(|p| p.settled)
                .map(|p| p.locked_profit)
                .sum();
        }
        st.quotes = quotes;
        st.last_error = None;
        st.updated_ms = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;

    fn products() -> Value {
        json!({"products": [
            {"product_id": "BIT-24DEC26-CDE", "product_venue": "FCM", "price_increment": "5", "future_product_details": {"contract_root_unit": "BTC", "contract_size": "0.01", "contract_expiry": "2026-12-24T16:00:00Z", "contract_display_name": "BTC 24 DEC 26", "non_crypto": false}},
            {"product_id": "BIP-20DEC30-CDE", "product_venue": "FCM", "price_increment": "5", "future_product_details": {"contract_root_unit": "BTC", "contract_size": "0.01", "contract_expiry": "2089-12-30T16:00:00Z", "contract_display_name": "BTC PERP", "non_crypto": false}},
            {"product_id": "GOL-25NOV26-CDE", "product_venue": "FCM", "price_increment": "0.1", "future_product_details": {"contract_root_unit": "GOLD", "contract_size": "1", "contract_expiry": "2026-11-25T18:30:00Z", "contract_display_name": "GOLD", "non_crypto": true}},
            {"product_id": "ET-27NOV26-CDE", "product_venue": "FCM", "price_increment": "0.5", "future_product_details": {"contract_root_unit": "ETH", "contract_size": "0.1", "contract_expiry": "2026-11-27T16:00:00Z", "contract_display_name": "ETH 27 NOV 26", "non_crypto": false}}
        ]})
    }

    #[test]
    fn parses_only_wanted_crypto_contracts() {
        let cs = parse_contracts(&products(), &["BTC".into()]);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].product_id, "BIT-24DEC26-CDE");
        assert!(!cs[0].perpetual_style);
        assert!(cs[1].perpetual_style);
        assert_eq!(cs[0].contract_size, dec!(0.01));
        let cs = parse_contracts(&products(), &["eth".into(), "BTC".into()]);
        assert_eq!(cs.len(), 3);
    }

    #[test]
    fn basis_math_is_exact() {
        let c = parse_contracts(&products(), &["BTC".into()]).remove(0);
        let now = c.expiry_ms - 50 * 86_400_000; // 50 days to expiry
        let p = CarryParams {
            notional: dec!(1000),
            spot_fee_bps: dec!(60),
            futures_fee_per_contract: dec!(0.10),
            initial_margin_pct: dec!(35),
            min_annualized_pct: dec!(3),
        };
        // spot 75,000, future bid 75,600 → +0.8% basis; 1 contract of 0.01 BTC fits $1,000
        let q = evaluate(&c, dec!(75000), dec!(75600), dec!(75700), now, &p).unwrap();
        assert_eq!(q.contracts, 1);
        assert_eq!(q.basis_bps, dec!(80));
        // gross = 600 * 0.01 = 6.00; spot fee = 750 * 0.006 = 4.50; fut fee 0.10 → locked 1.40
        assert_eq!(q.locked_profit, dec!(1.40));
        assert_eq!(q.fees_total, dec!(4.60));
        assert_eq!(q.margin_required, dec!(262.50));
        // net annualized = 1.40/750 * 365/50 = 1.3627%
        assert!(
            (q.net_annualized_pct - 1.36).abs() < 0.02,
            "{}",
            q.net_annualized_pct
        );
        assert!((q.gross_annualized_pct - 5.84).abs() < 0.02);
        assert!(!q.tradable, "1.36% is below the 3% minimum");
        let p2 = CarryParams {
            spot_fee_bps: dec!(10),
            ..p.clone()
        };
        let q2 = evaluate(&c, dec!(75000), dec!(75600), dec!(75700), now, &p2).unwrap();
        assert_eq!(q2.locked_profit, dec!(5.15));
        assert!(q2.tradable);
        // Too small a notional for one contract → nothing.
        let p3 = CarryParams {
            notional: dec!(100),
            ..p
        };
        assert!(evaluate(&c, dec!(75000), dec!(75600), dec!(75700), now, &p3).is_none());
    }

    #[test]
    fn perpetual_style_is_never_marked_tradable() {
        let c = parse_contracts(&products(), &["BTC".into()]).remove(1);
        let p = CarryParams {
            notional: dec!(1000),
            spot_fee_bps: dec!(0),
            futures_fee_per_contract: dec!(0),
            initial_margin_pct: dec!(35),
            min_annualized_pct: dec!(0),
        };
        let q = evaluate(&c, dec!(75000), dec!(76000), dec!(76010), now_ms(), &p).unwrap();
        assert!(q.perpetual_style);
        assert!(!q.tradable);
        assert_eq!(q.net_annualized_pct, 0.0);
        assert_eq!(q.basis_bps, dec!(133.33));
    }
}
