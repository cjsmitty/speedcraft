# Speedcraft: design and plan

## 0. The honest premise

No trading system can be made incapable of losing money. Prices move while an
order is in flight, exchanges go down mid-trade, fees change, APIs return
errors after a fill. A bot that "cannot lose" does not exist, and anyone who
sells one is lying.

What *can* be built is a bot whose only trade is one that is **already
profitable at the moment it is sent**, with every loss path either closed by
design or capped by a hard limit. That is cross-exchange arbitrage with
pre-positioned inventory, immediate-or-cancel orders, and a kill switch.

The second honest fact, from the fee research (see `docs/RESEARCH.md`): at
retail fee tiers the round trip costs more than the spread almost all the
time. Coinbase charges 0.6%+ taker at the lowest tier, Kraken 0.26% to 0.4%,
Binance 0.1%. Cross-exchange spreads on BTC and ETH are usually under 0.05%.
So the bot, run correctly, will mostly **do nothing**, and that is the correct
behaviour. It trades only when the net edge after real fees is positive. It
will not "turn money into more money fast" at retail tiers, and it is built
to tell you that rather than to lose money proving it.

## 1. Strategy: pre-positioned cross-exchange arbitrage

- Hold quote currency (USD/USDT) on every exchange and base currency (BTC,
  ETH) on every exchange. No on-chain transfers ever happen during trading.
- Stream top-of-book quotes from all exchanges over WebSocket.
- For each pair, for each ordered exchange pair (buy on A, sell on B):
  - gross edge = B.bid - A.ask
  - size = min(A.ask_qty, B.bid_qty) × depth_fraction, capped by max notional,
    capped by balances, rounded down to the exchange lot step.
  - net profit = size × (B.bid × (1 - fee_B) - A.ask × (1 + fee_A)) - slippage buffer
  - trade only if net edge in bps ≥ `min_edge_bps` **and** net profit ≥ `min_profit_quote`.
- Both legs are **limit IOC** (immediate-or-cancel) orders at the crossing
  price. They never rest on the book, so there is no exposure to the market
  moving while an order waits.
- Both legs are sent concurrently.

## 2. Loss paths and how each is handled

| Loss path | Handling |
|---|---|
| Quote is stale when we act | Quotes older than `max_quote_age_ms` are ignored. |
| Fee assumptions wrong | Fees are fetched from each exchange at startup; config values act as a floor (the higher of the two is used). |
| One leg fills, the other does not (legging risk) | Unwind immediately with an IOC order on the filled side at a crossing price. The loss is recorded, counted as a failure, and N consecutive failures trip the kill switch. |
| Partial fills | Matched quantity is kept; unmatched remainder is unwound as above. |
| Request times out, fill unknown | Order status is polled by client order id before any unwind decision. |
| Slow bleed from many small losses | `max_daily_loss_quote` halts trading for the day. |
| Runaway bot | `STOP` file in the working directory halts the bot within one loop iteration; Ctrl-C drains in-flight orders first. |
| Inventory drift (one side runs out) | Balances are refreshed after every trade and before each decision; the trade is skipped if a leg cannot be funded. |
| Exchange outage | Per-exchange circuit breaker: after `max_consecutive_errors` the exchange is disabled until manually re-enabled by restart. |
| Live trading by accident | Paper mode is the default. Live mode needs `--live`, `live = true` in config, and the env var `SPEEDCRAFT_I_ACCEPT_LOSSES=yes`. |
| Wrong symbol mapping (e.g. USD vs USDT) | Pair mapping is explicit in config; the bot refuses to start if any mapped market fails validation against exchange info. |
| Exchange insolvency / withdrawal freeze | **Not mitigated.** Keep only what you can afford to lose on any exchange. |
| Regulatory / tax | **Not mitigated.** Your responsibility. |

## 3. Architecture (Rust, async)

```
src/
  main.rs            CLI: paper (default) | live | record | replay | check
  lib.rs
  config.rs          TOML config + env secrets, validation
  types.rs           Decimal money types, ExchangeId, Pair, TopOfBook, orders
  book.rs            Lock-free-ish shared top-of-book cache (DashMap)
  strategy/arb.rs    Pure opportunity math (unit tested exhaustively)
  risk.rs            Risk manager: limits, kill switch, circuit breakers
  executor.rs        Two-leg concurrent IOC execution + unwind logic
  ledger.rs          Append-only JSONL trade log + PnL summary
  engine.rs          Event loop wiring quotes -> strategy -> risk -> executor
  replay.rs          Backtest recorded quotes through the full pipeline
  exchanges/
    mod.rs           Exchange trait
    paper.rs         Simulated exchange fed by real public quotes
    coinbase.rs      Advanced Trade REST (ES256 JWT) + WS ticker
    kraken.rs        REST (HMAC-SHA512) + WS v2 ticker
    binance.rs       REST (HMAC-SHA256) + WS bookTicker (binance.com / binance.us)
```

Every exchange, including live ones, is behind the `Exchange` trait. The paper
exchange wraps a real public market-data feed and simulates fills against the
real quote, with fees and adverse slippage applied. This means paper mode
tests the whole pipeline against live prices with **no API keys**.

## 4. Build targets

Pure-Rust TLS (rustls) is used everywhere, so the binary builds on macOS
(Apple Silicon and Intel) with nothing but `cargo build --release`. No
OpenSSL, no system libraries.

## 5. Test plan

- Unit tests: strategy math (fees, rounding, sizing, edge thresholds), risk
  manager state machine, exchange signature functions against known vectors,
  symbol mapping, config validation.
- Integration tests: full pipeline with the paper exchange and synthetic
  quotes, covering both-fill, one-leg-fail-with-unwind, stale quote, kill
  switch, daily loss halt.
- Replay: `speedcraft record` captures real quotes; `speedcraft replay`
  runs them through the pipeline and reports how many real opportunities
  existed after fees. Run this **before** considering live mode.

## 6. Operating procedure

1. `cargo build --release`
2. Copy `config/example.toml` to `config.toml`, map the pairs you hold.
3. `./target/release/speedcraft paper` for at least 24 hours. Read the ledger.
4. `./target/release/speedcraft record --out quotes.jsonl` overnight, then
   `speedcraft replay quotes.jsonl` to see the real after-fee opportunity count.
5. Only if replay shows positive expected value at **your** fee tier, fund
   both exchanges, set API keys in env vars, and run `live` with tiny limits.
