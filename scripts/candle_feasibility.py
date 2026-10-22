"""Feasibility check, version 2: do GeckoTerminal 1-minute candles exist for our September calls, and are they consistent with our on-chain entry prices?

Usage: python3 scripts/candle_feasibility.py results/sample_large_tiers_results.csv [how_many_calls]

Read-only. Only Pump.fun and PumpSwap pools are used (other venues quote different prices). Responses are cached in .cache_gt/, so a repeat run costs no requests.

Agreement test: our entry price should lie inside the high-low range of the candles around the call time (+-2 minutes, 5% tolerance). The first version compared the open of
the next full minute, which can be up to 59 seconds after the call and so is not a fair test on fast tokens.
"""
import csv
import hashlib
import json
import os
import statistics
import sys
import time

import requests

BASE = os.environ.get("GT_BASE", "https://api.geckoterminal.com/api/v2")
PAUSE = float(os.environ.get("GT_PAUSE", "6.0"))   # the free tier rate-limited us at 2.3 s; about 10 requests a minute is safe
WINDOW = 6 * 3600
CACHE = ".cache_gt"
os.makedirs(CACHE, exist_ok=True)
sent = 0
started = time.time()


def get(path, params=None):
    global sent
    key = hashlib.sha1((path + json.dumps(params, sort_keys=True)).encode()).hexdigest()
    cached = os.path.join(CACHE, key + ".json")
    if os.path.exists(cached):
        return 200, json.load(open(cached))
    for attempt in range(4):
        time.sleep(PAUSE)
        sent += 1
        try:
            r = requests.get(BASE + path, params=params, headers={"Accept": "application/json"}, timeout=30)
        except requests.RequestException as e:
            print("   request error:", e)
            continue
        if r.status_code == 429:
            print("   rate limited, waiting 45s")
            time.sleep(45)
            continue
        if r.status_code != 200:
            return r.status_code, r.text[:200]
        data = r.json()
        json.dump(data, open(cached, "w"))
        return 200, data
    return None, "failed after retries"


results_csv = sys.argv[1]
limit = int(sys.argv[2]) if len(sys.argv) > 2 else 30
rows = [r for r in csv.DictReader(open(results_csv, encoding="utf-8")) if r["status"] == "scored"][:limit]
print(f"checking {len(rows)} scored calls from {results_csv}\n")

with_candles = contained = checked = 0
first_hour_counts = []
for i, r in enumerate(rows, 1):
    mint, t0, entry = r["mint"], int(r["called_at_unix"]), float(r["entry_price_sol"])
    code, body = get(f"/networks/solana/tokens/{mint}/pools")
    if code != 200 or not body.get("data"):
        print(f"[{i}/{len(rows)}] {mint[:8]} no pools listed (status {code})")
        continue
    pools = []
    for p in body["data"]:
        dex = p.get("relationships", {}).get("dex", {}).get("data", {}).get("id", "?")
        if "pump" in dex:
            pools.append((p["attributes"]["address"], dex))
    if not pools:
        print(f"[{i}/{len(rows)}] {mint[:8]} no Pump.fun/PumpSwap pool listed (others: {[p['relationships']['dex']['data']['id'] for p in body['data'] if 'relationships' in p][:3]})")
        continue

    candles = []
    for addr, dex in pools[:2]:
        code, ob = get(f"/networks/solana/pools/{addr}/ohlcv/minute",
                       {"aggregate": 1, "before_timestamp": t0 + WINDOW, "limit": 400, "currency": "token", "token": "base"})
        if code != 200:
            print(f"   {dex}: candles request failed ({code}) {ob}")
            continue
        candles += [(c[0], c[1], c[2], c[3], c[4]) for c in ob["data"]["attributes"]["ohlcv_list"]]
    after = sorted(c for c in candles if t0 <= c[0] <= t0 + WINDOW)
    near = [c for c in candles if t0 - 120 <= c[0] <= t0 + 120]
    first_hour = [c for c in after if c[0] <= t0 + 3600]
    first_hour_counts.append(len(first_hour))
    if after:
        with_candles += 1
    if near:
        checked += 1
        lo, hi = min(c[3] for c in near), max(c[2] for c in near)
        ok = lo * 0.95 <= entry <= hi * 1.05
        contained += ok
        verdict = "inside" if ok else f"OUTSIDE (entry/low {entry / lo:.2f}, entry/high {entry / hi:.2f})"
    else:
        verdict = "no candle within 2 min of the call"
    print(f"[{i}/{len(rows)}] {mint[:8]} pools={[d for _, d in pools]} candles_after={len(after)} first_hour={len(first_hour)} entry price {verdict}")

mins = max(1e-9, (time.time() - started) / 60)
print("\n=== SUMMARY ===")
print(f"calls checked: {len(rows)} | with candles after the call: {with_candles} | with a candle near the call time: {checked}")
if first_hour_counts:
    print(f"candles in the first hour (minutes with trades only): median {statistics.median(first_hour_counts)}")
if checked:
    print(f"our entry price inside the candle high-low range: {contained}/{checked} = {contained / checked * 100:.0f}%")
print(f"requests sent: {sent} in {mins:.1f} minutes = {sent / mins:.1f} per minute")
