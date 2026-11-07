# Start here (plain English)

## What this is

Speedcraft watches the price of coins on several exchanges at the same time.
When the same coin is cheaper on one exchange than another by more than the
fees, it buys on the cheap one and sells on the dear one in the same instant.
That is the only kind of trade it makes. It never guesses which way prices
will go.

It also has a second trick, "maker mode", where it leaves standing offers on
the exchange with the lowest fees and, when someone takes an offer, instantly
does the opposite trade elsewhere.

## Can it make money? The honest answer

**At normal retail fees, no.** This was measured, not guessed. Over 20
minutes on 24 coins across Coinbase and Kraken, the biggest price gap seen was
0.27%, while the two exchanges' fees add up to about 1%. Every possible trade
would have lost money, so the bot made none. That is the bot working
correctly.

**What would change that:**

- Much lower fees. Binance.US charges 0% to leave an offer and 0.02% to take
  one. With it in the mix the numbers turn positive on paper for a few coins
  (OP, ATOM, AAVE, BCH), but those markets are so thin that in a 25-minute
  test nobody took a single one of the 11,084 offers the bot left. An offer
  that nobody takes earns nothing.
- Trading volume. Both Coinbase and Kraken cut fees sharply once you trade
  hundreds of thousands of dollars a month.

**How much, over what period?** The app tells you, from your own data, on the
"Can this make money?" panel: what it made so far, and what that rate works
out to per day and per month. Until you have run it overnight the honest
number is "unknown, probably zero". Anyone quoting you a number before
measuring is guessing.

**What it cannot lose:** more than the daily limit you set (default $25),
because it stops itself. What it can lose: that limit, every day you leave it
in live mode, plus whatever an exchange itself loses if it fails.

## What to run, and when

Easiest: download `Speedcraft-<version>-macos.app.zip` from
https://github.com/cjsmitty/speedcraft/releases, unzip it, drag
`Speedcraft.app` to Applications, then right-click it and choose Open (the
first time only; it is not from the App Store).

Or build it yourself (needs Rust once, from https://rustup.rs):

```sh
git clone https://github.com/cjsmitty/speedcraft
cd speedcraft
./install/make-mac-app.sh
open dist/Speedcraft.app
```

Either way a window opens in your browser. Everything else happens there.

**Day 1, any time:** press **Start watching**. Leave it on. This is safe:
real prices, pretend money, no orders.

**Day 1, before bed:** press **Record overnight** (step 2 on the screen).
The bot saves every price it sees to a file.

**Day 2, morning:** press **Rank the coins** (step 3). You get a table of
which coins came closest to paying at your fees, with a verdict for each.

**If the verdict is "no" for every coin** (expected at retail fees): you are
done. Leave it in paper mode as long as you like; it costs nothing. Check
the ranking again if your fee tier drops or you add an exchange.

**If a coin shows "yes":** run paper mode for a full week and watch the
"Can this make money?" panel. If it stays positive, read the live-trading
section in the README, fund both exchanges, and go live with small limits.

## Reading the screen

- **Start watching** runs the bot with pretend money. **Stop** turns it off.
  **Emergency stop** halts orders instantly but keeps watching.
- **Can this make money?** shows money made so far in this run, and that
  rate per day and per month. "Paper" results are optimistic: real fills are
  harder to get.
- **Price gaps between exchanges** lists every coin and every direction. The
  money column is per $10,000 traded, so "$103 short" means that trading
  $10,000 would have lost $103 after fees.
- **Which coins came closest** appears after you rank a recording.
- The **Glossary** button at the top explains every word used on the screen.

## Running it on a server so it never sleeps

Install the agent on any Linux box or VM with one command (see
`docs/REMOTE.md`), add it under "Advanced → Other computers", and control it
from the same screen. Your Mac can then sleep.
