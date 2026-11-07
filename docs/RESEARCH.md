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
