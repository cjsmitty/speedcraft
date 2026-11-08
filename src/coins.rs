//! The coin catalog: which coins Speedcraft knows how to map on each
//! exchange, and a comment-preserving rewrite of the `[[pairs]]` section of
//! a config file so the panel can switch coins on and off.

use crate::types::ExchangeId;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CoinInfo {
    pub base: String,
    pub name: String,
    /// Exchanges that list BASE/USD, with their native symbol.
    pub markets: Vec<(ExchangeId, String)>,
}

fn coin(base: &str, name: &str, binance_us: bool) -> CoinInfo {
    let mut markets = vec![
        (ExchangeId::Coinbase, format!("{base}-USD")),
        (ExchangeId::Kraken, format!("{base}/USD")),
    ];
    if binance_us {
        markets.push((ExchangeId::BinanceUs, format!("{base}USD")));
    }
    CoinInfo {
        base: base.into(),
        name: name.into(),
        markets,
    }
}

/// Coins quoted in USD on Coinbase and Kraken (and Binance.US where listed),
/// verified against the exchanges' product lists in November 2026.
pub fn catalog() -> Vec<CoinInfo> {
    vec![
        coin("BTC", "Bitcoin", true),
        coin("ETH", "Ethereum", true),
        coin("XRP", "XRP", true),
        coin("SOL", "Solana", true),
        coin("DOGE", "Dogecoin", true),
        coin("ADA", "Cardano", true),
        coin("LINK", "Chainlink", true),
        coin("AVAX", "Avalanche", true),
        coin("LTC", "Litecoin", true),
        coin("BCH", "Bitcoin Cash", true),
        coin("DOT", "Polkadot", true),
        coin("XLM", "Stellar", true),
        coin("UNI", "Uniswap", true),
        coin("NEAR", "NEAR", true),
        coin("SUI", "Sui", true),
        coin("HBAR", "Hedera", true),
        coin("AAVE", "Aave", true),
        coin("ATOM", "Cosmos", true),
        coin("OP", "Optimism", true),
        coin("PEPE", "Pepe", true),
        coin("SHIB", "Shiba Inu", true),
        coin("INJ", "Injective", false),
        coin("ONDO", "Ondo", false),
        coin("ARB", "Arbitrum", false),
    ]
}

/// Rewrite the `[[pairs]]` tables of `toml_text` to exactly `bases`, mapped on
/// `exchanges`, keeping every other line and comment intact.
pub fn apply_coins(toml_text: &str, bases: &[String], exchanges: &[ExchangeId]) -> Result<String> {
    let mut doc: DocumentMut = toml_text.parse().context("config is not valid TOML")?;
    let cat = catalog();
    let mut tables = ArrayOfTables::new();
    for b in bases {
        let b = b.trim().to_ascii_uppercase();
        let Some(c) = cat.iter().find(|c| c.base == b) else {
            bail!("unknown coin '{b}'");
        };
        let listed: Vec<&(ExchangeId, String)> = c
            .markets
            .iter()
            .filter(|(e, _)| exchanges.contains(e))
            .collect();
        if listed.len() < 2 {
            bail!("{b} is listed on fewer than two of your enabled exchanges");
        }
        let mut t = Table::new();
        t["pair"] = value(format!("{b}/USD"));
        let mut m = Table::new();
        for (e, sym) in listed {
            m[e.as_str()] = value(sym.as_str());
        }
        t["markets"] = Item::Table(m);
        tables.push(t);
    }
    if tables.is_empty() {
        bail!("select at least one coin");
    }
    doc["pairs"] = Item::ArrayOfTables(tables);
    Ok(doc.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EXAMPLE_TOML};

    #[test]
    fn catalog_is_consistent() {
        let cat = catalog();
        assert!(cat.len() >= 24);
        for c in &cat {
            assert!(c.markets.iter().any(|(e, _)| *e == ExchangeId::Coinbase));
            assert!(c.markets.iter().any(|(e, _)| *e == ExchangeId::Kraken));
        }
        assert!(!cat
            .iter()
            .find(|c| c.base == "INJ")
            .unwrap()
            .markets
            .iter()
            .any(|(e, _)| *e == ExchangeId::BinanceUs));
    }

    #[test]
    fn apply_rewrites_pairs_and_keeps_comments() {
        let out = apply_coins(
            EXAMPLE_TOML,
            &["sol".into(), "DOGE".into()],
            &[ExchangeId::Coinbase, ExchangeId::Kraken],
        )
        .unwrap();
        assert!(
            out.contains("# PAPER MODE IS THE DEFAULT"),
            "comments preserved"
        );
        assert!(out.contains("[strategy]"));
        let cfg: Config = toml::from_str(&out).unwrap();
        cfg.validate().unwrap();
        let pairs: Vec<String> = cfg.pairs.iter().map(|p| p.pair.clone()).collect();
        assert_eq!(pairs, vec!["SOL/USD", "DOGE/USD"]);
        assert_eq!(cfg.pairs[1].markets[&ExchangeId::Kraken], "DOGE/USD");
        assert!(!out.contains("BTC-USD"));
        // Binance.US symbol appears only when that exchange is enabled.
        let out = apply_coins(
            EXAMPLE_TOML,
            &["SOL".into()],
            &[
                ExchangeId::Coinbase,
                ExchangeId::Kraken,
                ExchangeId::BinanceUs,
            ],
        )
        .unwrap();
        assert!(out.contains("binance_us = \"SOLUSD\""));
        assert!(apply_coins(
            EXAMPLE_TOML,
            &["NOPE".into()],
            &[ExchangeId::Coinbase, ExchangeId::Kraken]
        )
        .is_err());
        assert!(apply_coins(
            EXAMPLE_TOML,
            &[],
            &[ExchangeId::Coinbase, ExchangeId::Kraken]
        )
        .is_err());
        assert!(apply_coins(
            EXAMPLE_TOML,
            &["INJ".into()],
            &[ExchangeId::Coinbase, ExchangeId::BinanceUs]
        )
        .is_err());
    }
}
