//! Kraken Spot.
//!
//! REST base: https://api.kraken.com. Private calls are form-encoded POSTs
//! with headers `API-Key` and `API-Sign`, where
//! `API-Sign = base64(HMAC-SHA512(base64decode(secret), path + SHA256(nonce + postdata)))`
//! and `path` starts at `/0/private/...`. Market data: wss://ws.kraken.com/v2
//! public `ticker` channel with symbols like "BTC/USD".

use super::{
    canonical_asset, dec_from_f64, http_client, parse_dec, ws_connect, Balances, Exchange,
    ExchangeError,
};
use crate::types::{
    now_ms, ExchangeId, Fees, MarketRules, OrderRequest, OrderResult, OrderStatus, Pair, Side,
    TopOfBook,
};
use async_trait::async_trait;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha512};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub const REST_BASE: &str = "https://api.kraken.com";
pub const WS_URL: &str = "wss://ws.kraken.com/v2";

/// Compute Kraken's API-Sign header value.
pub fn sign(
    secret_b64: &str,
    path: &str,
    nonce: u64,
    postdata: &str,
) -> Result<String, ExchangeError> {
    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret_b64)
        .map_err(|e| ExchangeError::Auth(format!("kraken secret is not base64: {e}")))?;
    let mut sha = Sha256::new();
    sha.update(nonce.to_string().as_bytes());
    sha.update(postdata.as_bytes());
    let inner = sha.finalize();
    let mut mac =
        Hmac::<Sha512>::new_from_slice(&secret).map_err(|e| ExchangeError::Auth(e.to_string()))?;
    mac.update(path.as_bytes());
    mac.update(&inner);
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

/// Kraken's legacy asset codes to the names everyone else (and Kraken's own
/// WebSocket v2) uses.
pub fn kraken_to_common(sym: &str) -> String {
    sym.replace("XBT", "BTC").replace("XDG", "DOGE")
}

/// Form-encode in a stable order with `nonce` first.
pub fn encode_form(nonce: u64, params: &[(&str, String)]) -> String {
    let mut parts = vec![format!("nonce={nonce}")];
    for (k, v) in params {
        parts.push(format!(
            "{}={}",
            k,
            url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
        ));
    }
    parts.join("&")
}

#[derive(Clone, Debug)]
struct KrakenMarket {
    /// Internal id, e.g. XXBTZUSD (ticker result key)
    id: String,
    /// altname, e.g. XBTUSD (used for AddOrder / TradeVolume)
    altname: String,
    /// wsname, e.g. XBT/USD or BTC/USD (used for WS v2)
    wsname: String,
    rules: MarketRules,
}

pub struct Kraken {
    http: reqwest::Client,
    key: Option<String>,
    secret: Option<String>,
    /// configured market symbol (wsname-style "BTC/USD") -> canonical pair
    pairs: HashMap<String, Pair>,
    markets: Mutex<HashMap<Pair, KrakenMarket>>,
    nonce: AtomicU64,
    order_ids: Mutex<HashMap<String, String>>,
}

impl Kraken {
    pub fn new(
        key: Option<String>,
        secret: Option<String>,
        markets: HashMap<String, Pair>,
    ) -> Self {
        Kraken {
            http: http_client(),
            key,
            secret,
            pairs: markets,
            markets: Mutex::new(HashMap::new()),
            nonce: AtomicU64::new(now_ms() as u64 * 1000),
            order_ids: Mutex::new(HashMap::new()),
        }
    }

    fn next_nonce(&self) -> u64 {
        let now = now_ms() as u64 * 1000;
        let prev = self.nonce.fetch_max(now, Ordering::SeqCst).max(now);
        self.nonce.fetch_add(1, Ordering::SeqCst);
        prev + 1
    }

    async fn public(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, ExchangeError> {
        let v: Value = self
            .http
            .get(format!("{REST_BASE}{path}"))
            .query(query)
            .send()
            .await?
            .json()
            .await?;
        Self::unwrap_result(v)
    }

    async fn private(&self, path: &str, params: &[(&str, String)]) -> Result<Value, ExchangeError> {
        let (Some(key), Some(secret)) = (&self.key, &self.secret) else {
            return Err(ExchangeError::NoCredentials(ExchangeId::Kraken));
        };
        let nonce = self.next_nonce();
        let body = encode_form(nonce, params);
        let sig = sign(secret, path, nonce, &body)?;
        let resp = self
            .http
            .post(format!("{REST_BASE}{path}"))
            .header("API-Key", key)
            .header("API-Sign", sig)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ExchangeError::RateLimited);
        }
        let text = resp.text().await?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| ExchangeError::BadResponse(format!("json: {e}: {text}")))?;
        Self::unwrap_result(v)
    }

    fn unwrap_result(v: Value) -> Result<Value, ExchangeError> {
        let errs: Vec<String> = v
            .get("error")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|e| e.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        if !errs.is_empty() {
            let joined = errs.join("; ");
            if joined.contains("EAPI:Invalid key")
                || joined.contains("EAPI:Invalid signature")
                || joined.contains("Permission denied")
            {
                return Err(ExchangeError::Auth(joined));
            }
            if joined.contains("Rate limit") {
                return Err(ExchangeError::RateLimited);
            }
            if joined.contains("EOrder:") {
                return Err(ExchangeError::Rejected(joined));
            }
            return Err(ExchangeError::BadResponse(joined));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| ExchangeError::BadResponse(format!("no result in {v}")))
    }

    /// Load AssetPairs once and resolve configured symbols to id/altname/wsname.
    async fn market(&self, pair: &Pair) -> Result<KrakenMarket, ExchangeError> {
        if let Some(m) = self.markets.lock().unwrap().get(pair) {
            return Ok(m.clone());
        }
        let want = self
            .pairs
            .iter()
            .find(|(_, p)| *p == pair)
            .map(|(s, _)| s.clone())
            .ok_or_else(|| ExchangeError::Unsupported(format!("kraken: no market for {pair}")))?;
        let all = self.public("/0/public/AssetPairs", &[]).await?;
        let obj = all
            .as_object()
            .ok_or_else(|| ExchangeError::BadResponse("AssetPairs not an object".into()))?;
        let want_nospace = want.replace('/', "");
        for (id, info) in obj {
            let altname = info.get("altname").and_then(Value::as_str).unwrap_or("");
            let wsname = info.get("wsname").and_then(Value::as_str).unwrap_or("");
            let matches = wsname == want
                || altname == want_nospace
                || id == &want
                || kraken_to_common(wsname) == want
                || kraken_to_common(altname) == want_nospace;
            if !matches {
                continue;
            }
            let lot_decimals = info
                .get("lot_decimals")
                .and_then(Value::as_u64)
                .unwrap_or(8) as u32;
            let pair_decimals = info
                .get("pair_decimals")
                .and_then(Value::as_u64)
                .unwrap_or(2) as u32;
            let qty_step = Decimal::new(1, lot_decimals);
            let price_tick = match info.get("tick_size").and_then(Value::as_str) {
                Some(t) => parse_dec(t)?,
                None => Decimal::new(1, pair_decimals),
            };
            let min_qty = parse_dec(info.get("ordermin").and_then(Value::as_str).unwrap_or("0"))?;
            let min_notional =
                parse_dec(info.get("costmin").and_then(Value::as_str).unwrap_or("0"))?;
            let m = KrakenMarket {
                id: id.clone(),
                altname: altname.to_string(),
                wsname: wsname.to_string(),
                rules: MarketRules {
                    qty_step,
                    price_tick,
                    min_qty,
                    min_notional,
                },
            };
            self.markets.lock().unwrap().insert(pair.clone(), m.clone());
            return Ok(m);
        }
        Err(ExchangeError::Unsupported(format!(
            "kraken: market '{want}' not found in AssetPairs"
        )))
    }

    fn parse_order(
        &self,
        client_id: &str,
        txid: &str,
        o: &Value,
    ) -> Result<OrderResult, ExchangeError> {
        let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("0");
        let vol_exec = parse_dec(s("vol_exec"))?;
        let fee = parse_dec(s("fee"))?;
        let cost = parse_dec(s("cost"))?;
        let avg = if vol_exec.is_zero() {
            Decimal::ZERO
        } else {
            cost / vol_exec
        };
        let status = match o.get("status").and_then(Value::as_str).unwrap_or("") {
            "closed" => OrderStatus::Filled,
            "canceled" | "expired" => {
                if vol_exec > Decimal::ZERO {
                    OrderStatus::PartiallyFilled
                } else {
                    OrderStatus::Cancelled
                }
            }
            "open" => {
                if vol_exec > Decimal::ZERO {
                    OrderStatus::PartiallyFilled
                } else {
                    OrderStatus::Open
                }
            }
            "pending" => OrderStatus::Unknown,
            other => {
                return Err(ExchangeError::BadResponse(format!(
                    "kraken order status {other}"
                )))
            }
        };
        Ok(OrderResult {
            client_id: client_id.to_string(),
            exchange_order_id: Some(txid.to_string()),
            status,
            filled_qty: vol_exec,
            avg_price: avg,
            fee_quote: fee,
        })
    }

    async fn await_terminal(
        &self,
        client_id: &str,
        txid: &str,
    ) -> Result<OrderResult, ExchangeError> {
        let mut last = None;
        for i in 0..10u32 {
            let v = self
                .private("/0/private/QueryOrders", &[("txid", txid.to_string())])
                .await?;
            let Some(o) = v.get(txid) else {
                return Err(ExchangeError::BadResponse(format!(
                    "order {txid} not found"
                )));
            };
            let r = self.parse_order(client_id, txid, o)?;
            if r.status != OrderStatus::Unknown && r.status != OrderStatus::Open {
                return Ok(r);
            }
            last = Some(r);
            tokio::time::sleep(Duration::from_millis(100 * (i as u64 + 1))).await;
        }
        Ok(last.unwrap_or_else(|| OrderResult::nothing(client_id, OrderStatus::Unknown)))
    }
}

#[derive(Deserialize)]
struct WsTick {
    symbol: String,
    bid: f64,
    bid_qty: f64,
    ask: f64,
    ask_qty: f64,
}

#[async_trait]
impl Exchange for Kraken {
    fn id(&self) -> ExchangeId {
        ExchangeId::Kraken
    }

    async fn fetch_fees(&self) -> Result<Fees, ExchangeError> {
        // Use the first configured market for the tier lookup.
        let pair = self
            .pairs
            .values()
            .next()
            .ok_or_else(|| ExchangeError::Unsupported("kraken: no pairs".into()))?
            .clone();
        let m = self.market(&pair).await?;
        let v = self
            .private("/0/private/TradeVolume", &[("pair", m.altname.clone())])
            .await?;
        let pick = |section: &str| -> Result<Decimal, ExchangeError> {
            let fees = v.get(section).and_then(Value::as_object).ok_or_else(|| {
                ExchangeError::BadResponse(format!("no {section} in TradeVolume: {v}"))
            })?;
            let entry = fees
                .get(&m.id)
                .or_else(|| fees.get(&m.altname))
                .or_else(|| fees.values().next())
                .ok_or_else(|| ExchangeError::BadResponse("empty fee map".into()))?;
            let pct = parse_dec(entry.get("fee").and_then(Value::as_str).unwrap_or("0"))?;
            Ok(pct * Decimal::from(100)) // percent -> bps
        };
        let taker = pick("fees")?;
        let maker = pick("fees_maker").unwrap_or(taker);
        Ok(Fees {
            maker_bps: maker,
            taker_bps: taker,
        })
    }

    async fn fetch_balances(&self) -> Result<Balances, ExchangeError> {
        let v = self.private("/0/private/BalanceEx", &[]).await?;
        let mut out = HashMap::new();
        for (asset, info) in v.as_object().into_iter().flatten() {
            let (bal, hold) = match info {
                Value::Object(o) => (
                    parse_dec(o.get("balance").and_then(Value::as_str).unwrap_or("0"))?,
                    parse_dec(o.get("hold_trade").and_then(Value::as_str).unwrap_or("0"))?,
                ),
                Value::String(s) => (parse_dec(s)?, Decimal::ZERO),
                _ => continue,
            };
            *out.entry(canonical_asset(asset)).or_insert(Decimal::ZERO) += bal - hold;
        }
        Ok(out)
    }

    async fn fetch_market_rules(&self, pair: &Pair) -> Result<MarketRules, ExchangeError> {
        Ok(self.market(pair).await?.rules)
    }

    async fn place_ioc_limit(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        let m = self.market(&req.pair).await?;
        let params = [
            ("ordertype", "limit".to_string()),
            (
                "type",
                match req.side {
                    Side::Buy => "buy",
                    Side::Sell => "sell",
                }
                .to_string(),
            ),
            ("volume", req.qty.normalize().to_string()),
            ("pair", m.altname.clone()),
            ("price", req.limit_price.normalize().to_string()),
            ("timeinforce", "IOC".to_string()),
            ("cl_ord_id", req.client_id.clone()),
        ];
        let v = self.private("/0/private/AddOrder", &params).await?;
        let txid = v
            .get("txid")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .ok_or_else(|| ExchangeError::BadResponse(format!("no txid in {v}")))?
            .to_string();
        self.order_ids
            .lock()
            .unwrap()
            .insert(req.client_id.clone(), txid.clone());
        self.await_terminal(&req.client_id, &txid).await
    }

    async fn fetch_order(
        &self,
        _pair: &Pair,
        client_id: &str,
        exchange_order_id: Option<&str>,
    ) -> Result<OrderResult, ExchangeError> {
        let known = exchange_order_id
            .map(String::from)
            .or_else(|| self.order_ids.lock().unwrap().get(client_id).cloned());
        if let Some(txid) = known {
            return self.await_terminal(client_id, &txid).await;
        }
        // Look up by client order id.
        let v = self
            .private(
                "/0/private/QueryOrders",
                &[("cl_ord_id", client_id.to_string())],
            )
            .await?;
        let Some((txid, o)) = v.as_object().and_then(|o| o.iter().next()) else {
            return Err(ExchangeError::BadResponse(format!(
                "order {client_id} not found"
            )));
        };
        self.parse_order(client_id, txid, o)
    }

    async fn place_post_only(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        let m = self.market(&req.pair).await?;
        let params = [
            ("ordertype", "limit".to_string()),
            (
                "type",
                match req.side {
                    Side::Buy => "buy",
                    Side::Sell => "sell",
                }
                .to_string(),
            ),
            ("volume", req.qty.normalize().to_string()),
            ("pair", m.altname.clone()),
            ("price", req.limit_price.normalize().to_string()),
            ("oflags", "post".to_string()),
            ("cl_ord_id", req.client_id.clone()),
        ];
        let v = match self.private("/0/private/AddOrder", &params).await {
            Ok(v) => v,
            Err(ExchangeError::Rejected(m))
                if m.contains("Post only") || m.contains("post only") =>
            {
                return Ok(OrderResult::nothing(&req.client_id, OrderStatus::Rejected));
            }
            Err(e) => return Err(e),
        };
        let txid = v
            .get("txid")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .ok_or_else(|| ExchangeError::BadResponse(format!("no txid in {v}")))?
            .to_string();
        self.order_ids
            .lock()
            .unwrap()
            .insert(req.client_id.clone(), txid.clone());
        Ok(OrderResult {
            client_id: req.client_id.clone(),
            exchange_order_id: Some(txid),
            status: OrderStatus::Open,
            filled_qty: Decimal::ZERO,
            avg_price: Decimal::ZERO,
            fee_quote: Decimal::ZERO,
        })
    }

    async fn cancel_order(
        &self,
        _pair: &Pair,
        client_id: &str,
        exchange_order_id: Option<&str>,
    ) -> Result<(), ExchangeError> {
        let id = exchange_order_id
            .map(String::from)
            .or_else(|| self.order_ids.lock().unwrap().get(client_id).cloned())
            .unwrap_or_else(|| client_id.to_string());
        match self
            .private("/0/private/CancelOrder", &[("txid", id)])
            .await
        {
            Ok(_) => Ok(()),
            Err(ExchangeError::BadResponse(m)) | Err(ExchangeError::Rejected(m))
                if m.contains("Unknown order") =>
            {
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    async fn stream_top_of_book(
        &self,
        pairs: Vec<Pair>,
        tx: mpsc::Sender<TopOfBook>,
    ) -> Result<(), ExchangeError> {
        // Resolve wsnames so we subscribe with exactly what Kraken expects.
        let mut symbols = Vec::new();
        let mut sym_to_pair: HashMap<String, Pair> = HashMap::new();
        for p in &pairs {
            let m = self.market(p).await?;
            let ws = if m.wsname.is_empty() {
                self.pairs
                    .iter()
                    .find(|(_, pp)| *pp == p)
                    .map(|(s, _)| s.clone())
                    .unwrap_or_default()
            } else {
                m.wsname.clone()
            };
            // Kraken v2 uses BTC/DOGE, not XBT/XDG, in symbols.
            let ws = kraken_to_common(&ws);
            sym_to_pair.insert(ws.clone(), p.clone());
            symbols.push(ws);
        }
        if symbols.is_empty() {
            return Err(ExchangeError::Unsupported("kraken: no mapped pairs".into()));
        }
        let mut ws = ws_connect(WS_URL).await?;
        let sub = json!({"method": "subscribe", "params": {"channel": "ticker", "symbol": symbols, "event_trigger": "bbo", "snapshot": true}});
        ws.send(Message::Text(sub.to_string()))
            .await
            .map_err(|e| ExchangeError::Network(e.to_string()))?;
        tracing::info!(exchange = "kraken", ?symbols, "subscribed");
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(30), ws.next()).await;
            let msg = match msg {
                Err(_) => return Err(ExchangeError::Network("kraken ws idle timeout".into())),
                Ok(None) => return Ok(()),
                Ok(Some(Err(e))) => return Err(ExchangeError::Network(e.to_string())),
                Ok(Some(Ok(m))) => m,
            };
            let text = match msg {
                Message::Text(t) => t,
                Message::Ping(p) => {
                    let _ = ws.send(Message::Pong(p)).await;
                    continue;
                }
                Message::Close(_) => return Ok(()),
                _ => continue,
            };
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("channel").and_then(Value::as_str) != Some("ticker") {
                if v.get("error").is_some() && v.get("success") == Some(&Value::Bool(false)) {
                    return Err(ExchangeError::BadResponse(format!(
                        "kraken ws error: {text}"
                    )));
                }
                continue;
            }
            for d in v
                .get("data")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Ok(t) = serde_json::from_value::<WsTick>(d.clone()) else {
                    continue;
                };
                let Some(pair) = sym_to_pair.get(&t.symbol) else {
                    continue;
                };
                let tob = TopOfBook {
                    exchange: ExchangeId::Kraken,
                    pair: pair.clone(),
                    bid: dec_from_f64(t.bid)?.round_dp(10).normalize(),
                    bid_qty: dec_from_f64(t.bid_qty)?.round_dp(10).normalize(),
                    ask: dec_from_f64(t.ask)?.round_dp(10).normalize(),
                    ask_qty: dec_from_f64(t.ask_qty)?.round_dp(10).normalize(),
                    received_at: Instant::now(),
                    received_ms: now_ms(),
                };
                if tx.send(tob).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Official test vector from docs.kraken.com (spot REST auth guide).
    #[test]
    fn signature_matches_official_test_vector() {
        let secret = "kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==";
        let body =
            "nonce=1616492376594&ordertype=limit&pair=XBTUSD&price=37500&type=buy&volume=1.25";
        let sig = sign(secret, "/0/private/AddOrder", 1616492376594, body).unwrap();
        assert_eq!(sig, "4/dpxb3iT4tp/ZCVEwSnEsLxx0bqyhLpdfOpc6fn7OR8+UClSV5n9E6aSS8MPtnRfp32bAb0nmbRn6H8ndwLUQ==");
    }

    #[test]
    fn legacy_codes_map_to_common_names() {
        assert_eq!(kraken_to_common("XBT/USD"), "BTC/USD");
        assert_eq!(kraken_to_common("XDGUSD"), "DOGEUSD");
        assert_eq!(kraken_to_common("SOL/USD"), "SOL/USD");
    }

    #[test]
    fn form_encoding_puts_nonce_first_and_escapes() {
        let s = encode_form(
            7,
            &[("pair", "XBT/USD".into()), ("cl_ord_id", "a b".into())],
        );
        assert_eq!(s, "nonce=7&pair=XBT%2FUSD&cl_ord_id=a+b");
    }

    #[test]
    fn nonces_strictly_increase() {
        let k = Kraken::new(None, None, HashMap::new());
        let a = k.next_nonce();
        let b = k.next_nonce();
        let c = k.next_nonce();
        assert!(a < b && b < c);
    }

    #[test]
    fn parse_order_statuses() {
        let k = Kraken::new(None, None, HashMap::new());
        let o = json!({"status":"canceled","vol_exec":"0.5","cost":"50.5","fee":"0.1"});
        let r = k.parse_order("c", "T1", &o).unwrap();
        assert_eq!(r.status, OrderStatus::PartiallyFilled);
        assert_eq!(r.avg_price, Decimal::new(101, 0));
        let o = json!({"status":"closed","vol_exec":"1","cost":"100","fee":"0.2"});
        assert_eq!(
            k.parse_order("c", "T1", &o).unwrap().status,
            OrderStatus::Filled
        );
        let o = json!({"status":"canceled","vol_exec":"0","cost":"0","fee":"0"});
        assert_eq!(
            k.parse_order("c", "T1", &o).unwrap().status,
            OrderStatus::Cancelled
        );
    }

    #[test]
    fn error_mapping() {
        let v = json!({"error":["EOrder:Insufficient funds"]});
        assert!(matches!(
            Kraken::unwrap_result(v),
            Err(ExchangeError::Rejected(_))
        ));
        let v = json!({"error":["EAPI:Invalid key"]});
        assert!(matches!(
            Kraken::unwrap_result(v),
            Err(ExchangeError::Auth(_))
        ));
        let v = json!({"error":[],"result":{"x":1}});
        assert_eq!(Kraken::unwrap_result(v).unwrap(), json!({"x":1}));
    }
}
