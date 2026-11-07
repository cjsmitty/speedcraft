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

What the bot measured on 10 minutes of real Coinbase and Kraken quotes
(6,108 top-of-book updates, BTC/USD and ETH/USD) replayed through the full
pipeline at three fee assumptions:

| Taker fees assumed | Best net edge, BTC/USD | Best net edge, ETH/USD | After-fee opportunities |
|---|---|---|---|
| Retail: 0.60% Coinbase, 0.40% Kraken | -100.9 bps | -99.6 bps | 0 |
| 0.10% on both (high-volume tier) | -20.9 bps | -19.6 bps | 0 |
| 0% (measures the raw spread ceiling) | +1.1 bps | +2.5 bps | gross only |

The largest cross-exchange price gap in the sample was 0.025%. Any real fee
tier turns that into a loss, so the bot did nothing, which is the correct
outcome. Run your own overnight `record` and `replay` before believing
anything else.

## The app

```sh
./install/make-mac-app.sh      # builds dist/Speedcraft.app (Rust via https://rustup.rs first)
open dist/Speedcraft.app       # or double-click; right-click → Open the first time (unsigned)
```

The app opens a Terminal window with logs and the control panel in your
browser at `http://127.0.0.1:7700`. From there you can start and stop the
local engine in paper or live mode, halt and resume trading, toggle the STOP
file, watch every cross-exchange edge against your fee tier in real time,
see balances and the trade ledger, edit and validate the config, and manage
any number of remote agents running on Linux VMs. Data lives in
`~/Library/Application Support/Speedcraft`.

Without the bundle, `speedcraft ui` from a directory with a `config.toml` does
the same thing.

### Remote agents on Linux VMs

Install the same binary as a headless, token-protected systemd service:

```sh
curl -fsSL https://raw.githubusercontent.com/cjsmitty/speedcraft/main/install/install-agent.sh | bash
```

The installer prints the agent's token and URL. Add it in the app under
"Remote agents" and switch to it with the tab at the top. The browser only
ever talks to the app on your Mac; the app forwards each command to the
agent with its token. Full details, SSH tunnel and Tailscale setups, and the
HTTP API are in `docs/REMOTE.md`.

## Build

```sh
cargo build --release
./target/release/speedcraft --help
```

Pure-Rust TLS with the system keychain as trust store. No OpenSSL, no Homebrew
dependencies. Apple Silicon, Intel Macs and Linux all build with the same
command.

## Run

```sh
cp config/example.toml config.toml   # edit fee floors, pairs, limits
./target/release/speedcraft check    # validates config, markets, (and keys if set)
./target/release/speedcraft paper    # live quotes, simulated fills, no keys needed
./target/release/speedcraft record --out quotes.jsonl --seconds 28800   # overnight capture
./target/release/speedcraft replay quotes.jsonl                          # backtest it
./target/release/speedcraft report ledger.jsonl
./target/release/speedcraft ui       # the control panel (see above)
./target/release/speedcraft agent    # headless mode for servers (see docs/REMOTE.md)
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

61 tests: strategy math with exact decimal expectations, risk state machine,
paper exchange fills, two-leg executor including unwind, Kraken HMAC-SHA512
against the official vector, Binance HMAC-SHA256 against the official vector,
Coinbase ES256 JWT round-trip verification, config validation, the HTTP API
(token enforcement, live-mode refusal, config validation, agent registry and
proxy error handling), the engine supervisor, and an end-to-end pipeline test
(profitable cross, balance caps, stale quotes, retail fees, STOP file,
one-legged unwind, daily loss halt).

The UI and the agent proxy were also exercised end to end against live
Coinbase and Kraken data: a UI host and a separate token-protected agent
process, agent registered through the API, status, halt, resume and stop
driven through the proxy, and the page rendered in headless Chromium with no
script errors.

## Layout

```
src/main.rs            CLI: paper | live | check | record | replay | report | ui | agent
src/server/            supervisor (start/stop/halt/resume), HTTP API, agent registry + proxy, web UI
src/runtime.rs         exchange construction and engine lifecycle shared by CLI, UI and agent
src/engine.rs          event loop: quotes -> strategy -> risk -> executor -> ledger
src/strategy/arb.rs    pure opportunity math
src/risk.rs            limits, kill switch, circuit breakers
src/executor.rs        concurrent IOC legs + unwind
src/exchanges/         trait, paper, coinbase, kraken, binance
src/ledger.rs          append-only JSONL audit log
src/replay.rs          quote recording and backtest
install/install-agent.sh   Linux: build + systemd service + token, one command
install/make-mac-app.sh    macOS: builds dist/Speedcraft.app
docs/PLAN.md           design, loss paths and their handling
docs/RESEARCH.md       fee data, why arbitrage fails, sources
docs/REMOTE.md         Mac app + Linux agents: topology, install, tunnels, API
```
