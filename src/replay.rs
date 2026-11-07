//! Record real quotes to JSONL and replay them through the whole pipeline
//! with paper execution. This is the backtest: it answers "how many
//! after-fee opportunities actually existed, and what would I have made?"

use crate::types::{ExchangeId, Pair, TopOfBook};
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::Instant;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QuoteRecord {
    pub ts_ms: i64,
    pub exchange: ExchangeId,
    pub pair: String,
    pub bid: Decimal,
    pub bid_qty: Decimal,
    pub ask: Decimal,
    pub ask_qty: Decimal,
}

impl From<&TopOfBook> for QuoteRecord {
    fn from(t: &TopOfBook) -> Self {
        QuoteRecord {
            ts_ms: t.received_ms,
            exchange: t.exchange,
            pair: t.pair.to_string(),
            bid: t.bid,
            bid_qty: t.bid_qty,
            ask: t.ask,
            ask_qty: t.ask_qty,
        }
    }
}

impl QuoteRecord {
    pub fn into_top_of_book(self) -> Result<TopOfBook> {
        Ok(TopOfBook {
            exchange: self.exchange,
            pair: Pair::parse(&self.pair).map_err(anyhow::Error::msg)?,
            bid: self.bid,
            bid_qty: self.bid_qty,
            ask: self.ask,
            ask_qty: self.ask_qty,
            received_at: Instant::now(),
            received_ms: self.ts_ms,
        })
    }
}

pub struct QuoteWriter<W: Write> {
    w: W,
    pub count: u64,
}

impl<W: Write> QuoteWriter<W> {
    pub fn new(w: W) -> Self {
        QuoteWriter { w, count: 0 }
    }
    pub fn write(&mut self, t: &TopOfBook) -> Result<()> {
        let rec = QuoteRecord::from(t);
        serde_json::to_writer(&mut self.w, &rec)?;
        self.w.write_all(b"\n")?;
        self.count += 1;
        Ok(())
    }
    pub fn flush(&mut self) -> Result<()> {
        Ok(self.w.flush()?)
    }
}

pub fn read_quotes(path: &Path) -> Result<Vec<QuoteRecord>> {
    let f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    let mut bad = 0usize;
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<QuoteRecord>(&line) {
            Ok(r) => out.push(r),
            Err(e) => {
                // A recording that is still being written ends in a partial line.
                bad += 1;
                if bad <= 3 {
                    tracing::warn!(line = i + 1, error = %e, "skipping unparseable quote line");
                }
            }
        }
    }
    if bad > 0 {
        tracing::warn!(skipped = bad, "unparseable quote lines skipped");
    }
    out.sort_by_key(|r| r.ts_ms);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn quote_record_roundtrip() {
        let t = TopOfBook {
            exchange: ExchangeId::Kraken,
            pair: Pair::parse("ETH/USD").unwrap(),
            bid: dec!(1),
            bid_qty: dec!(2),
            ask: dec!(3),
            ask_qty: dec!(4),
            received_at: Instant::now(),
            received_ms: 42,
        };
        let mut buf = Vec::new();
        {
            let mut w = QuoteWriter::new(&mut buf);
            w.write(&t).unwrap();
            w.flush().unwrap();
        }
        let rec: QuoteRecord = serde_json::from_slice(buf.trim_ascii_end()).unwrap();
        let back = rec.into_top_of_book().unwrap();
        assert_eq!(back.pair, t.pair);
        assert_eq!(back.ask_qty, dec!(4));
        assert_eq!(back.received_ms, 42);
    }
}
