//! End-to-end: synthetic quotes -> engine -> strategy -> risk -> executor
//! (paper fills) -> ledger. No network.

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use speedcraft::book::BookCache;
use speedcraft::config::{Config, EXAMPLE_TOML};
use speedcraft::engine::Engine;
use speedcraft::exchanges::paper::PaperExchange;
use speedcraft::exchanges::Exchange;
use speedcraft::ledger::{Ledger, LedgerEvent};
use speedcraft::types::{ExchangeId, MarketRules, Pair, TopOfBook};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

fn test_config() -> Config {
    let mut cfg: Config = toml::from_str(EXAMPLE_TOML).unwrap();
    for (id, ex) in cfg.exchanges.iter_mut() {
        ex.enabled = matches!(id, ExchangeId::Coinbase | ExchangeId::Kraken);
        ex.taker_fee_bps = dec!(10);
        ex.maker_fee_bps = dec!(10);
    }
    cfg.paper.sim_latency_ms = 0;
    cfg.paper.sim_slippage_bps = dec!(0);
    cfg.risk.pair_cooldown_ms = 0;
    cfg.strategy.min_profit_quote = dec!(0.01);
    cfg.stop_file = format!("/nonexistent/STOP-{}", std::process::id());
    cfg.validate().unwrap();
    cfg
}

fn rules() -> MarketRules {
    MarketRules {
        qty_step: dec!(0.0001),
        price_tick: dec!(0.01),
        min_qty: dec!(0.0001),
        min_notional: dec!(1),
    }
}

async fn engine(cfg: Config) -> (Engine, Arc<BookCache>) {
    let book = Arc::new(BookCache::new());
    let pairs = cfg.pairs();
    let mut exchanges: HashMap<ExchangeId, Arc<dyn Exchange>> = HashMap::new();
    for id in cfg.enabled_exchanges() {
        exchanges.insert(
            id,
            Arc::new(PaperExchange::new(
                id,
                None,
                book.clone(),
                cfg.fee_floor(id),
                cfg.paper.clone(),
                &pairs,
                rules(),
            )),
        );
    }
    let mut e = Engine::new(cfg, exchanges, book.clone(), Ledger::memory(), "test");
    e.preflight().await.unwrap();
    (e, book)
}

fn q(ex: ExchangeId, bid: Decimal, ask: Decimal, qty: Decimal) -> TopOfBook {
    TopOfBook {
        exchange: ex,
        pair: Pair::parse("BTC/USD").unwrap(),
        bid,
        bid_qty: qty,
        ask,
        ask_qty: qty,
        received_at: Instant::now(),
        received_ms: 0,
    }
}

async fn run_with(e: &mut Engine, quotes: Vec<TopOfBook>) {
    let (tx, rx) = mpsc::channel(64);
    for qq in quotes {
        tx.send(qq).await.unwrap();
    }
    drop(tx);
    let (_stx, srx) = watch::channel(false);
    e.run(rx, srx).await.unwrap();
}

fn trades(e: &Engine) -> u32 {
    e.ledger.trades
}

#[tokio::test]
async fn profitable_cross_executes_once_and_books_profit() {
    let (mut e, _b) = engine(test_config()).await;
    // No cross: nothing happens.
    run_with(
        &mut e,
        vec![
            q(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1)),
            q(ExchangeId::Kraken, dec!(99.5), dec!(100.5), dec!(1)),
        ],
    )
    .await;
    assert_eq!(trades(&e), 0);
    // Kraken bid jumps above Coinbase ask by 1%.
    run_with(
        &mut e,
        vec![q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1))],
    )
    .await;
    assert_eq!(trades(&e), 1);
    assert_eq!(e.ledger.successes, 1);
    // depth allows 0.5 but the selling exchange holds only 0.2 BTC → qty 0.2.
    // buy 0.2 @ 100 = 20, fee 0.02; sell 0.2 @ 101 = 20.2, fee 0.0202
    assert_eq!(
        e.ledger.realised_pnl,
        dec!(20.2) - dec!(0.0202) - dec!(20) - dec!(0.02)
    );
    assert!(e.risk.is_halted().is_none());
    // Balances moved on the paper exchanges.
    let cb = e.exchanges[&ExchangeId::Coinbase]
        .fetch_balances()
        .await
        .unwrap();
    assert_eq!(cb["BTC"], dec!(0.4));
    assert_eq!(cb["USD"], dec!(10000) - dec!(20) - dec!(0.02));
    let kr = e.exchanges[&ExchangeId::Kraken]
        .fetch_balances()
        .await
        .unwrap();
    assert_eq!(kr["BTC"], dec!(0));
    assert_eq!(kr["USD"], dec!(10000) + dec!(20.2) - dec!(0.0202));
}

#[tokio::test]
async fn sell_side_is_capped_by_base_balance() {
    // Start with only 0.2 BTC on the selling exchange: qty must be 0.2, not 0.5.
    let (mut e, _b) = engine(test_config()).await;
    run_with(
        &mut e,
        vec![
            q(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1)),
            q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1)),
        ],
    )
    .await;
    assert_eq!(trades(&e), 1);
    let kr = e.exchanges[&ExchangeId::Kraken]
        .fetch_balances()
        .await
        .unwrap();
    assert_eq!(kr["BTC"], dec!(0));
    let cb = e.exchanges[&ExchangeId::Coinbase]
        .fetch_balances()
        .await
        .unwrap();
    assert_eq!(cb["BTC"], dec!(0.4));
}

#[tokio::test]
async fn stale_quotes_are_ignored() {
    let (mut e, _b) = engine(test_config()).await;
    let mut old = q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1));
    old.received_at = Instant::now() - Duration::from_secs(5);
    run_with(
        &mut e,
        vec![q(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1)), old],
    )
    .await;
    assert_eq!(trades(&e), 0);
}

#[tokio::test]
async fn retail_fees_block_everything() {
    let mut cfg = test_config();
    cfg.exchanges
        .get_mut(&ExchangeId::Coinbase)
        .unwrap()
        .taker_fee_bps = dec!(60);
    cfg.exchanges
        .get_mut(&ExchangeId::Kraken)
        .unwrap()
        .taker_fee_bps = dec!(40);
    let (mut e, _b) = engine(cfg).await;
    // 0.5% gross spread is not enough against 1.0% round-trip fees.
    run_with(
        &mut e,
        vec![
            q(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1)),
            q(ExchangeId::Kraken, dec!(100.5), dec!(101), dec!(1)),
        ],
    )
    .await;
    assert_eq!(trades(&e), 0);
    assert_eq!(e.ledger.opportunities, 0);
    // The status line reports how far from profitable we were.
    assert!(e.status_line().contains("best net edge"));
}

#[tokio::test]
async fn stop_file_blocks_trading() {
    let dir = std::env::temp_dir().join(format!("speedcraft-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let stop = dir.join("STOP");
    std::fs::write(&stop, "").unwrap();
    let mut cfg = test_config();
    cfg.stop_file = stop.to_string_lossy().to_string();
    let (mut e, _b) = engine(cfg).await;
    run_with(
        &mut e,
        vec![
            q(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1)),
            q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1)),
        ],
    )
    .await;
    assert_eq!(trades(&e), 0);
    assert_eq!(
        e.ledger.opportunities, 1,
        "opportunity is logged even when rejected"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn one_legged_fill_is_unwound_and_counted_as_failure() {
    let (mut e, _b) = engine(test_config()).await;
    // Make the Kraken sell leg fail after the engine has decided to trade:
    // drain Kraken's BTC behind the engine's back. The engine still believes
    // Kraken holds 0.2 BTC (balances refresh only after trades), so it sends
    // both legs; the sell is rejected and the bought BTC must be unwound.
    let book = e.book.clone();
    run_with(
        &mut e,
        vec![q(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1))],
    )
    .await;
    book.update(q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1)));
    let pair = Pair::parse("BTC/USD").unwrap();
    let kr = e.exchanges[&ExchangeId::Kraken].clone();
    kr.place_ioc_limit(&speedcraft::types::OrderRequest {
        client_id: "drain".into(),
        pair: pair.clone(),
        side: speedcraft::types::Side::Sell,
        qty: dec!(0.2),
        limit_price: dec!(1),
    })
    .await
    .unwrap();
    // Engine still believes Kraken has 0.2 BTC (balances refreshed only after trades).
    run_with(
        &mut e,
        vec![q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1))],
    )
    .await;
    assert_eq!(trades(&e), 1);
    assert_eq!(e.ledger.successes, 0);
    // The buy leg filled on Coinbase and had to be sold back at the Coinbase bid (99): a real, bounded loss.
    assert!(e.ledger.realised_pnl < Decimal::ZERO);
    assert!(e.ledger.realised_pnl > dec!(-1));
    assert_eq!(e.risk.snapshot().consecutive_failures, 1);
    assert!(e.risk.is_halted().is_none());
    // After refresh the engine knows Kraken has no BTC, so no further trades.
    run_with(
        &mut e,
        vec![q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1))],
    )
    .await;
    assert_eq!(trades(&e), 1);
}

#[tokio::test]
async fn daily_loss_limit_halts() {
    let mut cfg = test_config();
    cfg.risk.max_daily_loss_quote = dec!(0.001);
    let (mut e, b) = engine(cfg).await;
    let pair = Pair::parse("BTC/USD").unwrap();
    // Drain Kraken's BTC behind the engine's back (needs a Kraken quote to fill against).
    b.update(q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1)));
    let kr = e.exchanges[&ExchangeId::Kraken].clone();
    let drained = kr
        .place_ioc_limit(&speedcraft::types::OrderRequest {
            client_id: "drain".into(),
            pair,
            side: speedcraft::types::Side::Sell,
            qty: dec!(0.2),
            limit_price: dec!(1),
        })
        .await
        .unwrap();
    assert_eq!(drained.filled_qty, dec!(0.2));
    run_with(
        &mut e,
        vec![
            q(ExchangeId::Coinbase, dec!(99), dec!(100), dec!(1)),
            q(ExchangeId::Kraken, dec!(101), dec!(102), dec!(1)),
        ],
    )
    .await;
    assert_eq!(trades(&e), 1);
    assert!(e.risk.is_halted().unwrap().starts_with("daily loss"));
    // Restock Kraken so an opportunity exists again, then confirm the halt rejects it.
    kr.place_ioc_limit(&speedcraft::types::OrderRequest {
        client_id: "restock".into(),
        pair: Pair::parse("BTC/USD").unwrap(),
        side: speedcraft::types::Side::Buy,
        qty: dec!(0.2),
        limit_price: dec!(1000),
    })
    .await
    .unwrap();
    e.refresh_balances(None).await.unwrap();
    let opps_before = e.ledger.opportunities;
    run_with(
        &mut e,
        vec![q(ExchangeId::Kraken, dec!(105), dec!(106), dec!(1))],
    )
    .await;
    assert_eq!(trades(&e), 1);
    assert_eq!(
        e.ledger.opportunities,
        opps_before + 1,
        "opportunity found but rejected"
    );
    assert!(e.risk.is_halted().is_some());
}

#[test]
fn ledger_events_serialise() {
    let ev = LedgerEvent::Halt {
        ts_ms: 1,
        reason: "x".into(),
    };
    let s = serde_json::to_string(&ev).unwrap();
    assert!(s.contains("\"event\":\"halt\""));
}
