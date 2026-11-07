# Speedcraft

A cross-exchange crypto arbitrage bot in Rust for macOS (and Linux), with
hard risk limits and a paper-trading default.

**Read this first.** No trading bot can be made incapable of losing money.
What this one does instead is refuse to send any trade that is not already
profitable after real fees at the moment it is sent, use immediate-or-cancel
orders so nothing ever rests on a book, unwind any one-legged fill at once,
and halt on a daily loss limit, a failure streak, or a `STOP` file. Its worst
case is bounded by the limits you set. Its best case depends entirely on
whether after-fee opportunities exist at your fee tier, and at retail tiers
they almost never do. `docs/PLAN.md` has the full design; `docs/RESEARCH.md`
has the fee data and sources.

What the bot measured on real Coinbase and Kraken quotes while being built
(BTC/USD and ETH/USD, default fee floors of 0.60% and 0.40% taker):

| Metric | Value |
|---|---|
| Best net edge seen, BTC/USD | about -102 bps |
| Best net edge seen, ETH/USD | about -100 bps |
| Opportunities after fees | 0 |

The spread between the two exchanges was near zero; fees are about 1% round
trip. The bot therefore did nothing, which is the correct outcome.

## Build (macOS)

```sh
# Rust via https://rustup.rs if you do not have it
cargo build --release
./target/release/speedcraft --help
```

Pure-Rust TLS with the system keychain as trust store. No OpenSSL, no Homebrew
dependencies. Apple Silicon and Intel both work with the same command.

## Run

```sh
cp config/example.toml config.toml   # edit fee floors, pairs, limits
./target/release/speedcraft check    # validates config, markets, (and keys if set)
./target/release/speedcraft paper    # live quotes, simulated fills, no keys needed
./target/release/speedcraft record --out quotes.jsonl --seconds 28800   # overnight capture
./target/release/speedcraft replay quotes.jsonl                          # backtest it
./target/release/speedcraft report ledger.jsonl
```

`paper` is the default subcommand. It prints a status line every 30 seconds
that includes the best net edge seen per pair, so you can see exactly how far
the market is from a profitable trade at your fee tier.

## Going live

Only after `replay` on an overnight recording shows positive expected value at
**your** fee tier. Then:

1. Fund both exchanges: quote currency (USD) **and** base (BTC, ETH) on each.
   The bot never transfers between exchanges; it only trades what is already there.
2. Create API keys with trade permission and **no withdrawal permission**.
3. Export credentials (never put them in the config file):

   | Exchange | Variables |
   |---|---|
   | Coinbase Advanced | `COINBASE_API_KEY_NAME` (`organizations/.../apiKeys/...`), `COINBASE_API_PRIVATE_KEY` (EC PEM; `\n` escapes accepted) |
   | Kraken | `KRAKEN_API_KEY`, `KRAKEN_API_SECRET` |
   | Binance | `BINANCE_API_KEY`, `BINANCE_API_SECRET` |
   | Binance.US | `BINANCE_US_API_KEY`, `BINANCE_US_API_SECRET` |

4. Set `live = true` in `config.toml`, export `SPEEDCRAFT_I_ACCEPT_LOSSES=yes`,
   and run `speedcraft live --live`. All three are required; any one alone refuses.
5. Start with `max_notional_quote` and `max_daily_loss_quote` tiny.
6. `touch STOP` pauses trading within one loop iteration. Ctrl-C shuts down cleanly.

The live Coinbase, Kraken and Binance connectors were built from the official
API references and their signing code is unit-tested against the exchanges'
published test vectors. They have **not** been exercised against funded
accounts in this repository. Run `check` with your keys first: it fetches your
real fee tier, balances and market rules and prints them without trading.

## Safety properties, in order of importance

| Property | Mechanism |
|---|---|
| Never sends a trade that loses on paper | Net edge after fetched fees, slippage buffer, and exchange minimums must be positive and above `min_edge_bps` and `min_profit_quote` |
| Never pays more than planned | Both legs are limit IOC at the quoted price plus/minus half the slippage buffer |
| Nothing rests on a book | IOC only. No GTC orders anywhere in the code |
| One-legged fills are closed immediately | Unmatched quantity is unwound with a crossing IOC on the filled exchange, up to 3 attempts |
| Unknown fill state stops the bot | A leg whose status cannot be determined halts trading until a human looks |
| Losses are capped | `max_daily_loss_quote`, `max_consecutive_failures`, `max_trades_per_day` |
| A dead exchange is isolated | Per-exchange circuit breaker after `max_consecutive_errors` |
| Stale data is never acted on | Quotes older than `max_quote_age_ms` are discarded |
| Fees cannot be under-estimated | Uses the higher of the fetched tier and your configured floor |
| Cannot go live by accident | Flag + config + environment variable, all three |

## What it does not protect against

Exchange insolvency or withdrawal freezes, API changes that break a connector,
a quote feed that is wrong rather than stale, regulatory and tax obligations,
and a fee tier that is higher than you configured. Keep only what you can
afford to lose on any exchange.

## Tests

```sh
cargo test
```

51 tests: strategy math with exact decimal expectations, risk state machine,
paper exchange fills, two-leg executor including unwind, Kraken HMAC-SHA512
against the official vector, Binance HMAC-SHA256 against the official vector,
Coinbase ES256 JWT round-trip verification, config validation, and an
end-to-end pipeline test (profitable cross, balance caps, stale quotes, retail
fees, STOP file, one-legged unwind, daily loss halt).

## Layout

```
src/main.rs            CLI: paper | live | check | record | replay | report
src/engine.rs          event loop: quotes -> strategy -> risk -> executor -> ledger
src/strategy/arb.rs    pure opportunity math
src/risk.rs            limits, kill switch, circuit breakers
src/executor.rs        concurrent IOC legs + unwind
src/exchanges/         trait, paper, coinbase, kraken, binance
src/ledger.rs          append-only JSONL audit log
src/replay.rs          quote recording and backtest
docs/PLAN.md           design, loss paths and their handling
docs/RESEARCH.md       fee data, why arbitrage fails, sources
```
