"""Feasibility check for the control group: can we list random Pump.fun token creations from the Pump.fun mint authority address?

Usage: export RPC_URL=<Helius endpoint>; python3 scripts/control_discovery_test.py

Read-only. Picks 5 random moments between 1 and 19 September 2026 and asks Helius for the token creations that followed each one (oldest first). Every Pump.fun creation includes
the Pump.fun mint authority account, and the new mint signs the transaction, so the mint is the signer whose address ends in "pump".
"""
import datetime
import os
import random
import sys

import requests

RPC = os.environ["RPC_URL"]
MINT_AUTHORITY = "TSLvdd1pWpHVjahSpsvCXUbgwsL3JAcvokwaKt1eokM"
START = int(datetime.datetime(2026, 9, 1, tzinfo=datetime.timezone.utc).timestamp())
END = int(datetime.datetime(2026, 9, 19, tzinfo=datetime.timezone.utc).timestamp())


def mint_from_tx(item):
    """The new token's mint: the signer whose address ends in 'pump'."""
    msg = item["transaction"]["message"]
    keys = [k if isinstance(k, str) else k["pubkey"] for k in msg["accountKeys"]]
    n_signers = msg.get("header", {}).get("numRequiredSignatures", 1)
    for k in keys[:n_signers]:
        if k.endswith("pump"):
            return k
    return None


def main():
    random.seed(7)
    total_tx = total_mints = failed_tx = 0
    for i in range(5):
        t = random.randint(START, END)
        body = {
            "jsonrpc": "2.0", "id": 1, "method": "getTransactionsForAddress",
            "params": [MINT_AUTHORITY, {
                "transactionDetails": "full", "sortOrder": "asc", "limit": 10,
                "maxSupportedTransactionVersion": 1,
                "filters": {"blockTime": {"gte": t, "lte": t + 900}},
            }],
        }
        r = requests.post(RPC, json=body, timeout=60)
        when = datetime.datetime.fromtimestamp(t, datetime.timezone.utc).strftime("%Y-%m-%d %H:%M")
        if r.status_code != 200 or "error" in r.json():
            print(f"[{i + 1}] {when} request failed: {r.status_code} {r.text[:200]}")
            continue
        items = r.json()["result"]["data"]
        mints = []
        for it in items:
            total_tx += 1
            if it["meta"].get("err") is not None:
                failed_tx += 1
                continue
            m = mint_from_tx(it)
            if m:
                mints.append(m)
        total_mints += len(mints)
        print(f"[{i + 1}] {when} transactions={len(items)} mints_found={len(mints)} sample={[m[:10] for m in mints[:3]]}")
    print(f"\n=== SUMMARY ===\ntransactions returned: {total_tx} | failed: {failed_tx} | mints extracted: {total_mints}")
    if total_tx:
        print(f"share of successful transactions that yielded a mint: {total_mints / max(1, total_tx - failed_tx) * 100:.0f}%")


if __name__ == "__main__":
    main()
