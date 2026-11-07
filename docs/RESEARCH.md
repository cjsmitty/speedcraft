# Research notes (2026-11-07)

## Fees (verify on each exchange's own page; sources conflict)

| Exchange | Maker | Taker | Note |
|---|---|---|---|
| Binance (global) | 0.10% | 0.10% | BNB discount available; VIP tiers lower |
| Binance.US | 0.00% | 0.02% | April 2026 cut, all pairs |
| Coinbase Advanced | 0.40–0.60% | 0.60–1.20% | lowest tier; falls with volume |
| Kraken Pro | 0.25% | 0.40% | lowest tier; ~0.10% taker at high volume |

Round-trip taker cost at the lowest tiers, Coinbase + Kraken: ~1.0%.
Typical BTC-USD cross-exchange top-of-book spread: 0.00–0.05%.
Conclusion: at retail tiers, after-fee opportunities are rare to nonexistent.
Binance.US + Binance global is the cheapest pairing but the two are legally
separate entities and most users cannot hold accounts on both.

## Why cross-exchange arbitrage fails in practice (sources agree)

- Fees exceed the spread (BloFin break-even formula: taker_A + taker_B +
  slippage + withdrawal% + buffer).
- Transfer time between venues ("the silent killer") — solved here by holding
  inventory on both sides and never transferring during trading.
- Opportunities last seconds; HFT firms with colocation take the good ones.
- Thin top-of-book depth means the quoted size is often tiny.
- Spreads have narrowed every year since 2017.

## API facts used in the connectors

See inline comments in `src/exchanges/*.rs`. Summary:

- Coinbase Advanced Trade: REST `https://api.coinbase.com`, auth via ES256
  JWT (CDP key `organizations/{org}/apiKeys/{id}` + EC private key PEM), JWT
  valid 120 s, one per request; WS `wss://advanced-trade-ws.coinbase.com`.
- Kraken: REST `https://api.kraken.com`, `API-Sign =
  base64(HMAC-SHA512(b64decode(secret), path + SHA256(nonce + postdata)))`,
  path must include `/0/private/...`; WS v2 `wss://ws.kraken.com/v2`.
- Binance: REST `https://api.binance.com` (or `api.binance.us`),
  `signature = HMAC-SHA256(secret, querystring)`, header `X-MBX-APIKEY`;
  WS `wss://stream.binance.com:9443/ws/<symbol>@bookTicker`.

## Sources

- https://nautilustrader.io/docs/latest/integrations/coinbase/
- https://docs.cloud.coinbase.com/advanced-trade/docs/changelog
- https://docs.kraken.com/exchange/guides/rest/authentication
- https://support.kraken.com/mx/articles/common-causes-of-invalid-key-error
- https://github.com/KenRoytman/binance-official-api-docs/blob/master/rest-api.md
- https://blofin.com/academy/education/trading/cross-exchange-arbitrage
- https://www.altrady.com/blog/crypto-trading-strategies/crypto-arbitrage-guide-2026
- https://www.bitget.com/wiki/does-arbitrage-still-work-in-crypto
- https://www.dextools.io/news/what-is-crypto-arbitrage-trading-guide-2026
- https://cointelegraph.com/news/binance-us-cuts-spot-trading-fees-to-near-zero-in-push-to-undercut-rivals
- https://www.dextools.io/tutorials/binance-vs-coinbase-vs-kraken-best-exchange-comparison-2026

## Altcoin scan results (2026-11-07)

Produced by `speedcraft scan`. "Arb net" is taker-taker arbitrage after both
fees and a 2 bps buffer; "Maker net" is a post-only quote on one venue hedged
with a taker order on the other. Retail fees: Coinbase 60/40 bps taker/maker,
Kraken 40/25, Binance.US 2/0. Volume tier: Coinbase 25/10, Kraken 20/10.
p95 = 95th percentile of the best directed edge across venue pairs. A positive
maker edge on a thin market does not mean fills happen; see the paper-run
notes below.

### Recording A: Coinbase + Kraken, 24 pairs, ~20 minutes, 158k quotes

| Pair | Samples | Spread coinbase (bps) | Spread kraken (bps) | Gross cross p95 (bps) | Arb net p95, retail | Arb net p95, volume | Maker net p95, retail | Maker positive, retail | Maker net p95, zero fees |
|---|---|---|---|---|---|---|---|---|---|
| SHIB/USD | 273 | 23.6 | 6.5 | -4.5 | -106.5 | -51.5 | -59.7 | 0% | 22.3 |
| PEPE/USD | 1756 | 24.1 | 5.4 | -2.0 | -104.0 | -49.0 | -61.7 | 0% | 20.3 |
| INJ/USD | 5173 | 8.1 | 8.9 | 7.4 | -94.6 | -39.6 | -64.8 | 0% | 19.6 |
| ATOM/USD | 1260 | 8.2 | 19.1 | -5.8 | -107.8 | -52.8 | -71.1 | 0% | 15.9 |
| XLM/USD | 3278 | 1.2 | 2.1 | 4.7 | -97.3 | -42.3 | -75.8 | 0% | 6.7 |
| ARB/USD | 3537 | 7.9 | 9.9 | -3.0 | -105.0 | -50.0 | -76.0 | 0% | 6.6 |
| OP/USD | 1701 | 7.9 | 13.8 | -3.9 | -105.9 | -50.9 | -76.3 | 0% | 9.8 |
| DOT/USD | 1073 | 5.7 | 4.1 | 0.0 | -102.0 | -47.0 | -76.5 | 0% | 5.5 |
| UNI/USD | 10497 | 3.9 | 3.9 | 1.3 | -100.7 | -45.7 | -77.0 | 0% | 5.1 |
| BCH/USD | 1178 | 5.0 | 5.3 | 0.0 | -102.0 | -47.0 | -77.4 | 0% | 5.4 |
| NEAR/USD | 14882 | 4.7 | 3.8 | -1.3 | -103.3 | -48.3 | -77.4 | 0% | 4.9 |
| HBAR/USD | 1002 | 3.8 | 4.1 | 1.5 | -100.5 | -45.5 | -77.6 | 0% | 4.4 |
| ONDO/USD | 1390 | 3.4 | 3.4 | 1.6 | -100.4 | -45.4 | -77.6 | 0% | 6.6 |
| LINK/USD | 27090 | 2.0 | 3.5 | 2.6 | -99.4 | -44.4 | -77.8 | 0% | 6.5 |
| LTC/USD | 4664 | 2.7 | 2.3 | 0.6 | -101.4 | -46.4 | -78.9 | 0% | 3.7 |
| DOGE/USD | 2727 | 2.7 | 0.9 | 0.4 | -101.6 | -46.6 | -79.2 | 0% | 2.8 |
| XRP/USD | 11048 | 1.4 | 1.0 | 1.7 | -100.3 | -45.3 | -79.3 | 0% | 3.1 |
| AVAX/USD | 1371 | 3.5 | 4.6 | -1.2 | -103.2 | -48.2 | -79.6 | 0% | 3.6 |
| SUI/USD | 2413 | 3.4 | 2.3 | 0.0 | -102.0 | -47.0 | -79.7 | 0% | 2.3 |
| AAVE/USD | 10209 | 4.9 | 6.0 | -2.3 | -104.3 | -49.3 | -79.7 | 0% | 3.8 |
| ETH/USD | 5979 | 0.1 | 0.2 | 1.9 | -100.1 | -45.1 | -80.0 | 0% | 2.0 |
| BTC/USD | 5779 | 0.0 | 0.0 | 1.5 | -100.5 | -45.5 | -80.4 | 0% | 1.7 |
| ADA/USD | 23254 | 3.3 | 3.6 | -1.4 | -103.4 | -48.4 | -80.7 | 0% | 2.6 |
| SOL/USD | 4519 | 1.4 | 1.3 | 0.0 | -102.0 | -47.0 | -80.9 | 0% | 2.2 |

Reading: nothing is positive at any realistic tier. The best maker-mode edge
at retail is PEPE at -62 bps. Altcoins' wider venue spreads (PEPE 24 bps on
Coinbase vs 0.0 for BTC) do not come close to covering a 65 to 100 bps fee
round trip.

### Recording B: Coinbase + Kraken + Binance.US, 24 pairs, ~20 minutes

Binance.US quotes were polled over REST at 1 Hz because the sandbox blocked
its stream port; on a normal network the WebSocket feed is used.

| Pair | Samples | Spread coinbase (bps) | Spread kraken (bps) | Spread binance_us (bps) | Gross cross p95 (bps) | Arb net p95, retail | Arb net p95, volume | Maker net p95, retail | Maker positive, retail | Maker net p95, zero fees |
|---|---|---|---|---|---|---|---|---|---|---|
| OP/USD | 2795 | 8.5 | 15.0 | 273.8 | -4.9 | -106.9 | -51.9 | 146.1 | 100% | 192.1 |
| ATOM/USD | 3094 | 8.3 | 17.8 | 107.4 | -5.8 | -87.8 | -52.8 | 22.6 | 66% | 73.3 |
| BCH/USD | 3206 | 4.9 | 5.3 | 27.2 | 22.4 | -23.5 | -3.5 | 2.2 | 30% | 45.8 |
| AAVE/USD | 20503 | 5.1 | 5.8 | 84.4 | -3.0 | -84.1 | -50.0 | 2.1 | 39% | 44.8 |
| DOT/USD | 2144 | 6.0 | 4.5 | 48.0 | -1.1 | -60.6 | -40.6 | -12.1 | 0% | 29.9 |
| PEPE/USD | 3539 | 23.8 | 5.4 | 32.8 | -2.0 | -48.1 | -28.1 | -15.6 | 0% | 26.4 |
| SHIB/USD | 1299 | 28.1 | 6.5 | 38.3 | -4.5 | -50.7 | -30.7 | -17.5 | 0% | 24.5 |
| NEAR/USD | 19628 | 5.2 | 4.3 | 12.7 | 2.4 | -41.8 | -21.8 | -21.7 | 0% | 15.1 |
| LTC/USD | 6243 | 2.8 | 2.6 | 17.9 | 0.8 | -47.8 | -27.8 | -22.8 | 0% | 19.3 |
| XLM/USD | 5577 | 1.5 | 1.8 | 9.4 | 5.4 | -38.7 | -18.7 | -23.1 | 0% | 14.4 |
| UNI/USD | 26315 | 3.5 | 4.6 | 26.1 | 0.6 | -51.2 | -31.2 | -23.2 | 0% | 20.3 |
| LINK/USD | 36306 | 2.1 | 4.2 | 6.0 | 3.7 | -44.3 | -24.0 | -24.5 | 0% | 10.1 |
| ETH/USD | 8042 | 0.1 | 0.2 | 0.2 | 4.9 | -40.4 | -20.4 | -25.3 | 0% | 5.1 |
| BTC/USD | 8424 | 0.0 | 0.0 | 0.0 | 4.5 | -40.5 | -20.5 | -25.5 | 0% | 4.5 |
| XRP/USD | 34517 | 1.4 | 1.2 | 1.0 | 3.4 | -41.6 | -21.6 | -25.5 | 0% | 5.1 |
| SOL/USD | 6408 | 1.5 | 1.4 | 2.1 | 2.2 | -41.8 | -21.8 | -25.7 | 0% | 4.4 |
| DOGE/USD | 4710 | 3.1 | 1.1 | 2.6 | 1.5 | -42.5 | -22.5 | -26.5 | 0% | 4.8 |
| AVAX/USD | 3121 | 3.9 | 4.7 | 6.6 | -1.2 | -45.2 | -25.2 | -26.6 | 0% | 7.3 |
| HBAR/USD | 2463 | 3.1 | 4.5 | 12.1 | 1.5 | -45.5 | -25.5 | -27.5 | 0% | 10.3 |
| ADA/USD | 27120 | 2.4 | 3.7 | 7.7 | -0.5 | -46.7 | -26.7 | -27.8 | 0% | 7.1 |
| SUI/USD | 3977 | 3.5 | 2.3 | 7.5 | 0.0 | -45.2 | -25.2 | -27.8 | 0% | 7.0 |
| INJ/USD | 4154 | 6.9 | 8.6 | nan | 6.1 | -95.9 | -40.9 | -64.8 | 0% | 18.4 |
| ONDO/USD | 1386 | 3.6 | 3.5 | nan | 1.4 | -100.6 | -45.6 | -77.6 | 0% | 6.6 |
| ARB/USD | 2822 | 6.4 | 9.6 | nan | -3.0 | -105.0 | -50.0 | -77.8 | 0% | 7.2 |

Reading: with a 0% maker / 0.02% taker venue in the mix, maker mode shows
positive edge at retail fees on OP, ATOM, AAVE and BCH, and BCH is near
break-even for plain arbitrage against Coinbase at a volume tier. The
Binance.US books for those coins are thin (OP's spread there is wide), so
treat these as "there is room" rather than "there is money" until a paper
run shows real fills.

### Paper run: maker mode quoting on Binance.US, hedging on Kraken

Configuration: 10 pairs (OP, ATOM, AAVE, BCH, PEPE, SHIB, DOT, NEAR, LTC,
UNI), target edge 5 bps, 50 USD per quote, Binance.US quotes polled at 1 Hz,
paper fills only when the venue's book trades through our resting price.

| Duration | Pairs quoting | Quotes placed | Fills | PnL |
|---|---|---|---|---|
| ~25 min | 8 of 10 (BCH/DOT/PEPE/SHIB sizes below minimums at times) | 11,084 | **0** | 0 |

Reading: the scan's positive "maker edge" on OP, ATOM and AAVE exists because
Binance.US's best bid and ask on those coins sit far from where Coinbase and
Kraken trade. A quote resting at that level is far from the real market, so
nothing hits it. Edge that never fills is not edge. Two things would change
this and both need a real account to test: (1) quoting closer to the hedge
venue's price (which only works at a genuinely low maker fee: 0% on
Binance.US, and still needs the hedge taker fee covered), and (2) a longer
run with the native WebSocket feed instead of 1 Hz polling, since fills
happen inside the second. Run this overnight on the Mac before drawing a
final conclusion, with the same config (`config/alts3.toml`, `[maker]`
enabled, `quote_exchange = "binance_us"`).
