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

/// Generic WebSocket connect with the rustls connector.
pub async fn ws_connect(
    url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    ExchangeError,
> {
    let (ws, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio_tungstenite::connect_async(url),
    )
    .await
    .map_err(|_| ExchangeError::Network(format!("ws connect timeout {url}")))?
    .map_err(|e| ExchangeError::Network(format!("ws connect {url}: {e}")))?;
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
