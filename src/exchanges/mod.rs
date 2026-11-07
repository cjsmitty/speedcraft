//! The `Exchange` trait every venue implements, plus shared helpers.

pub mod binance;
pub mod coinbase;
pub mod kraken;
pub mod paper;

use crate::types::{ExchangeId, Fees, MarketRules, OrderRequest, OrderResult, Pair, TopOfBook};
use async_trait::async_trait;
use rust_decimal::Decimal;
use std::collections::HashMap;
use tokio::sync::mpsc;

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("network: {0}")]
    Network(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("rejected by exchange: {0}")]
    Rejected(String),
    #[error("rate limited")]
    RateLimited,
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("not supported: {0}")]
    Unsupported(String),
    #[error("missing credentials for {0}")]
    NoCredentials(ExchangeId),
}

impl From<reqwest::Error> for ExchangeError {
    fn from(e: reqwest::Error) -> Self {
        ExchangeError::Network(e.to_string())
    }
}

/// Balances keyed by asset code in the exchange's own naming, normalised to
/// canonical codes (XBT -> BTC etc.) by each connector.
pub type Balances = HashMap<String, Decimal>;

#[async_trait]
pub trait Exchange: Send + Sync {
    fn id(&self) -> ExchangeId;

    /// Current taker/maker fees for this account.
    async fn fetch_fees(&self) -> Result<Fees, ExchangeError>;

    /// Free (available) balances per asset.
    async fn fetch_balances(&self) -> Result<Balances, ExchangeError>;

    /// Lot/tick/minimum rules for a market.
    async fn fetch_market_rules(&self, pair: &Pair) -> Result<MarketRules, ExchangeError>;

    /// Place an immediate-or-cancel limit order and return what filled.
    async fn place_ioc_limit(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError>;

    /// Look up an order's state (used when a request timed out).
    async fn fetch_order(
        &self,
        pair: &Pair,
        client_id: &str,
        exchange_order_id: Option<&str>,
    ) -> Result<OrderResult, ExchangeError>;

    /// Rest a post-only limit order (maker mode). Returns status Open with
    /// the exchange order id, or Rejected if it would have crossed.
    async fn place_post_only(&self, req: &OrderRequest) -> Result<OrderResult, ExchangeError> {
        let _ = req;
        Err(ExchangeError::Unsupported(format!(
            "{}: post-only orders not implemented",
            self.id()
        )))
    }

    /// Cancel a resting order. Ok if it is already gone.
    async fn cancel_order(
        &self,
        pair: &Pair,
        client_id: &str,
        exchange_order_id: Option<&str>,
    ) -> Result<(), ExchangeError> {
        let _ = (pair, client_id, exchange_order_id);
        Err(ExchangeError::Unsupported(format!(
            "{}: cancel not implemented",
            self.id()
        )))
    }

    /// Stream top-of-book updates for `pairs` until the connection drops.
    /// The engine reconnects on return.
    async fn stream_top_of_book(
        &self,
        pairs: Vec<Pair>,
        tx: mpsc::Sender<TopOfBook>,
    ) -> Result<(), ExchangeError>;
}

/// Shared reqwest client with sane timeouts and rustls.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .connect_timeout(std::time::Duration::from_secs(5))
        .user_agent(concat!("speedcraft/", env!("CARGO_PKG_VERSION")))
        .build()
        .expect("reqwest client")
}

pub fn parse_dec(s: &str) -> Result<Decimal, ExchangeError> {
    s.parse::<Decimal>()
        .map_err(|e| ExchangeError::BadResponse(format!("decimal '{s}': {e}")))
}

pub fn dec_from_f64(v: f64) -> Result<Decimal, ExchangeError> {
    Decimal::try_from(v)
        .map_err(|e| ExchangeError::BadResponse(format!("decimal from f64 {v}: {e}")))
}

/// Normalise asset codes across exchanges.
pub fn canonical_asset(code: &str) -> String {
    match code.to_ascii_uppercase().as_str() {
        "XBT" | "XXBT" => "BTC".into(),
        "XETH" => "ETH".into(),
        "ZUSD" => "USD".into(),
        "ZEUR" => "EUR".into(),
        "XXDG" => "DOGE".into(),
        "XLTC" => "LTC".into(),
        "XXRP" => "XRP".into(),
        other => other.to_string(),
    }
}

pub type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Generic WebSocket connect with the rustls connector. Honors HTTPS_PROXY /
/// https_proxy (an `http://host:port` CONNECT proxy) so the bot works behind
/// corporate proxies and in sandboxes that only allow proxied egress.
pub async fn ws_connect(url: &str) -> Result<WsStream, ExchangeError> {
    let t = std::time::Duration::from_secs(10);
    let proxy = std::env::var("HTTPS_PROXY")
        .or_else(|_| std::env::var("https_proxy"))
        .ok()
        .filter(|p| !p.is_empty());
    let fut = async {
        match proxy {
            None => {
                let (ws, _) = tokio_tungstenite::connect_async(url)
                    .await
                    .map_err(|e| ExchangeError::Network(format!("ws connect {url}: {e}")))?;
                Ok::<WsStream, ExchangeError>(ws)
            }
            Some(p) => ws_connect_via_proxy(url, &p).await,
        }
    };
    tokio::time::timeout(t, fut)
        .await
        .map_err(|_| ExchangeError::Network(format!("ws connect timeout {url}")))?
}

async fn ws_connect_via_proxy(url: &str, proxy: &str) -> Result<WsStream, ExchangeError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let target = url::Url::parse(url)
        .map_err(|e| ExchangeError::Network(format!("bad ws url {url}: {e}")))?;
    let host = target
        .host_str()
        .ok_or_else(|| ExchangeError::Network(format!("no host in {url}")))?
        .to_string();
    let port = target.port_or_known_default().unwrap_or(443);
    let purl = url::Url::parse(proxy)
        .map_err(|e| ExchangeError::Network(format!("bad proxy url {proxy}: {e}")))?;
    let phost = purl
        .host_str()
        .ok_or_else(|| ExchangeError::Network("proxy url has no host".into()))?;
    let pport = purl.port_or_known_default().unwrap_or(80);
    let mut tcp = tokio::net::TcpStream::connect((phost, pport))
        .await
        .map_err(|e| ExchangeError::Network(format!("proxy connect {phost}:{pport}: {e}")))?;
    tcp.write_all(format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\nProxy-Connection: keep-alive\r\n\r\n").as_bytes())
        .await
        .map_err(|e| ExchangeError::Network(format!("proxy CONNECT write: {e}")))?;
    // Read the proxy's response headers (up to the blank line).
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = tcp
            .read(&mut byte)
            .await
            .map_err(|e| ExchangeError::Network(format!("proxy CONNECT read: {e}")))?;
        if n == 0 {
            return Err(ExchangeError::Network("proxy closed during CONNECT".into()));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 16 * 1024 {
            return Err(ExchangeError::Network(
                "proxy CONNECT response too large".into(),
            ));
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let status_ok = head
        .lines()
        .next()
        .map(|l| l.contains(" 200 "))
        .unwrap_or(false);
    if !status_ok {
        return Err(ExchangeError::Network(format!(
            "proxy CONNECT refused: {}",
            head.lines().next().unwrap_or("")
        )));
    }
    let (ws, _) = tokio_tungstenite::client_async_tls_with_config(url, tcp, None, None)
        .await
        .map_err(|e| ExchangeError::Network(format!("ws handshake via proxy {url}: {e}")))?;
    Ok(ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn asset_normalisation() {
        assert_eq!(canonical_asset("XXBT"), "BTC");
        assert_eq!(canonical_asset("ZUSD"), "USD");
        assert_eq!(canonical_asset("usdt"), "USDT");
    }
}
