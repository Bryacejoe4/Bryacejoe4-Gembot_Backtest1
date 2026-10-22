"""Reproduces the 600-call large-tier sample used in the backtest.

Usage: python3 scripts/make_sample.py data/gembot_data.txt data/sample_large_tiers_check.csv

Draws 300 ultra_risky and 300 risky calls at random with seed 42 from the full export. The result should be byte-identical to data/sample_large_tiers.csv.
"""
import csv
import random
import sys

src, dst = sys.argv[1], sys.argv[2]

with open(src, encoding="utf-8-sig", newline="") as f:
    rows = list(csv.DictReader(f))

random.seed(42)
ultra = [r for r in rows if r["feed"] == "ultra_risky"]
risky = [r for r in rows if r["feed"] == "risky"]
sample = random.sample(ultra, 300) + random.sample(risky, 300)

with open(dst, "w", encoding="utf-8", newline="") as f:
    w = csv.DictWriter(f, fieldnames=["mint", "feed", "called_at", "call_market_cap", "peak_mult"])
    w.writeheader()
    w.writerows(sample)

print(f"export rows: {len(rows)} | ultra_risky: {len(ultra)} | risky: {len(risky)} | sample written: {len(sample)} rows -> {dst}")
