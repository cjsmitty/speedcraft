//! Coinbase Advanced Trade.
//!
//! REST base: https://api.coinbase.com. Auth is a per-request ES256 JWT
//! (CDP key name `organizations/{org}/apiKeys/{id}` + EC private key PEM),
//! claims `sub`, `iss = "cdp"`, `nbf`, `exp = nbf + 120`, `uri = "METHOD host path"`,
//! header `kid` and random `nonce`. Market data: wss://advanced-trade-ws.coinbase.com,
//! public `ticker` channel (no JWT needed).

use super::{
    canonical_asset, http_client, parse_dec, ws_connect, Balances, Exchange, ExchangeError,
};
use crate::types::{
    now_ms, ExchangeId, Fees, MarketRules, OrderRequest, OrderResult, OrderStatus, Pair, Side,
    TopOfBook,
};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub const REST_HOST: &str = "api.coinbase.com";
pub const WS_URL: &str = "wss://advanced-trade-ws.coinbase.com";

pub struct Coinbase {
    http: reqwest::Client,
    key_name: Option<String>,
    key: Option<p256::ecdsa::SigningKey>,
    /// canonical pair -> product_id (e.g. "BTC-USD")
    products: HashMap<Pair, String>,
    /// product_id -> canonical pair
    pairs: HashMap<String, Pair>,
    /// client_order_id -> exchange order_id
    order_ids: Mutex<HashMap<String, String>>,
}

#[derive(Serialize)]
struct Claims<'a> {
    sub: &'a str,
    iss: &'a str,
    nbf: u64,
    exp: u64,
    uri: String,
}

/// Build the JWT for one request. Public so it can be unit tested.
pub fn build_jwt(
    key_name: &str,
    key: &p256::ecdsa::SigningKey,
    method: &str,
    path: &str,
    now: u64,
) -> Result<String, ExchangeError> {
    use base64::Engine;
    use p256::ecdsa::signature::Signer;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let nonce: String = (0..16)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    let header = json!({"alg": "ES256", "typ": "JWT", "kid": key_name, "nonce": nonce});
    let claims = Claims {
        sub: key_name,
        iss: "cdp",
        nbf: now,
        exp: now + 120,
        uri: format!("{method} {REST_HOST}{path}"),
    };
    let signing_input = format!(
        "{}.{}",
        b64.encode(
            serde_json::to_vec(&header).map_err(|e| ExchangeError::BadResponse(e.to_string()))?
        ),
        b64.encode(
            serde_json::to_vec(&claims).map_err(|e| ExchangeError::BadResponse(e.to_string()))?
        )
    );
    // ES256 JWT signatures are the raw 64-byte r||s concatenation.
    let sig: p256::ecdsa::Signature = key.sign(signing_input.as_bytes());
    Ok(format!("{signing_input}.{}", b64.encode(sig.to_bytes())))
}

/// Load a P-256 private key from PEM in either SEC1 ("EC PRIVATE KEY", what
/// Coinbase issues) or PKCS#8 ("PRIVATE KEY") form.
pub fn load_ec_key(pem: &str) -> Result<p256::ecdsa::SigningKey, ExchangeError> {
    use p256::pkcs8::DecodePrivateKey;
    let pem = pem.trim();
    if pem.contains("BEGIN EC PRIVATE KEY") {
        let sk = p256::SecretKey::from_sec1_pem(pem)
            .map_err(|e| ExchangeError::Auth(format!("coinbase SEC1 key: {e}")))?;
        return Ok(p256::ecdsa::SigningKey::from(sk));
    }
    p256::ecdsa::SigningKey::from_pkcs8_pem(pem)
        .map_err(|e| ExchangeError::Auth(format!("coinbase PKCS#8 key: {e}")))
}

impl Coinbase {
    /// `key_name`/`pem` may be None for public-data-only use (paper mode).
    pub fn new(
        key_name: Option<String>,
        pem: Option<String>,
        markets: HashMap<String, Pair>,
    ) -> Result<Self, ExchangeError> {
        let key = match &pem {
            Some(p) => Some(load_ec_key(p)?),
            None => None,
        };
        let products = markets
            .iter()
            .map(|(s, p)| (p.clone(), s.clone()))
            .collect();
        Ok(Coinbase {
            http: http_client(),
            key_name,
            key,
            products,
            pairs: markets,
            order_ids: Mutex::new(HashMap::new()),
        })
    }

    fn product(&self, pair: &Pair) -> Result<&str, ExchangeError> {
        self.products
            .get(pair)
            .map(|s| s.as_str())
            .ok_or_else(|| ExchangeError::Unsupported(format!("coinbase: no market for {pair}")))
    }

    fn auth_header(&self, method: &str, path: &str) -> Result<String, ExchangeError> {
        let (Some(name), Some(key)) = (&self.key_name, &self.key) else {
            return Err(ExchangeError::NoCredentials(ExchangeId::Coinbase));
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Ok(format!(
            "Bearer {}",
            build_jwt(name, key, method, path, now)?
        ))
    }

    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
        auth: bool,
    ) -> Result<Value, ExchangeError> {
        let url = format!("https://{REST_HOST}{path}");
        let mut req = self.http.get(&url).query(query);
        if auth {
            req = req.header("Authorization", self.auth_header("GET", path)?);
        }
        let resp = req.send().await?;
        Self::handle(resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, ExchangeError> {
        let url = format!("https://{REST_HOST}{path}");
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth_header("POST", path)?)
            .json(body)
            .send()
            .await?;
        Self::handle(resp).await
    }

    async fn handle(resp: reqwest::Response) -> Result<Value, ExchangeError> {
        let status = resp.status();
        let text = resp.text().await?;
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(ExchangeError::RateLimited);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ExchangeError::Auth(format!("{status}: {text}")));
        }
        if !status.is_success() {
            return Err(ExchangeError::BadResponse(format!("{status}: {text}")));
        }
        serde_json::from_str(&text)
            .map_err(|e| ExchangeError::BadResponse(format!("json: {e}: {text}")))
    }

    fn parse_order(&self, o: &Value) -> Result<OrderResult, ExchangeError> {
        let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("0");
        let status = match o.get("status").and_then(Value::as_str).unwrap_or("") {
            "FILLED" => OrderStatus::Filled,
            "CANCELLED" | "EXPIRED" | "CANCEL_QUEUED" => OrderStatus::Cancelled,
            "FAILED" => OrderStatus::Rejected,
            "OPEN" | "PENDING" | "QUEUED" | "" => OrderStatus::Unknown,
            other => return Err(ExchangeError::BadResponse(format!("order status {other}"))),
        };
        let filled = parse_dec(s("filled_size"))?;
        let mut status = status;
        if status == OrderStatus::Cancelled && filled > Decimal::ZERO {
            status = OrderStatus::PartiallyFilled;
        }
        Ok(OrderResult {
            client_id: o
                .get("client_order_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            exchange_order_id: o.get("order_id").and_then(Value::as_str).map(String::from),
            status,
            filled_qty: filled,
            avg_price: parse_dec(s("average_filled_price"))?,
            fee_quote: parse_dec(s("total_fees"))?,
        })
    }

    async fn get_order_by_id(&self, order_id: &str) -> Result<OrderResult, ExchangeError> {
        let v = self
            .get(
                &format!("/api/v3/brokerage/orders/historical/{order_id}"),
                &[],
                true,
            )
            .await?;
        let o = v
            .get("order")
            .ok_or_else(|| ExchangeError::BadResponse(format!("no order in {v}")))?;
        self.parse_order(o)
    }

    /// Poll until the IOC order is in a terminal state (it should be at once).
    async fn await_terminal(&self, order_id: &str) -> Result<OrderResult, ExchangeError> {
        let mut last = None;
        for i in 0..10u32 {
            let r = self.get_order_by_id(order_id).await?;
            if r.status != OrderStatus::Unknown {
                return Ok(r);
            }
            last = Some(r);
            tokio::time::sleep(Duration::from_millis(100 * (i as u64 + 1))).await;
        }
        Ok(last.unwrap_or_else(|| OrderResult::nothing("", OrderStatus::Unknown)))
    }
}

#[derive(Deserialize)]
struct WsTicker {
    product_id: String,
    best_bid: Option<String>,
    best_ask: Option<String>,
    best_bid_quantity: Option<String>,
    best_ask_quantity: Option<String>,
}

#[async_trait]
impl Exchange for Coinbase {
    fn id(&self) -> ExchangeId {
        ExchangeId::Coinbase
    }

    async fn fetch_fees(&self) -> Result<Fees, ExchangeError> {
        let v = self
            .get(
                "/api/v3/brokerage/transaction_summary",
                &[("product_type", "SPOT")],
                true,
            )
            .await?;
        let tier = v
            .get("fee_tier")
            .ok_or_else(|| ExchangeError::BadResponse(format!("no fee_tier in {v}")))?;
        let rate = |k: &str| -> Result<Decimal, ExchangeError> {
            let raw = tier
                .get(k)
                .ok_or_else(|| ExchangeError::BadResponse(format!("no {k} in fee_tier")))?;
            match raw {
                Value::String(s) => parse_dec(s),
                Value::Number(n) => super::dec_from_f64(n.as_f64().unwrap_or(0.0)),
                _ => Err(ExchangeError::BadResponse(format!("bad {k}"))),
            }
        };
        Ok(Fees {
            maker_bps: rate("maker_fee_rate")? * Decimal::from(10_000),
            taker_bps: rate("taker_fee_rate")? * Decimal::from(10_000),
        })
    }

    async fn fetch_balances(&self) -> Result<Balances, ExchangeError> {
        let mut out = HashMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut q = vec![("limit", "250".to_string())];
            if let Some(c) = &cursor {
                q.push(("cursor", c.clone()));
            }
            let qref: Vec<(&str, &str)> = q.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let v = self.get("/api/v3/brokerage/accounts", &qref, true).await?;
            for a in v
                .get("accounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let cur = a.get("currency").and_then(Value::as_str).unwrap_or("");
                let avail = a
                    .get("available_balance")
                    .and_then(|b| b.get("value"))
                    .and_then(Value::as_str)
                    .unwrap_or("0");
                *out.entry(canonical_asset(cur)).or_insert(Decimal::ZERO) += parse_dec(avail)?;
            }
            if v.get("has_next").and_then(Value::as_bool).unwrap_or(false) {
                cursor = v.get("cursor").and_then(Value::as_str).map(String::from);
                if cursor.is_none() {
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    async fn fetch_market_rules(&self, pair: &Pair) -> Result<MarketRules, ExchangeError> {
        let product = self.product(pair)?;
        // Public endpoint first (works without keys), authenticated as fallback.
        let v = match self
            .get(
                &format!("/api/v3/brokerage/market/products/{product}"),
                &[],
                false,
            )
            .await
        {
            Ok(v) => v,
            Err(public_err) => {
                if self.key.is_some() {
                    self.get(&format!("/api/v3/brokerage/products/{product}"), &[], true)
                        .await?
                } else {
                    return Err(public_err);
                }
            }
        };
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .ok_or_else(|| ExchangeError::BadResponse(format!("product missing {k}: {v}")))
        };
        let qty_step = parse_dec(s("base_increment")?)?;
        let price_tick = parse_dec(s("quote_increment")?)?;
        let min_qty = parse_dec(s("base_min_size")?)?;
        let min_notional = parse_dec(s("quote_min_size").unwrap_or("0"))?;
        Ok(MarketRules {
            qty_step,
            price_tick,
            min_qty,
            min_notional,
        })
    }

    async fn place_ioc_limit(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        let product = self.product(&req.pair)?;
        let body = json!({
            "client_order_id": req.client_id,
            "product_id": product,
            "side": match req.side { Side::Buy => "BUY", Side::Sell => "SELL" },
            "order_configuration": {
                "sor_limit_ioc": {
                    "base_size": req.qty.normalize().to_string(),
                    "limit_price": req.limit_price.normalize().to_string(),
                }
            }
        });
        let v = self.post("/api/v3/brokerage/orders", &body).await?;
        if !v.get("success").and_then(Value::as_bool).unwrap_or(false) {
            let msg = v
                .get("error_response")
                .map(|e| e.to_string())
                .unwrap_or_else(|| v.to_string());
            return Err(ExchangeError::Rejected(msg));
        }
        let order_id = v
            .get("success_response")
            .and_then(|s| s.get("order_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| ExchangeError::BadResponse(format!("no order_id in {v}")))?
            .to_string();
        self.order_ids
            .lock()
            .unwrap()
            .insert(req.client_id.clone(), order_id.clone());
        let mut r = self.await_terminal(&order_id).await?;
        r.client_id = req.client_id.clone();
        r.exchange_order_id = Some(order_id);
        Ok(r)
    }

    async fn fetch_order(
        &self,
        pair: &Pair,
        client_id: &str,
        exchange_order_id: Option<&str>,
    ) -> Result<OrderResult, ExchangeError> {
        let known = exchange_order_id
            .map(String::from)
            .or_else(|| self.order_ids.lock().unwrap().get(client_id).cloned());
        if let Some(id) = known {
            return self.await_terminal(&id).await;
        }
        // We never learned the order id (request timed out). Scan recent orders.
        let product = self.product(pair)?;
        let v = self
            .get(
                "/api/v3/brokerage/orders/historical/batch",
                &[("product_ids", product), ("limit", "100")],
                true,
            )
            .await?;
        for o in v
            .get("orders")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if o.get("client_order_id").and_then(Value::as_str) == Some(client_id) {
                return self.parse_order(o);
            }
        }
        Err(ExchangeError::BadResponse(format!(
            "order {client_id} not found"
        )))
    }

    async fn stream_top_of_book(
        &self,
        pairs: Vec<Pair>,
        tx: mpsc::Sender<TopOfBook>,
    ) -> Result<(), ExchangeError> {
        let ids: Vec<&str> = pairs
            .iter()
            .filter_map(|p| self.products.get(p).map(String::as_str))
            .collect();
        if ids.is_empty() {
            return Err(ExchangeError::Unsupported(
                "coinbase: no mapped pairs".into(),
            ));
        }
        let mut ws = ws_connect(WS_URL).await?;
        let sub = json!({"type": "subscribe", "channel": "ticker", "product_ids": ids});
        ws.send(Message::Text(sub.to_string()))
            .await
            .map_err(|e| ExchangeError::Network(e.to_string()))?;
        let hb = json!({"type": "subscribe", "channel": "heartbeats"});
        ws.send(Message::Text(hb.to_string()))
            .await
            .map_err(|e| ExchangeError::Network(e.to_string()))?;
        tracing::info!(exchange = "coinbase", ?ids, "subscribed");
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(30), ws.next()).await;
            let msg = match msg {
                Err(_) => return Err(ExchangeError::Network("coinbase ws idle timeout".into())),
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
                if v.get("type").and_then(Value::as_str) == Some("error") {
                    return Err(ExchangeError::BadResponse(format!(
                        "coinbase ws error: {text}"
                    )));
                }
                continue;
            }
            for ev in v
                .get("events")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                for t in ev
                    .get("tickers")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Ok(t) = serde_json::from_value::<WsTicker>(t.clone()) else {
                        continue;
                    };
                    let Some(pair) = self.pairs.get(&t.product_id) else {
                        continue;
                    };
                    let (Some(b), Some(a)) = (t.best_bid, t.best_ask) else {
                        continue;
                    };
                    let tob = TopOfBook {
                        exchange: ExchangeId::Coinbase,
                        pair: pair.clone(),
                        bid: parse_dec(&b)?,
                        bid_qty: parse_dec(t.best_bid_quantity.as_deref().unwrap_or("0"))?,
                        ask: parse_dec(&a)?,
                        ask_qty: parse_dec(t.best_ask_quantity.as_deref().unwrap_or("0"))?,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    // A throwaway P-256 key generated for tests only. Never used anywhere real.
    const TEST_PEM: &str = "-----BEGIN EC PRIVATE KEY-----\nMHcCAQEEIGUi+WJ2lR2+67ULA7xXp6nW/jOap0k8JuiILsuw3iOmoAoGCCqGSM49\nAwEHoUQDQgAEWdVw9esGMTIdqLRblQjmhD5bO0Xywvf/TocpMUHFTnj8wi8KIXUp\nXkw78AYvWGQAb/cBdrgRLu8VLHDxBeFJ2g==\n-----END EC PRIVATE KEY-----\n";

    #[test]
    fn jwt_has_expected_header_and_claims() {
        let key = load_ec_key(TEST_PEM).expect("test key loads");
        let name = "organizations/abc/apiKeys/def";
        let jwt = build_jwt(
            name,
            &key,
            "GET",
            "/api/v3/brokerage/accounts",
            1_700_000_000,
        )
        .unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let header: Value = serde_json::from_slice(&b64.decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["kid"], name);
        assert_eq!(header["nonce"].as_str().unwrap().len(), 32);
        let claims: Value = serde_json::from_slice(&b64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["iss"], "cdp");
        assert_eq!(claims["sub"], name);
        assert_eq!(claims["nbf"], 1_700_000_000u64);
        assert_eq!(claims["exp"], 1_700_000_120u64);
        assert_eq!(
            claims["uri"],
            "GET api.coinbase.com/api/v3/brokerage/accounts"
        );
        // 64-byte raw signature, and it verifies against the public key.
        let sig_bytes = b64.decode(parts[2]).unwrap();
        assert_eq!(sig_bytes.len(), 64);
        use p256::ecdsa::signature::Verifier;
        let vk = p256::ecdsa::VerifyingKey::from(&key);
        let sig = p256::ecdsa::Signature::from_slice(&sig_bytes).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }

    #[test]
    fn pkcs8_keys_also_load() {
        use p256::pkcs8::EncodePrivateKey;
        let sk = load_ec_key(TEST_PEM).unwrap();
        let pkcs8 = p256::SecretKey::from(sk.as_nonzero_scalar())
            .to_pkcs8_pem(p256::pkcs8::LineEnding::LF)
            .unwrap();
        assert!(pkcs8.contains("BEGIN PRIVATE KEY"));
        load_ec_key(&pkcs8).unwrap();
        assert!(load_ec_key("garbage").is_err());
    }

    #[test]
    fn parse_order_maps_statuses() {
        let cb = Coinbase::new(None, None, HashMap::new()).unwrap();
        let o = json!({"order_id":"x","client_order_id":"c","status":"CANCELLED","filled_size":"0.5","average_filled_price":"100.5","total_fees":"0.3"});
        let r = cb.parse_order(&o).unwrap();
        assert_eq!(r.status, OrderStatus::PartiallyFilled);
        assert_eq!(r.filled_qty, Decimal::new(5, 1));
        assert_eq!(r.fee_quote, Decimal::new(3, 1));
        let o = json!({"order_id":"x","status":"FILLED","filled_size":"1","average_filled_price":"100","total_fees":"0"});
        assert_eq!(cb.parse_order(&o).unwrap().status, OrderStatus::Filled);
        let o = json!({"order_id":"x","status":"OPEN","filled_size":"0","average_filled_price":"0","total_fees":"0"});
        assert_eq!(cb.parse_order(&o).unwrap().status, OrderStatus::Unknown);
    }
}
