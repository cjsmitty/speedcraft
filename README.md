# Speedcraft

A crypto arbitrage bot in Rust with a point-and-click control panel, a
headless mode for Linux servers, and hard safety limits. Paper trading is the
default; real money needs three separate confirmations.

**New here? Read [docs/START_HERE.md](docs/START_HERE.md).** It says what to
run, when, and gives the straight answer on whether it makes money.

**The short version of that answer:** at normal retail fees it does not. That
is measured: the gaps between exchanges are 0.0 to 0.3%, the fees are about
1%, so the bot correctly sends no trades. The in-app ranking tool shows you
exactly how far each coin is from paying at your fees, and the panel at the
top of the screen projects earnings per day and month from your own data.

## Altcoins: what the data says

`speedcraft scan` ranks every pair in a recording by the cross-exchange edge
that actually existed, at retail, volume-tier and zero fees, for both
taker-taker arbitrage and maker mode. Two recordings of 24 USD pairs were
taken while building this (Nov 2026, ~20 minutes each; full tables in
`docs/RESEARCH.md`):

- **Coinbase + Kraken only:** no coin is positive in either mode at any
  realistic fee tier. The widest gap was INJ at 27 bps once; retail round-trip
  fees are 102 bps. Altcoins have wider spreads than BTC, but fees are a
  percentage, so "less fees in altcoins" is not a thing.
- **Adding Binance.US** (0% maker, 0.02% taker) changes it. Quoting on
  Binance.US and hedging on Kraken shows positive maker-mode edge at retail
  fees on OP, ATOM, AAVE and BCH, and BCH is within a few bps of break-even
  even for plain arbitrage at a Coinbase volume tier. Caveat: those Binance.US
  books are thin, and a resting quote only earns when someone hits it. Run
  maker mode in paper on those pairs and read the fill count before
  believing the scan.

```sh
./target/release/speedcraft -c config/alts3.toml record --out alts.jsonl --seconds 28800
./target/release/speedcraft -c config/alts3.toml scan alts.jsonl
```

`config/alts.toml` (Coinbase + Kraken) and `config/alts3.toml` (plus
Binance.US) map the 24-coin basket. Kraken's XBT/XDG legacy names are handled.

## Futures carry (Coinbase Derivatives)

The carry card measures cash-and-carry on Coinbase's dated futures: buy
spot, sell the future, hold to expiry, pocket the premium. Public data only;
it discovers the CDE contracts (nano BTC, ETH, SOL, XRP and more), polls
their books, computes the locked return net of your spot fee and the
per-contract futures fee, estimates margin, and keeps paper positions.
Perpetual-style contracts (2089 expiry) show basis only since their funding
rate is not public. No futures orders are placed in this version.

## Maker mode

Taker-taker arbitrage pays two taker fees. Maker mode rests **post-only**
quotes on the cheapest-maker venue, priced off the other venue's book so that
a fill plus an immediate taker hedge still nets `target_edge_bps`. You earn the
spread instead of paying it. The cost is adverse selection, which shows up
honestly in realised PnL. Enable it in the `[maker]` section of the config;
the UI shows resting quotes, inventory and fills per pair. Fills are hedged
with the same crossing logic that unwinds one-legged arbitrage, and every
halt, STOP file or shutdown cancels all resting quotes first. Live post-only
and cancel are implemented for Coinbase (`limit_limit_gtc` + `post_only`),
Kraken (`oflags=post`) and Binance/Binance.US (`LIMIT_MAKER`).

## Download

Prebuilt packages for every tagged version are on the
[Releases page](https://github.com/cjsmitty/speedcraft/releases): a universal
`Speedcraft.app` for macOS (Apple Silicon and Intel) and Linux agent binaries
for x86_64 and ARM, with SHA-256 checksums. They are built by the
`Release` GitHub Actions workflow; `CI` runs format, clippy and the test
suite on Linux and macOS for every push.

The macOS app is ad-hoc signed unless the maintainer configures Developer ID
secrets, so on first launch right-click → Open, or run
`xattr -dr com.apple.quarantine /Applications/Speedcraft.app`.

To cut a release: `git tag v0.3.0 && git push origin v0.3.0`.

## The app

```sh
# or build it yourself:
./install/make-mac-app.sh      # builds dist/Speedcraft.app (Rust via https://rustup.rs first)
open dist/Speedcraft.app       # right-click → Open the first time (ad-hoc signed)
```

The app runs in the background (no window, no Dock icon) and opens the
control panel in your browser at `http://127.0.0.1:7700`; quit it from the
panel's Quit button. Logs go to `~/Library/Logs/Speedcraft/speedcraft.log`. From there you can start and stop the
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
command. WebSocket connections honor `HTTPS_PROXY`, and the Binance connector
falls back to polling the REST book ticker at 1 Hz if its stream port (9443)
is blocked.

## Run

```sh
cp config/example.toml config.toml   # edit fee floors, pairs, limits
./target/release/speedcraft check    # validates config, markets, (and keys if set)
./target/release/speedcraft paper    # live quotes, simulated fills, no keys needed
./target/release/speedcraft record --out quotes.jsonl --seconds 28800   # overnight capture
./target/release/speedcraft replay quotes.jsonl                          # backtest it
./target/release/speedcraft report ledger.jsonl
./target/release/speedcraft scan quotes.jsonl   # rank pairs by real edge at several fee tiers
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
3. Enter credentials in the app: ⚙ Settings → Exchanges & keys, then "Test
   connection". They are saved to `secrets.toml` (mode 0600) next to the
   config. For headless agents you can instead export environment variables,
   which take precedence:

   | Exchange | Variables |
   |---|---|
   | Coinbase Advanced | `COINBASE_API_KEY_NAME` (`organizations/.../apiKeys/...`), `COINBASE_API_PRIVATE_KEY` (EC PEM; `\n` escapes accepted) |
   | Kraken | `KRAKEN_API_KEY`, `KRAKEN_API_SECRET` |
   | Binance | `BINANCE_API_KEY`, `BINANCE_API_SECRET` |
   | Binance.US | `BINANCE_US_API_KEY`, `BINANCE_US_API_SECRET` |

4. Switch on both toggles under Settings → Live trading (or set `live = true`
   in `config.toml` and `accept_losses = true` in `secrets.toml`, or export
   `SPEEDCRAFT_I_ACCEPT_LOSSES=yes`), then press "Start live trading" and type
   the phrase (CLI: `speedcraft live --live`). All three are required.
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
| Cannot go live by accident | Typed phrase + settings switch + loss acknowledgement, all three |

## What it does not protect against

Exchange insolvency or withdrawal freezes, API changes that break a connector,
a quote feed that is wrong rather than stale, regulatory and tax obligations,
and a fee tier that is higher than you configured. Keep only what you can
afford to lose on any exchange.

## Tests

```sh
cargo test
```

77 tests: strategy math with exact decimal expectations, maker quote math,
maker manager end to end (rest, fill, hedge, inventory cap, halt cancels),
the scan ranker, risk state machine,
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
src/strategy/maker.rs  pure maker quote math
src/maker.rs           maker manager: rest / cancel / poll / hedge
src/scan.rs            rank pairs in a recording by real edge
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
