//! Binance Spot (binance.com) and Binance.US (binance.us). Same API shape.
//!
//! Signed requests: `signature = hex(HMAC-SHA256(secret, querystring))`,
//! header `X-MBX-APIKEY`. Market data: `wss://stream.<host>:9443/stream?streams=<sym>@bookTicker/...`
//! with payload fields `u, s, b, B, a, A`.

use super::{
    canonical_asset, http_client, parse_dec, ws_connect, Balances, Exchange, ExchangeError,
};
use crate::types::{
    now_ms, ExchangeId, Fees, MarketRules, OrderRequest, OrderResult, OrderStatus, Pair, Side,
    TopOfBook,
};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub fn hmac_sha256_hex(secret: &str, payload: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac accepts any key length");
    mac.update(payload.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

pub fn query_string(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(k, v)| {
            format!(
                "{}={}",
                k,
                url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

pub struct Binance {
    id: ExchangeId,
    http: reqwest::Client,
    rest_base: String,
    ws_base: String,
    key: Option<String>,
    secret: Option<String>,
    /// symbol (e.g. BTCUSDT) -> canonical pair
    pairs: HashMap<String, Pair>,
    symbols: HashMap<Pair, String>,
    fee_cache: Mutex<Option<Fees>>,
}

impl Binance {
    pub fn new(
        id: ExchangeId,
        key: Option<String>,
        secret: Option<String>,
        markets: HashMap<String, Pair>,
    ) -> Self {
        let (rest_base, ws_base) = match id {
            ExchangeId::BinanceUs => (
                "https://api.binance.us".to_string(),
                "wss://stream.binance.us:9443".to_string(),
            ),
            _ => (
                "https://api.binance.com".to_string(),
                "wss://stream.binance.com:9443".to_string(),
            ),
        };
        let symbols = markets
            .iter()
            .map(|(s, p)| (p.clone(), s.clone()))
            .collect();
        Binance {
            id,
            http: http_client(),
            rest_base,
            ws_base,
            key,
            secret,
            pairs: markets,
            symbols,
            fee_cache: Mutex::new(None),
        }
    }

    fn symbol(&self, pair: &Pair) -> Result<&str, ExchangeError> {
        self.symbols
            .get(pair)
            .map(String::as_str)
            .ok_or_else(|| ExchangeError::Unsupported(format!("{}: no market for {pair}", self.id)))
    }

    async fn handle(resp: reqwest::Response) -> Result<Value, ExchangeError> {
        let status = resp.status();
        let text = resp.text().await?;
        if status.as_u16() == 429 || status.as_u16() == 418 {
            return Err(ExchangeError::RateLimited);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ExchangeError::Auth(format!("{status}: {text}")));
        }
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| ExchangeError::BadResponse(format!("json: {e}: {text}")))?;
        if !status.is_success() {
            let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
            let msg = v
                .get("msg")
                .and_then(Value::as_str)
                .unwrap_or(&text)
                .to_string();
            // -2010 = NEW_ORDER_REJECTED, -1013 = filter failure, -2011 = cancel rejected, -2013 = order does not exist
            return match code {
                -2010 | -1013 | -1111 | -1121 => {
                    Err(ExchangeError::Rejected(format!("{code}: {msg}")))
                }
                -2013 => Err(ExchangeError::BadResponse(format!(
                    "order not found: {msg}"
                ))),
                -1022 | -2014 | -2015 => Err(ExchangeError::Auth(format!("{code}: {msg}"))),
                _ => Err(ExchangeError::BadResponse(format!(
                    "{status} {code}: {msg}"
                ))),
            };
        }
        Ok(v)
    }

    async fn public(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, ExchangeError> {
        let resp = self
            .http
            .get(format!("{}{path}", self.rest_base))
            .query(query)
            .send()
            .await?;
        Self::handle(resp).await
    }

    async fn signed(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<Value, ExchangeError> {
        let (Some(key), Some(secret)) = (&self.key, &self.secret) else {
            return Err(ExchangeError::NoCredentials(self.id));
        };
        let mut p: Vec<(&str, String)> = params.to_vec();
        p.push(("recvWindow", "5000".into()));
        p.push(("timestamp", now_ms().to_string()));
        let qs = query_string(&p);
        let sig = hmac_sha256_hex(secret, &qs);
        let url = format!("{}{path}?{qs}&signature={sig}", self.rest_base);
        let resp = self
            .http
            .request(method, url)
            .header("X-MBX-APIKEY", key)
            .send()
            .await?;
        Self::handle(resp).await
    }

    fn parse_order(
        &self,
        pair: &Pair,
        v: &Value,
        fallback_fee: Option<&Fees>,
    ) -> Result<OrderResult, ExchangeError> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("0");
        let executed = parse_dec(s("executedQty"))?;
        let quote_qty = parse_dec(
            v.get("cummulativeQuoteQty")
                .or_else(|| v.get("cumulativeQuoteQty"))
                .and_then(Value::as_str)
                .unwrap_or("0"),
        )?;
        let avg = if executed.is_zero() {
            Decimal::ZERO
        } else {
            quote_qty / executed
        };
        let status = match v.get("status").and_then(Value::as_str).unwrap_or("") {
            "FILLED" => OrderStatus::Filled,
            "PARTIALLY_FILLED" => OrderStatus::PartiallyFilled,
            "EXPIRED" | "CANCELED" | "EXPIRED_IN_MATCH" => {
                if executed > Decimal::ZERO {
                    OrderStatus::PartiallyFilled
                } else {
                    OrderStatus::Cancelled
                }
            }
            "REJECTED" => OrderStatus::Rejected,
            "NEW" => OrderStatus::Open,
            "PENDING_NEW" | "" => OrderStatus::Unknown,
            other => {
                return Err(ExchangeError::BadResponse(format!(
                    "binance order status {other}"
                )))
            }
        };
        // Fees: sum fills' commission in quote; convert base-denominated
        // commission at the fill price; anything else (BNB) falls back to the
        // configured rate.
        let mut fee = Decimal::ZERO;
        let mut unknown_asset = false;
        if let Some(fills) = v.get("fills").and_then(Value::as_array) {
            for f in fills {
                let c = parse_dec(f.get("commission").and_then(Value::as_str).unwrap_or("0"))?;
                let asset = canonical_asset(
                    f.get("commissionAsset")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                );
                let px = parse_dec(f.get("price").and_then(Value::as_str).unwrap_or("0"))?;
                if asset == pair.quote {
                    fee += c;
                } else if asset == pair.base {
                    fee += c * px;
                } else {
                    unknown_asset = true;
                }
            }
        } else {
            unknown_asset = executed > Decimal::ZERO;
        }
        if unknown_asset {
            let rate = fallback_fee
                .map(|f| f.taker_rate())
                .unwrap_or(Decimal::new(10, 4));
            fee = fee.max(quote_qty * rate);
        }
        Ok(OrderResult {
            client_id: v
                .get("clientOrderId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            exchange_order_id: v.get("orderId").map(|i| i.to_string()),
            status,
            filled_qty: executed,
            avg_price: avg,
            fee_quote: fee,
        })
    }
}

impl Binance {
    async fn poll_top_of_book(
        &self,
        pairs: Vec<Pair>,
        tx: mpsc::Sender<TopOfBook>,
    ) -> Result<(), ExchangeError> {
        let symbols: Vec<String> = pairs
            .iter()
            .filter_map(|p| self.symbols.get(p).cloned())
            .collect();
        let upper: HashMap<String, Pair> = self
            .pairs
            .iter()
            .map(|(s, p)| (s.to_ascii_uppercase(), p.clone()))
            .collect();
        let list = format!(
            "[{}]",
            symbols
                .iter()
                .map(|s| format!("\"{s}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        let mut tick = tokio::time::interval(Duration::from_millis(1000));
        let mut failures = 0u32;
        loop {
            tick.tick().await;
            let v = match self
                .public("/api/v3/ticker/bookTicker", &[("symbols", list.as_str())])
                .await
            {
                Ok(v) => {
                    failures = 0;
                    v
                }
                Err(e) => {
                    failures += 1;
                    if failures >= 10 {
                        return Err(e);
                    }
                    continue;
                }
            };
            let items: Vec<Value> = match v {
                Value::Array(a) => a,
                other => vec![other],
            };
            for it in items {
                let (Some(sym), Some(b), Some(bq), Some(a), Some(aq)) = (
                    it.get("symbol").and_then(Value::as_str),
                    it.get("bidPrice").and_then(Value::as_str),
                    it.get("bidQty").and_then(Value::as_str),
                    it.get("askPrice").and_then(Value::as_str),
                    it.get("askQty").and_then(Value::as_str),
                ) else {
                    continue;
                };
                let Some(pair) = upper.get(&sym.to_ascii_uppercase()) else {
                    continue;
                };
                let tob = TopOfBook {
                    exchange: self.id,
                    pair: pair.clone(),
                    bid: parse_dec(b)?,
                    bid_qty: parse_dec(bq)?,
                    ask: parse_dec(a)?,
                    ask_qty: parse_dec(aq)?,
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

#[derive(Deserialize)]
struct BookTicker {
    s: String,
    b: String,
    #[serde(rename = "B")]
    bq: String,
    a: String,
    #[serde(rename = "A")]
    aq: String,
}

#[async_trait]
impl Exchange for Binance {
    fn id(&self) -> ExchangeId {
        self.id
    }

    async fn fetch_fees(&self) -> Result<Fees, ExchangeError> {
        let v = self
            .signed(
                reqwest::Method::GET,
                "/api/v3/account",
                &[("omitZeroBalances", "true".into())],
            )
            .await?;
        let rates = v.get("commissionRates").ok_or_else(|| {
            ExchangeError::BadResponse(format!("no commissionRates in account: {v}"))
        })?;
        let r = |k: &str| parse_dec(rates.get(k).and_then(Value::as_str).unwrap_or("0"));
        let fees = Fees {
            maker_bps: r("maker")? * Decimal::from(10_000),
            taker_bps: r("taker")? * Decimal::from(10_000),
        };
        *self.fee_cache.lock().unwrap() = Some(fees);
        Ok(fees)
    }

    async fn fetch_balances(&self) -> Result<Balances, ExchangeError> {
        let v = self
            .signed(
                reqwest::Method::GET,
                "/api/v3/account",
                &[("omitZeroBalances", "true".into())],
            )
            .await?;
        let mut out = HashMap::new();
        for b in v
            .get("balances")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let asset = b.get("asset").and_then(Value::as_str).unwrap_or("");
            let free = parse_dec(b.get("free").and_then(Value::as_str).unwrap_or("0"))?;
            *out.entry(canonical_asset(asset)).or_insert(Decimal::ZERO) += free;
        }
        Ok(out)
    }

    async fn fetch_market_rules(&self, pair: &Pair) -> Result<MarketRules, ExchangeError> {
        let symbol = self.symbol(pair)?;
        let v = self
            .public("/api/v3/exchangeInfo", &[("symbol", symbol)])
            .await?;
        let sym = v
            .get("symbols")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .ok_or_else(|| {
                ExchangeError::BadResponse(format!("exchangeInfo has no symbol {symbol}"))
            })?;
        if sym.get("status").and_then(Value::as_str) != Some("TRADING") {
            return Err(ExchangeError::Unsupported(format!(
                "{symbol} is not TRADING"
            )));
        }
        let mut rules = MarketRules {
            qty_step: Decimal::ZERO,
            price_tick: Decimal::ZERO,
            min_qty: Decimal::ZERO,
            min_notional: Decimal::ZERO,
        };
        for f in sym
            .get("filters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let g = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("0");
            match f.get("filterType").and_then(Value::as_str).unwrap_or("") {
                "LOT_SIZE" => {
                    rules.qty_step = parse_dec(g("stepSize"))?;
                    rules.min_qty = parse_dec(g("minQty"))?;
                }
                "PRICE_FILTER" => rules.price_tick = parse_dec(g("tickSize"))?,
                "NOTIONAL" | "MIN_NOTIONAL" => {
                    rules.min_notional = rules.min_notional.max(parse_dec(g("minNotional"))?)
                }
                _ => {}
            }
        }
        if rules.qty_step.is_zero() || rules.price_tick.is_zero() {
            return Err(ExchangeError::BadResponse(format!(
                "missing LOT_SIZE/PRICE_FILTER for {symbol}"
            )));
        }
        Ok(rules)
    }

    async fn place_ioc_limit(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        let symbol = self.symbol(&req.pair)?.to_string();
        let params = [
            ("symbol", symbol),
            (
                "side",
                match req.side {
                    Side::Buy => "BUY",
                    Side::Sell => "SELL",
                }
                .to_string(),
            ),
            ("type", "LIMIT".to_string()),
            ("timeInForce", "IOC".to_string()),
            ("quantity", req.qty.normalize().to_string()),
            ("price", req.limit_price.normalize().to_string()),
            ("newClientOrderId", req.client_id.clone()),
            ("newOrderRespType", "FULL".to_string()),
        ];
        let v = self
            .signed(reqwest::Method::POST, "/api/v3/order", &params)
            .await?;
        let fees = *self.fee_cache.lock().unwrap();
        let mut r = self.parse_order(&req.pair, &v, fees.as_ref())?;
        r.client_id = req.client_id.clone();
        if r.status == OrderStatus::Unknown {
            // IOC should be terminal immediately; look again.
            return self.fetch_order(&req.pair, &req.client_id, None).await;
        }
        Ok(r)
    }

    async fn fetch_order(
        &self,
        pair: &Pair,
        client_id: &str,
        _xid: Option<&str>,
    ) -> Result<OrderResult, ExchangeError> {
        let symbol = self.symbol(pair)?.to_string();
        let fees = *self.fee_cache.lock().unwrap();
        for i in 0..10u32 {
            let v = self
                .signed(
                    reqwest::Method::GET,
                    "/api/v3/order",
                    &[
                        ("symbol", symbol.clone()),
                        ("origClientOrderId", client_id.to_string()),
                    ],
                )
                .await?;
            let r = self.parse_order(pair, &v, fees.as_ref())?;
            if r.status != OrderStatus::Unknown {
                return Ok(r);
            }
            tokio::time::sleep(Duration::from_millis(100 * (i as u64 + 1))).await;
        }
        Ok(OrderResult::nothing(client_id, OrderStatus::Unknown))
    }

    async fn place_post_only(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        let symbol = self.symbol(&req.pair)?.to_string();
        let params = [
            ("symbol", symbol),
            (
                "side",
                match req.side {
                    Side::Buy => "BUY",
                    Side::Sell => "SELL",
                }
                .to_string(),
            ),
            ("type", "LIMIT_MAKER".to_string()),
            ("quantity", req.qty.normalize().to_string()),
            ("price", req.limit_price.normalize().to_string()),
            ("newClientOrderId", req.client_id.clone()),
            ("newOrderRespType", "RESULT".to_string()),
        ];
        let v = match self
            .signed(reqwest::Method::POST, "/api/v3/order", &params)
            .await
        {
            Ok(v) => v,
            // -2010 with "would immediately match" = post-only rejection, non-fatal.
            Err(ExchangeError::Rejected(m))
                if m.contains("immediately match") || m.contains("-2010") =>
            {
                return Ok(OrderResult::nothing(&req.client_id, OrderStatus::Rejected));
            }
            Err(e) => return Err(e),
        };
        let fees = *self.fee_cache.lock().unwrap();
        let mut r = self.parse_order(&req.pair, &v, fees.as_ref())?;
        r.client_id = req.client_id.clone();
        if r.status == OrderStatus::Unknown {
            r.status = OrderStatus::Open;
        }
        Ok(r)
    }

    async fn cancel_order(
        &self,
        pair: &Pair,
        client_id: &str,
        _xid: Option<&str>,
    ) -> Result<(), ExchangeError> {
        let symbol = self.symbol(pair)?.to_string();
        match self
            .signed(
                reqwest::Method::DELETE,
                "/api/v3/order",
                &[
                    ("symbol", symbol),
                    ("origClientOrderId", client_id.to_string()),
                ],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(ExchangeError::BadResponse(m))
                if m.contains("not found") || m.contains("-2011") =>
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
        let streams: Vec<String> = pairs
            .iter()
            .filter_map(|p| self.symbols.get(p))
            .map(|s| format!("{}@bookTicker", s.to_ascii_lowercase()))
            .collect();
        if streams.is_empty() {
            return Err(ExchangeError::Unsupported(format!(
                "{}: no mapped pairs",
                self.id
            )));
        }
        let url = format!("{}/stream?streams={}", self.ws_base, streams.join("/"));
        let mut ws = match ws_connect(&url).await {
            Ok(ws) => ws,
            Err(e) => {
                // Some networks block the 9443 stream port. Fall back to polling
                // the public REST book ticker once a second: coarser, but the
                // staleness checks downstream keep it honest.
                tracing::warn!(exchange = %self.id, error = %e, "websocket unavailable; polling REST bookTicker at 1 Hz");
                return self.poll_top_of_book(pairs, tx).await;
            }
        };
        tracing::info!(exchange = %self.id, ?streams, "subscribed");
        let upper: HashMap<String, Pair> = self
            .pairs
            .iter()
            .map(|(s, p)| (s.to_ascii_uppercase(), p.clone()))
            .collect();
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(60), ws.next()).await;
            let msg = match msg {
                Err(_) => return Err(ExchangeError::Network("binance ws idle timeout".into())),
                Ok(None) => return Ok(()),
                Ok(Some(Err(e))) => return Err(ExchangeError::Network(e.to_string())),
                Ok(Some(Ok(m))) => m,
            };
            let text = match msg {
                Message::Text(t) => t,
                Message::Ping(p) => {
                    ws.send(Message::Pong(p))
                        .await
                        .map_err(|e| ExchangeError::Network(e.to_string()))?;
                    continue;
                }
                Message::Close(_) => return Ok(()),
                _ => continue,
            };
            let v: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let data = v.get("data").unwrap_or(&v);
            let Ok(t) = serde_json::from_value::<BookTicker>(data.clone()) else {
                continue;
            };
            let Some(pair) = upper.get(&t.s.to_ascii_uppercase()) else {
                continue;
            };
            let tob = TopOfBook {
                exchange: self.id,
                pair: pair.clone(),
                bid: parse_dec(&t.b)?,
                bid_qty: parse_dec(&t.bq)?,
                ask: parse_dec(&t.a)?,
                ask_qty: parse_dec(&t.aq)?,
                received_at: Instant::now(),
                received_ms: now_ms(),
            };
            if tx.send(tob).await.is_err() {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Vector from the Binance API docs (signed endpoint example).
    #[test]
    fn hmac_matches_binance_doc_example() {
        let secret = "NhqPtmdSJYdKjVHjA7PZj4Mge3R5YNiP1e3UZjInClVN65XAbvqqM6A7H5fATj0j";
        let payload = "symbol=LTCBTC&side=BUY&type=LIMIT&timeInForce=GTC&quantity=1&price=0.1&recvWindow=5000&timestamp=1499827319559";
        assert_eq!(
            hmac_sha256_hex(secret, payload),
            "c8db56825ae71d6d79447849e617115f4a920fa2acdcab2b053c4b2838bd6b71"
        );
    }

    #[test]
    fn query_string_escapes() {
        assert_eq!(
            query_string(&[("a", "1".into()), ("b", "x y".into())]),
            "a=1&b=x+y"
        );
    }

    #[test]
    fn parse_order_full_response_with_fills() {
        let b = Binance::new(ExchangeId::Binance, None, None, HashMap::new());
        let pair = Pair::parse("BTC/USDT").unwrap();
        let v = json!({
            "clientOrderId":"c1","orderId":123,"status":"FILLED","executedQty":"0.5","cummulativeQuoteQty":"50.0",
            "fills":[{"price":"100","qty":"0.5","commission":"0.05","commissionAsset":"USDT"}]
        });
        let r = b.parse_order(&pair, &v, None).unwrap();
        assert_eq!(r.status, OrderStatus::Filled);
        assert_eq!(r.avg_price, Decimal::new(100, 0));
        assert_eq!(r.fee_quote, Decimal::new(5, 2));
        // base-denominated commission converts at fill price
        let v = json!({"status":"EXPIRED","executedQty":"0.2","cumulativeQuoteQty":"20","fills":[{"price":"100","qty":"0.2","commission":"0.0002","commissionAsset":"BTC"}]});
        let r = b.parse_order(&pair, &v, None).unwrap();
        assert_eq!(r.status, OrderStatus::PartiallyFilled);
        assert_eq!(r.fee_quote, Decimal::new(2, 2));
        // BNB commission falls back to configured rate
        let v = json!({"status":"FILLED","executedQty":"1","cummulativeQuoteQty":"100","fills":[{"price":"100","qty":"1","commission":"0.001","commissionAsset":"BNB"}]});
        let r = b
            .parse_order(
                &pair,
                &v,
                Some(&Fees {
                    maker_bps: Decimal::new(10, 0),
                    taker_bps: Decimal::new(10, 0),
                }),
            )
            .unwrap();
        assert_eq!(r.fee_quote, Decimal::new(1, 1));
        let v = json!({"status":"EXPIRED","executedQty":"0","cummulativeQuoteQty":"0","fills":[]});
        assert_eq!(
            b.parse_order(&pair, &v, None).unwrap().status,
            OrderStatus::Cancelled
        );
    }

    #[tokio::test]
    async fn error_codes_map() {
        // Build a fake response through reqwest's From<http::Response>.
        let resp = http::Response::builder()
            .status(400)
            .body(r#"{"code":-2010,"msg":"Account has insufficient balance"}"#)
            .unwrap();
        let r = Binance::handle(reqwest::Response::from(resp)).await;
        assert!(matches!(r, Err(ExchangeError::Rejected(_))));
        let resp = http::Response::builder().status(429).body("{}").unwrap();
        assert!(matches!(
            Binance::handle(reqwest::Response::from(resp)).await,
            Err(ExchangeError::RateLimited)
        ));
    }
}
