//! Structured settings for the panel: a comment-preserving patch of the
//! config file, a masked view of secrets, and a connection test per exchange.

use crate::config::{Config, Secrets, SecretsFile};
use crate::types::ExchangeId;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use toml_edit::{DocumentMut, Item, Table};

/// Apply a nested JSON patch (`{"strategy": {"min_edge_bps": 5}, "live": true,
/// "exchanges": {"kraken": {"enabled": false}}}`) to the TOML text, keeping
/// comments and ordering. `null` removes a key. The result is validated.
pub fn apply_patch(toml_text: &str, patch: &Value) -> Result<String> {
    let mut doc: DocumentMut = toml_text.parse().context("config is not valid TOML")?;
    let Some(obj) = patch.as_object() else {
        bail!("patch must be an object")
    };
    for (k, v) in obj {
        if k == "pairs" {
            bail!("use the Coins setting to change pairs");
        }
        set_path(doc.as_table_mut(), k, v)?;
    }
    let text = doc.to_string();
    let cfg: Config = toml::from_str(&text).context("settings produce an invalid config")?;
    cfg.validate()?;
    Ok(text)
}

fn set_path(table: &mut Table, key: &str, v: &Value) -> Result<()> {
    match v {
        Value::Null => {
            table.remove(key);
            Ok(())
        }
        Value::Object(o) => {
            if !table.contains_key(key) || !table[key].is_table() {
                let mut t = Table::new();
                t.set_implicit(false);
                table[key] = Item::Table(t);
            }
            let sub = table[key].as_table_mut().context("expected a table")?;
            for (k2, v2) in o {
                set_path(sub, k2, v2)?;
            }
            Ok(())
        }
        _ => {
            table[key] = Item::Value(to_toml(v)?);
            Ok(())
        }
    }
}

fn to_toml(v: &Value) -> Result<toml_edit::Value> {
    Ok(match v {
        Value::Bool(b) => toml_edit::Value::from(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml_edit::Value::from(i)
            } else {
                toml_edit::Value::from(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => {
            let t = s.trim();
            // Numeric strings (Decimal fields arrive as strings) become numbers.
            if let Ok(i) = t.parse::<i64>() {
                toml_edit::Value::from(i)
            } else if let Ok(f) = t.parse::<f64>() {
                if t.chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == '-')
                {
                    toml_edit::Value::from(f)
                } else {
                    toml_edit::Value::from(t)
                }
            } else {
                toml_edit::Value::from(t)
            }
        }
        Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(to_toml(x)?);
            }
            toml_edit::Value::Array(arr)
        }
        Value::Null | Value::Object(_) => bail!("unsupported value"),
    })
}

/// What the panel may see about stored secrets: presence and a hint, never values.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SecretsView {
    pub coinbase_key_name: Option<String>,
    pub coinbase_private_key_set: bool,
    pub kraken_key: Option<String>,
    pub kraken_secret_set: bool,
    pub binance_key: Option<String>,
    pub binance_secret_set: bool,
    pub binance_us_key: Option<String>,
    pub binance_us_secret_set: bool,
    pub accept_losses: bool,
    /// Which exchanges have complete credentials (file or environment).
    pub ready: Vec<ExchangeId>,
    pub path: String,
}

fn hint(v: &Option<String>) -> Option<String> {
    v.as_ref().map(|s| {
        let t = s.trim();
        if t.len() <= 8 {
            "••••".to_string()
        } else {
            format!("{}…{}", &t[..4], &t[t.len() - 4..])
        }
    })
}

pub fn secrets_view(dir: &Path) -> Result<SecretsView> {
    let f = SecretsFile::load(dir)?;
    let eff = Secrets::load(Some(dir));
    Ok(SecretsView {
        coinbase_key_name: f.coinbase_key_name.clone(),
        coinbase_private_key_set: eff.coinbase_key_pem.is_some(),
        kraken_key: hint(&f.kraken_key),
        kraken_secret_set: eff.kraken_secret.is_some(),
        binance_key: hint(&f.binance_key),
        binance_secret_set: eff.binance_secret.is_some(),
        binance_us_key: hint(&f.binance_us_key),
        binance_us_secret_set: eff.binance_us_secret.is_some(),
        accept_losses: eff.accept_losses,
        ready: ExchangeId::all()
            .into_iter()
            .filter(|e| eff.has(*e))
            .collect(),
        path: SecretsFile::path(dir).display().to_string(),
    })
}

/// Patch for the secrets file. A field present with a value sets it; an
/// empty string clears it; absent fields are untouched.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SecretsPatch {
    pub coinbase_key_name: Option<String>,
    pub coinbase_private_key: Option<String>,
    pub kraken_key: Option<String>,
    pub kraken_secret: Option<String>,
    pub binance_key: Option<String>,
    pub binance_secret: Option<String>,
    pub binance_us_key: Option<String>,
    pub binance_us_secret: Option<String>,
    pub accept_losses: Option<bool>,
}

pub fn apply_secrets(dir: &Path, p: &SecretsPatch) -> Result<SecretsView> {
    let mut f = SecretsFile::load(dir)?;
    let set = |slot: &mut Option<String>, v: &Option<String>| {
        if let Some(v) = v {
            let t = v.trim();
            *slot = if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            };
        }
    };
    set(&mut f.coinbase_key_name, &p.coinbase_key_name);
    set(&mut f.coinbase_private_key, &p.coinbase_private_key);
    set(&mut f.kraken_key, &p.kraken_key);
    set(&mut f.kraken_secret, &p.kraken_secret);
    set(&mut f.binance_key, &p.binance_key);
    set(&mut f.binance_secret, &p.binance_secret);
    set(&mut f.binance_us_key, &p.binance_us_key);
    set(&mut f.binance_us_secret, &p.binance_us_secret);
    if let Some(a) = p.accept_losses {
        f.accept_losses = a;
    }
    if let Some(pem) = &f.coinbase_private_key {
        crate::exchanges::coinbase::load_ec_key(&pem.replace("\\n", "\n"))
            .map_err(|e| anyhow::anyhow!("Coinbase private key is not a valid EC key: {e}"))?;
    }
    f.save(dir)?;
    secrets_view(dir)
}

#[derive(Clone, Debug, Serialize)]
pub struct ExchangeTest {
    pub exchange: ExchangeId,
    pub ok: bool,
    pub message: String,
    pub taker_fee_bps: Option<String>,
    pub maker_fee_bps: Option<String>,
    pub balances: Vec<(String, String)>,
}

/// Talk to one exchange with the stored credentials: fees, balances, and the
/// first configured market's rules. Never places an order.
pub async fn test_exchange(cfg: &Config, id: ExchangeId) -> ExchangeTest {
    let secrets = Secrets::load(cfg.dir.as_deref());
    let mut out = ExchangeTest {
        exchange: id,
        ok: false,
        message: String::new(),
        taker_fee_bps: None,
        maker_fee_bps: None,
        balances: vec![],
    };
    if !cfg.enabled_exchanges().contains(&id) {
        out.message = "exchange is switched off in settings".into();
        return out;
    }
    if !secrets.has(id) {
        out.message = "no API credentials saved for this exchange".into();
        return out;
    }
    let mut single = cfg.clone();
    for (e, c) in single.exchanges.iter_mut() {
        c.enabled = *e == id;
    }
    let ex = match crate::runtime::build_live_exchanges(&single, &secrets, true) {
        Ok(m) => match m.get(&id) {
            Some(x) => x.clone(),
            None => {
                out.message = "connector not available".into();
                return out;
            }
        },
        Err(e) => {
            out.message = format!("{e:#}");
            return out;
        }
    };
    let t = std::time::Duration::from_secs(20);
    match tokio::time::timeout(t, ex.fetch_fees()).await {
        Ok(Ok(f)) => {
            out.taker_fee_bps = Some(f.taker_bps.to_string());
            out.maker_fee_bps = Some(f.maker_bps.to_string());
        }
        Ok(Err(e)) => {
            out.message = format!("fee lookup failed: {e}");
            return out;
        }
        Err(_) => {
            out.message = "fee lookup timed out".into();
            return out;
        }
    }
    match tokio::time::timeout(t, ex.fetch_balances()).await {
        Ok(Ok(b)) => {
            let mut v: Vec<(String, String)> = b
                .into_iter()
                .filter(|(_, q)| !q.is_zero())
                .map(|(a, q)| (a, q.normalize().to_string()))
                .collect();
            v.sort();
            out.balances = v;
        }
        Ok(Err(e)) => {
            out.message = format!("balance lookup failed: {e}");
            return out;
        }
        Err(_) => {
            out.message = "balance lookup timed out".into();
            return out;
        }
    }
    if let Some(p) = cfg.pairs().first() {
        if cfg.market_symbol(id, p).is_some() {
            if let Ok(Err(e)) = tokio::time::timeout(t, ex.fetch_market_rules(p)).await {
                out.message = format!("market lookup for {p} failed: {e}");
                return out;
            }
        }
    }
    out.ok = true;
    out.message = "connected: credentials work, fees and balances read".into();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EXAMPLE_TOML;
    use serde_json::json;

    #[test]
    fn patch_sets_nested_values_and_keeps_comments() {
        let out = apply_patch(EXAMPLE_TOML, &json!({"live": true, "strategy": {"min_edge_bps": "7.5", "max_quote_age_ms": 900}, "exchanges": {"kraken": {"enabled": true, "taker_fee_bps": "26"}}, "risk": {"max_daily_loss_quote": 50}})).unwrap();
        assert!(out.contains("# PAPER MODE IS THE DEFAULT"));
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(cfg.live);
        assert_eq!(cfg.strategy.min_edge_bps.to_string(), "7.5");
        assert_eq!(cfg.strategy.max_quote_age_ms, 900);
        assert_eq!(
            cfg.exchanges[&ExchangeId::Kraken].taker_fee_bps.to_string(),
            "26"
        );
        assert_eq!(cfg.risk.max_daily_loss_quote.to_string(), "50");
    }

    #[test]
    fn patch_rejects_invalid_results_and_pairs() {
        assert!(apply_patch(EXAMPLE_TOML, &json!({"strategy": {"depth_fraction": 5}})).is_err());
        assert!(apply_patch(EXAMPLE_TOML, &json!({"pairs": []})).is_err());
        assert!(apply_patch(EXAMPLE_TOML, &json!({"exchanges": {"coinbase": {"enabled": false}, "kraken": {"enabled": false}}})).is_err(), "needs two exchanges");
        // maker section can be created and cleared
        let out = apply_patch(EXAMPLE_TOML, &json!({"maker": {"enabled": true, "quote_exchange": "kraken", "hedge_exchange": "coinbase", "pairs": ["BTC/USD"]}})).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(cfg.maker.as_ref().unwrap().enabled);
        assert_eq!(
            cfg.maker.as_ref().unwrap().pairs.as_ref().unwrap(),
            &vec!["BTC/USD".to_string()]
        );
        let out = apply_patch(&out, &json!({"maker": {"pairs": null}})).unwrap();
        let cfg: Config = toml::from_str(&out).unwrap();
        assert!(cfg.maker.as_ref().unwrap().pairs.is_none());
    }

    #[test]
    fn secrets_patch_masks_and_validates() {
        let dir = std::env::temp_dir().join(format!(
            "speedcraft-settings-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let v = apply_secrets(
            &dir,
            &SecretsPatch {
                kraken_key: Some("ABCDEFGHIJKLMNOP".into()),
                kraken_secret: Some("c2VjcmV0".into()),
                accept_losses: Some(true),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(v.kraken_key.as_deref(), Some("ABCD…MNOP"));
        assert!(v.kraken_secret_set);
        assert!(v.accept_losses);
        assert_eq!(v.ready, vec![ExchangeId::Kraken]);
        let v = apply_secrets(
            &dir,
            &SecretsPatch {
                kraken_secret: Some("".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!v.kraken_secret_set);
        assert!(v.ready.is_empty());
        assert!(apply_secrets(
            &dir,
            &SecretsPatch {
                coinbase_private_key: Some("not a key".into()),
                ..Default::default()
            }
        )
        .is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
