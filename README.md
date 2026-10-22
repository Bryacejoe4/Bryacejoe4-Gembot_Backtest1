# Gembot call backtest

Simulates trading Gembot's calls with a fixed exit rule and reports results by tier. Prices are rebuilt from on-chain balance changes, not taken from Gembot's figures.

## Result (supersedes the earlier repo)

1494 calls were tested on this code: a random 600-call sample of the two large tiers (300 ultra_risky, 300 risky; seed 42) and all 894 calls of the three small tiers. 1372 had prices; 122 had no Pump.fun or PumpSwap trades and are excluded; 0 lookups failed.

Return per call on a 0.1 SOL position, priced calls only:

| tier | calls | priced | win rate | average | median | average without the 5 best calls |
|---|---|---|---|---|---|---|
| ultra_risky | 300 | 278 | 8.3% | -18.7% | -39.7% | -38.1% |
| risky | 300 | 283 | 3.9% | -49.3% | -57.0% | -52.1% |
| balanced | 839 | 762 | 7.6% | -49.6% | -63.5% | -59.4% |
| precision | 39 | 35 | 5.7% | +81.8% | -61.6% | -65.1% |
| conservative | 16 | 14 | 7.1% | -40.1% | -17.9% | -59.2% |

Every tier is negative on the median and on the average without the five best calls. The positive average for precision is one call (+4,813%); the same token also appears under balanced. Rows for the same token under different feeds are not independent (134 tokens appear in more than one row), so tier rows should not be added together as if they were separate bets.

Both large tiers are negative even if every unsold token could be sold at its marked value. The full tables (stress, entry delay, coverage) are at the end of each log in `results/`, and every call is in the matching `*_results.csv`.

The earlier repo reported +24.1% and +14.2% for the two large tiers. Those numbers were wrong: unsold tokens were valued at prices from the first minutes after each call, and the data pull missed some transactions (newer transaction versions are only returned when requested).

## Method

- Price path: Pump.fun bonding curve, then PumpSwap, sampled across the 48 hours after each call in time slices, with a dense first minute for entry-delay tests and an end-of-window price for unsold tokens.
- Exit rule tested: sell 30% at +25%, 30% at +60%, hold 40% to the end of the window. Legs fill at their trigger price. Slippage 300 bps each way plus a network fee. Position 0.1 SOL.
- Each run reports the baseline, 15 and 30 second entry delays, a stress table for how much of the unsold value is realisable, and a per-call results table.

## Reproduce

    python3 scripts/make_sample.py data/gembot_data.txt /tmp/sample_check.csv   # must match data/sample_large_tiers.csv
    export RPC_URL=<Helius endpoint>
    cargo run -- data/sample_large_tiers.csv      # large tiers, results/large_run.log
    cargo run -- data/small_part_00.csv           # small tiers, in three parts (00, 01, 02)

## Data

`data/gembot_data.txt` is the export supplied by Gembot's team (mint, feed, called_at, call_market_cap, peak_mult), included for verification. Its `peak_mult` column is Gembot's own figure and is never used. `data/pilot_894.csv` holds the small-tier calls and `data/small_part_*.csv` the three parts it was split into.

## Limitations

- After the first minutes the price path is a set of snapshots, not every trade, so brief moves between snapshots are not seen.
- A minority of priced calls have a trade in the final hour, so the end-of-window price can be stale.
- Calls with no Pump.fun or PumpSwap trades are excluded. They are likely tokens launched elsewhere, which this tool cannot price.
- Fixed 300 bps slippage; no model of pool depth.
- One exit rule is tested, on three weeks of data (September 2026).
- A request that fails is retried and counted; one large-tier call had one failed request (a timeout).

## Status

A pre-registered research step on alternative exit rules, with a random-token control group, is next.
