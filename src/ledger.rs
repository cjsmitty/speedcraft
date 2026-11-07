//! Append-only JSONL ledger. Every decision and fill is written so the
//! history can be audited and replayed.

use crate::strategy::Opportunity;
use crate::types::{ExchangeId, OrderResult, Pair, Side};
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum LedgerEvent {
    Start {
        ts_ms: i64,
        mode: String,
        version: String,
    },
    Opportunity {
        ts_ms: i64,
        op: Opportunity,
    },
    Rejected {
        ts_ms: i64,
        op: Opportunity,
        reason: String,
    },
    Leg {
        ts_ms: i64,
        exchange: ExchangeId,
        pair: Pair,
        side: Side,
        requested_qty: Decimal,
        limit_price: Decimal,
        result: OrderResult,
        unwind: bool,
    },
    Trade {
        ts_ms: i64,
        pair: Pair,
        buy_exchange: ExchangeId,
        sell_exchange: ExchangeId,
        matched_qty: Decimal,
        realised_pnl_quote: Decimal,
        expected_pnl_quote: Decimal,
        success: bool,
        note: String,
    },
    Halt {
        ts_ms: i64,
        reason: String,
    },
    Resume {
        ts_ms: i64,
    },
    Stop {
        ts_ms: i64,
    },
}

pub struct Ledger {
    file: Option<File>,
    pub trades: u32,
    pub successes: u32,
    pub realised_pnl: Decimal,
    pub opportunities: u32,
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Ledger> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening ledger {}", path.display()))?;
        Ok(Ledger {
            file: Some(file),
            trades: 0,
            successes: 0,
            realised_pnl: Decimal::ZERO,
            opportunities: 0,
        })
    }

    /// In-memory ledger (replay/tests).
    pub fn memory() -> Ledger {
        Ledger {
            file: None,
            trades: 0,
            successes: 0,
            realised_pnl: Decimal::ZERO,
            opportunities: 0,
        }
    }

    pub fn write(&mut self, ev: &LedgerEvent) {
        match ev {
            LedgerEvent::Opportunity { .. } => self.opportunities += 1,
            LedgerEvent::Trade {
                realised_pnl_quote,
                success,
                ..
            } => {
                self.trades += 1;
                if *success {
                    self.successes += 1;
                }
                self.realised_pnl += *realised_pnl_quote;
            }
            _ => {}
        }
        if let Some(f) = self.file.as_mut() {
            match serde_json::to_string(ev) {
                Ok(line) => {
                    if let Err(e) = writeln!(f, "{line}").and_then(|_| f.flush()) {
                        tracing::error!(error = %e, "ledger write failed");
                    }
                }
                Err(e) => tracing::error!(error = %e, "ledger serialise failed"),
            }
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "opportunities={} trades={} successes={} realised_pnl={}",
            self.opportunities,
            self.trades,
            self.successes,
            self.realised_pnl.round_dp(4)
        )
    }
}

/// Read a ledger file back (for the `report` command).
pub fn read_ledger(path: &Path) -> Result<Vec<LedgerEvent>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let ev: LedgerEvent =
            serde_json::from_str(&line).with_context(|| format!("ledger line {}", i + 1))?;
        out.push(ev);
    }
    Ok(out)
}
