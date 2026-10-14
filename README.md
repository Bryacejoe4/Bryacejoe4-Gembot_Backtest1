# Gembot call backtest

Simulates trading Gembot's calls and reports results by tier.

- Price path: reconstructed from on-chain balance changes (Pump.fun bonding curve, then PumpSwap), sampled across the full 48 hours after each call, with a dense first minute for entry-delay tests and an end-of-window price for valuing unsold tokens.
- Exit rule tested: sell 30% at +25%, 30% at +60%, hold 40% to the end of the window. Legs fill at their trigger price. Slippage 300 bps each way.
- Each run reports the baseline, 15 and 30 second entry delays, a stress table for how much of the unsold value is realisable, and a per-call results table.

Sample: 300 ultra_risky and 300 risky calls, random with seed 42 (`sample_large_tiers.csv`).
Results: `sample_large_tiers_results.csv` and `large_run.log`.

Headline on 561 priced calls: ultra_risky -18.7% per call, risky -49.3%. This supersedes the numbers in the earlier repo.

Run: `export RPC_URL=<Helius endpoint>; cargo run -- sample_large_tiers.csv`
