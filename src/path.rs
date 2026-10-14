// src/path.rs
//
// Reconstructs a token's on-chain price path for the 48 hours after a Gembot call.
//
// Replaces price_path.rs and pumpswap_path.rs. Three problems with those files are fixed here:
//
// 1. Coverage. They collected up to 500 transactions starting at the call, which on an active token can be only the first few minutes. 
//    This module instead samples the whole 48-hour window in time slices: a dense look at the first minute (so entry delays of 0, 15 and 30 seconds can be
//    tested), then progressively wider slices out to a final slice at the very end of the window, which provides a real end-of-window price instead of an early-window one.
//
// 2. Silent failures. A failed request used to be swallowed and reported as "no price data". Every request is now retried, and any request that still fails is counted, so "no trades found" and "lookup failed" are reported separately.
//
// 3. Versioned transactions. Helius returns only legacy transactions unless maxSupportedTransactionVersion is set, and a versioned transaction can load accounts through address lookup tables. Both are handled below.
//
// Price at each point is still derived from pre/post balance deltas in transaction metadata, exactly as before.

use crate::models::PricePoint;
use anyhow::{anyhow, Result};
use reqwest::Client;
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::task::JoinSet;

const PUMP_PROGRAM_ID: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
const PUMPSWAP_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

pub const WINDOW_SECS: i64 = 48 * 3600;

/// Requests in flight at once for a single mint. Mints are processed one at a time, so this is also the total concurrency against the RPC.
const CONCURRENCY: usize = 6;

/// Attempts per request before it is counted as failed.
const MAX_ATTEMPTS: usize = 3;

/// Highest transaction version we ask the RPC to return. Version-1 transactions exist on mainnet, and a request that meets one while declaring a lower limit fails outright (RPC error -32015), which loses the whole slice. 
// If an even newer version ever appears, the error names it and the request is retried with that version instead.
const MAX_TX_VERSION: u64 = 1;

/// Only the first few failure reasons are printed, so a systematic problem (for example a rejected parameter) is visible without flooding the log.
static FAILURE_LOGS: AtomicUsize = AtomicUsize::new(0);
const MAX_FAILURE_LOGS: usize = 3;

/// (start offset, end offset, max transactions), in seconds after the call. Each slice asks for the earliest transactions inside it, so a slice is a snapshot of the market at its start. 
// The first three slices line up with the 0, 15 and 30 second entry-delay scenarios. The last slice is the final ten minutes of the window and supplies the end-of-window price.
const SLICES: &[(i64, i64, usize)] = &[
    (0, 14, 30),
    (15, 29, 30),
    (30, 59, 30),
    (60, 299, 50),
    (300, 1_799, 50),
    (1_800, 3_599, 40),
    (3_600, 10_799, 40),
    (10_800, 21_599, 30),
    (21_600, 43_199, 30),
    (43_200, 86_399, 30),
    (86_400, 129_599, 30),
    (129_600, 172_199, 30),
    (172_200, 172_800, 20),
];

#[derive(Debug, Default)]
pub struct PathResult {
    /// Price points in chronological order, de-duplicated by transaction.
    pub points: Vec<PricePoint>,
    pub requests: usize,
    /// Requests that still failed after all retries.
    pub failed_requests: usize,
    /// True when the token traded on PumpSwap inside the window.
    pub graduated: bool,
    /// Hours between the call and the last price point.
    pub span_hours: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    Curve,
    Pool,
}

type Job = (Source, i64, i64, usize);

struct RawPoint {
    order: (i64, u64, u64),
    sig: String,
    price: f64,
}

pub fn derive_curve(mint: &Pubkey) -> Result<Pubkey> {
    let program = Pubkey::from_str(PUMP_PROGRAM_ID)?;
    let (pda, _) = Pubkey::find_program_address(&[b"bonding-curve", mint.as_ref()], &program);
    Ok(pda)
}

/// Canonical PumpSwap pool for a token that migrated from the Pump.fun bonding curve (pool index 0, quote = WSOL). Verified earlier in this project against a real on-chain pool address.
pub fn derive_pool(mint: &Pubkey) -> Result<Pubkey> {
    let pump_program = Pubkey::from_str(PUMP_PROGRAM_ID)?;
    let pumpswap_program = Pubkey::from_str(PUMPSWAP_PROGRAM_ID)?;
    let wsol = Pubkey::from_str(WSOL_MINT)?;
    let (pool_authority, _) = Pubkey::find_program_address(&[b"pool-authority", mint.as_ref()], &pump_program);
    let index_bytes: [u8; 2] = 0u16.to_le_bytes();
    let (pool, _) = Pubkey::find_program_address(
        &[b"pool", &index_bytes, pool_authority.as_ref(), mint.as_ref(), wsol.as_ref()],
        &pumpswap_program,
    );
    Ok(pool)
}

pub async fn fetch_path(http: &Client, rpc_url: &str, mint: &str, call_ts: i64) -> Result<PathResult> {
    let mint_pk = Pubkey::from_str(mint).map_err(|_| anyhow!("invalid mint address: {mint}"))?;
    let curve = derive_curve(&mint_pk)?.to_string();
    let pool = derive_pool(&mint_pk)?.to_string();
    let end_ts = call_ts + WINDOW_SECS;

    let mut result = PathResult::default();

    // Step 1: find when each address first trades inside the window. This avoids paying for empty slices on dead tokens and tells us when the token moved from the bonding curve to PumpSwap.
    let (curve_probe, pool_probe) = tokio::join!(
        gtfa(http, rpc_url, &curve, false, call_ts, end_ts, 1),
        gtfa(http, rpc_url, &pool, false, call_ts, end_ts, 1),
    );
    result.requests += 2;
    let curve_first = probe_start(curve_probe, call_ts, &mut result.failed_requests);
    let pool_first = probe_start(pool_probe, call_ts, &mut result.failed_requests);
    result.graduated = graduated(pool_first, call_ts);

    // Step 2: build the slice requests for whichever address was active in each part of the window.
    let jobs = plan(call_ts, curve_first, pool_first);

    // Step 3: run them a few at a time.
    let mut raw: Vec<RawPoint> = Vec::new();
    for chunk in jobs.chunks(CONCURRENCY) {
        let mut set = JoinSet::new();
        for &(src, a, b, limit) in chunk {
            let http = http.clone();
            let url = rpc_url.to_string();
            let addr = if src == Source::Curve { curve.clone() } else { pool.clone() };
            set.spawn(async move {
                let items = gtfa(&http, &url, &addr, true, a, b, limit).await;
                (src, addr, items)
            });
        }
        while let Some(joined) = set.join_next().await {
            result.requests += 1;
            match joined {
                Ok((src, addr, Some(items))) => {
                    for item in &items {
                        // Local guard in case the server ever returns anything outside the window.
                        match item["blockTime"].as_i64() {
                            Some(bt) if bt >= call_ts && bt <= end_ts => {}
                            _ => continue,
                        }
                        if let Some(p) = extract_point(item, src, &addr, mint) {
                            raw.push(p);
                        }
                    }
                }
                _ => result.failed_requests += 1,
            }
        }
    }

    raw.sort_by_key(|p| p.order);
    let mut seen: HashSet<String> = HashSet::new();
    for p in raw {
        if seen.insert(p.sig.clone()) {
            result.points.push(PricePoint { timestamp: p.order.0, price_sol: p.price });
        }
    }
    result.span_hours = result.points.last().map(|p| (p.timestamp - call_ts) as f64 / 3600.0).unwrap_or(0.0);
    Ok(result)
}

/// Interprets a probe response: Some(t) = first trade at time t, None = no trades in the window. A failed probe is counted and treated as "active for the whole window" so the slices still run and can find data on their own.
fn probe_start(probe: Option<Vec<Value>>, call_ts: i64, failed: &mut usize) -> Option<i64> {
    match probe {
        None => {
            *failed += 1;
            Some(call_ts - 1) // sentinel: unknown, assume active from the start
        }
        Some(items) => items.first().and_then(|i| i["blockTime"].as_i64()),
    }
}

/// A real first pool trade sits at or after the call. The failed-probe sentinel sits just before it and must not count as graduation.
fn graduated(pool_first: Option<i64>, call_ts: i64) -> bool {
    pool_first.map_or(false, |t| t >= call_ts)
}

/// Turns the probe results into slice requests. The bonding curve is queried up to the moment the pool first trades, and the pool from then on.
/// A probe sentinel (call_ts - 1) means "unknown", in which case the whole window is queried for that address.
fn plan(call_ts: i64, curve_first: Option<i64>, pool_first: Option<i64>) -> Vec<Job> {
    let end_ts = call_ts + WINDOW_SECS;
    let pool_known = pool_first.filter(|&t| t >= call_ts);
    let pool_unknown = pool_first == Some(call_ts - 1);

    let curve_range = curve_first.map(|t| {
        let start = t.max(call_ts);
        let end = pool_known.unwrap_or(end_ts).min(end_ts);
        (start, end)
    });
    let pool_range = if pool_unknown {
        Some((call_ts, end_ts))
    } else {
        pool_known.map(|t| (t, end_ts))
    };

    let mut jobs = Vec::new();
    for (src, range) in [(Source::Curve, curve_range), (Source::Pool, pool_range)] {
        let Some((lo, hi)) = range else { continue };
        for &(s, e, limit) in SLICES {
            let a = (call_ts + s).max(lo);
            let b = (call_ts + e).min(hi);
            if a <= b {
                jobs.push((src, a, b, limit));
            }
        }
    }
    jobs
}

/// One getTransactionsForAddress request, oldest first, with retries. Returns None only if every attempt failed. A successful request that finds nothing returns Some(empty), which is a real "no trades here" answer.
async fn gtfa(http: &Client, rpc_url: &str, address: &str, full: bool, from_ts: i64, to_ts: i64, limit: usize) -> Option<Vec<Value>> {
    let mut max_version = MAX_TX_VERSION;
    let mut last_error = String::new();
    let mut attempt = 0usize;
    let mut version_bumps = 0usize;

    while attempt < MAX_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(400 * 3u64.pow(attempt as u32 - 1))).await;
        }

        let mut config = json!({
            "transactionDetails": if full { "full" } else { "signatures" },
            "sortOrder": "asc",
            "limit": limit,
            "filters": { "blockTime": { "gte": from_ts, "lte": to_ts } },
        });
        if full {
            config["maxSupportedTransactionVersion"] = json!(max_version);
        }
        let body = json!({
            "jsonrpc": "2.0", "id": 1,
            "method": "getTransactionsForAddress",
            "params": [address, config],
        });

        let resp = match http.post(rpc_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("request error: {e}");
                attempt += 1;
                continue;
            }
        };
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            last_error = format!("HTTP {status}: {}", text.chars().take(200).collect::<String>());
            attempt += 1;
            continue;
        }
        let v = match resp.json::<Value>().await {
            Ok(v) => v,
            Err(e) => {
                last_error = format!("unreadable response: {e}");
                attempt += 1;
                continue;
            }
        };
        if let Some(err) = v.get("error") {
            // A newer transaction version than we declared: raise the limit and retry straight away, without using up an attempt.
            if let Some(n) = unsupported_version(err) {
                if full && n > max_version && version_bumps < 3 {
                    max_version = n;
                    version_bumps += 1;
                    continue;
                }
            }
            last_error = format!("RPC error: {}", err.to_string().chars().take(300).collect::<String>());
            attempt += 1;
            continue;
        }
        if let Some(arr) = v["result"]["data"].as_array() {
            return Some(arr.clone());
        }
        last_error = "response had no result.data".to_string();
        attempt += 1;
    }
    if FAILURE_LOGS.fetch_add(1, Ordering::Relaxed) < MAX_FAILURE_LOGS {
        eprintln!("  [REQUEST FAILED after {MAX_ATTEMPTS} attempts] {last_error}");
    }
    None
}

/// Reads the version number out of "Transaction version (N) is not supported by the requesting client".
fn unsupported_version(err: &Value) -> Option<u64> {
    let msg = err["message"].as_str()?;
    let rest = msg.split("Transaction version (").nth(1)?;
    rest.split(')').next()?.trim().parse().ok()
}

fn extract_point(item: &Value, src: Source, address: &str, mint: &str) -> Option<RawPoint> {
    if !item["meta"]["err"].is_null() {
        return None;
    }
    let block_time = item["blockTime"].as_i64()?;
    let price = match src {
        Source::Curve => curve_price(item, address, mint)?,
        Source::Pool => pool_price(item, address, mint)?,
    };
    let slot = item["slot"].as_u64().unwrap_or(0);
    let idx = item["transactionIndex"].as_u64().unwrap_or(0);
    let sig = item["transaction"]["signatures"][0]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("{block_time}-{slot}-{idx}"));
    Some(RawPoint { order: (block_time, slot, idx), sig, price })
}

/// Every account in the transaction, in the order balance arrays use: static keys, then loaded writable, then loaded readonly. 
// Accounts reached through an address lookup table are only listed under meta.loadedAddresses, so ignoring them would drop every trade where the bonding curve was loaded that way.
fn full_account_keys(item: &Value) -> Vec<String> {
    let mut keys = Vec::new();
    let mut parsed_objects = false;
    if let Some(arr) = item["transaction"]["message"]["accountKeys"].as_array() {
        for k in arr {
            if let Some(s) = k.as_str() {
                keys.push(s.to_string());
            } else if let Some(s) = k["pubkey"].as_str() {
                parsed_objects = true;
                keys.push(s.to_string());
            }
        }
    }
    // jsonParsed output already merges loaded accounts into accountKeys, so only add them for the plain format.
    if !parsed_objects {
        for list in ["writable", "readonly"] {
            if let Some(arr) = item["meta"]["loadedAddresses"][list].as_array() {
                keys.extend(arr.iter().filter_map(|k| k.as_str().map(String::from)));
            }
        }
    }
    keys
}

fn token_amount(entries: &Value, owner: &str, mint: &str) -> Option<(f64, u32)> {
    entries.as_array()?.iter().find_map(|e| {
        if e["mint"].as_str() == Some(mint) && e["owner"].as_str() == Some(owner) {
            let amount: f64 = e["uiTokenAmount"]["amount"].as_str()?.parse().ok()?;
            let decimals = e["uiTokenAmount"]["decimals"].as_u64()? as u32;
            Some((amount, decimals))
        } else {
            None
        }
    })
}

fn curve_price(item: &Value, curve: &str, mint: &str) -> Option<f64> {
    let keys = full_account_keys(item);
    let idx = keys.iter().position(|k| k == curve)?;
    let pre = item["meta"]["preBalances"].get(idx)?.as_i64()?;
    let post = item["meta"]["postBalances"].get(idx)?.as_i64()?;
    let sol = (post - pre).unsigned_abs() as f64 / 1e9;

    let (pre_amt, decimals) = token_amount(&item["meta"]["preTokenBalances"], curve, mint)?;
    let (post_amt, _) = token_amount(&item["meta"]["postTokenBalances"], curve, mint)?;
    let tokens = (post_amt - pre_amt).abs() / 10f64.powi(decimals as i32);

    if sol <= 0.0 || tokens <= 0.0 {
        return None;
    }
    Some(sol / tokens)
}

fn pool_price(item: &Value, pool: &str, mint: &str) -> Option<f64> {
    let pre = &item["meta"]["preTokenBalances"];
    let post = &item["meta"]["postTokenBalances"];
    let (pre_m, decimals) = token_amount(pre, pool, mint)?;
    let (post_m, _) = token_amount(post, pool, mint)?;
    let (pre_w, _) = token_amount(pre, pool, WSOL_MINT)?;
    let (post_w, _) = token_amount(post, pool, WSOL_MINT)?;

    let tokens = (post_m - pre_m).abs() / 10f64.powi(decimals as i32);
    let sol = (post_w - pre_w).abs() / 1e9;
    if sol <= 0.0 || tokens <= 0.0 {
        return None;
    }
    Some(sol / tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINT: &str = "MintMintMintMintMintMintMintMintMintMintMint";
    const CURVE: &str = "CurveCurveCurveCurveCurveCurveCurveCurveCurve";
    const POOL: &str = "PoolPoolPoolPoolPoolPoolPoolPoolPoolPoolPool";

    fn token_entry(owner: &str, mint: &str, amount: &str, decimals: u32) -> Value {
        json!({"mint": mint, "owner": owner, "uiTokenAmount": {"amount": amount, "decimals": decimals}})
    }

    #[test]
    fn legacy_curve_trade_prices_correctly() {
        // Curve gains 0.5 SOL and loses 1,000,000 tokens (6 decimals = 1.0 token): price 0.5 SOL per token.
        let item = json!({
            "blockTime": 100, "slot": 5, "transactionIndex": 2,
            "transaction": {"signatures": ["sig1"], "message": {"accountKeys": ["payer", CURVE, "other"]}},
            "meta": {
                "err": null,
                "preBalances": [10, 1_000_000_000u64, 0], "postBalances": [10, 1_500_000_000u64, 0],
                "preTokenBalances": [token_entry(CURVE, MINT, "5000000", 6)],
                "postTokenBalances": [token_entry(CURVE, MINT, "4000000", 6)],
                "loadedAddresses": {"writable": [], "readonly": []}
            }
        });
        let p = extract_point(&item, Source::Curve, CURVE, MINT).expect("point");
        assert!((p.price - 0.5).abs() < 1e-12, "price was {}", p.price);
        assert_eq!(p.order, (100, 5, 2));
    }

    #[test]
    fn versioned_curve_trade_with_lookup_table_account_is_not_dropped() {
        // The curve is NOT in the static keys; it sits at index 3 (2 static keys + 1 loaded writable before it).
        let item = json!({
            "blockTime": 200, "slot": 9, "transactionIndex": 1,
            "transaction": {"signatures": ["sig2"], "message": {"accountKeys": ["payer", "prog"]}},
            "meta": {
                "err": null,
                "preBalances": [1, 1, 7, 2_000_000_000u64], "postBalances": [1, 1, 7, 2_200_000_000u64],
                "preTokenBalances": [token_entry(CURVE, MINT, "10000000", 6)],
                "postTokenBalances": [token_entry(CURVE, MINT, "9000000", 6)],
                "loadedAddresses": {"writable": ["someloaded"], "readonly": [CURVE]}
            }
        });
        // keys = payer, prog, someloaded, CURVE -> index 3
        let p = extract_point(&item, Source::Curve, CURVE, MINT).expect("lookup-table trade must be found");
        assert!((p.price - 0.2).abs() < 1e-12, "price was {}", p.price);
    }

    #[test]
    fn pool_trade_prices_from_both_token_balances() {
        // Pool gains 2 SOL (WSOL, 9 decimals) and loses 4 tokens (6 decimals): 0.5 SOL per token.
        let item = json!({
            "blockTime": 300, "slot": 1, "transactionIndex": 0,
            "transaction": {"signatures": ["sig3"], "message": {"accountKeys": []}},
            "meta": {
                "err": null,
                "preTokenBalances": [token_entry(POOL, MINT, "10000000", 6), token_entry(POOL, WSOL_MINT, "5000000000", 9)],
                "postTokenBalances": [token_entry(POOL, MINT, "6000000", 6), token_entry(POOL, WSOL_MINT, "7000000000", 9)]
            }
        });
        let p = extract_point(&item, Source::Pool, POOL, MINT).expect("point");
        assert!((p.price - 0.5).abs() < 1e-12, "price was {}", p.price);
    }

    #[test]
    fn failed_transactions_are_ignored() {
        let item = json!({
            "blockTime": 100, "slot": 5, "transactionIndex": 2,
            "transaction": {"signatures": ["sig4"], "message": {"accountKeys": [CURVE]}},
            "meta": {"err": {"InstructionError": [0, "Custom"]}, "preBalances": [1], "postBalances": [2]}
        });
        assert!(extract_point(&item, Source::Curve, CURVE, MINT).is_none());
    }

    #[test]
    fn plan_for_dead_token_makes_no_requests() {
        assert!(plan(1_000, None, None).is_empty());
    }

    #[test]
    fn plan_for_token_that_never_graduates_uses_curve_only() {
        let jobs = plan(1_000, Some(1_000), None);
        assert!(!jobs.is_empty());
        assert!(jobs.iter().all(|j| j.0 == Source::Curve));
        assert_eq!(jobs.len(), SLICES.len());
        // Last slice ends exactly at the window end.
        assert_eq!(jobs.last().unwrap().2, 1_000 + WINDOW_SECS);
    }

    #[test]
    fn plan_splits_curve_and_pool_at_graduation() {
        let call = 1_000;
        let grad = call + 4_000;
        let jobs = plan(call, Some(call), Some(grad));
        let curve: Vec<_> = jobs.iter().filter(|j| j.0 == Source::Curve).collect();
        let pool: Vec<_> = jobs.iter().filter(|j| j.0 == Source::Pool).collect();
        assert!(curve.iter().all(|j| j.2 <= grad));
        assert!(pool.iter().all(|j| j.1 >= grad));
        assert_eq!(pool.last().unwrap().2, call + WINDOW_SECS);
    }

    #[test]
    fn plan_for_already_graduated_token_skips_the_curve() {
        let call = 1_000;
        let jobs = plan(call, None, Some(call + 2));
        assert!(jobs.iter().all(|j| j.0 == Source::Pool));
        assert!(!jobs.is_empty());
    }

    #[test]
    fn failed_probe_falls_back_to_querying_everything() {
        let call = 1_000;
        let mut failed = 0;
        let c = probe_start(None, call, &mut failed);
        let p = probe_start(None, call, &mut failed);
        assert_eq!(failed, 2);
        let jobs = plan(call, c, p);
        assert!(jobs.iter().any(|j| j.0 == Source::Curve));
        assert!(jobs.iter().any(|j| j.0 == Source::Pool));
        assert!(!graduated(p, call), "a failed probe must not be read as graduation");
        assert!(graduated(Some(call + 5), call));
    }

    #[test]
    fn newer_transaction_version_is_read_from_the_real_helius_error() {
        let err = json!({"code": -32015, "message": "Transaction version (1) is not supported by the requesting client. Please try the request again with the following configuration parameter: \"maxSupportedTransactionVersion\": 1"});
        assert_eq!(unsupported_version(&err), Some(1));
        let other = json!({"code": -32005, "message": "Node is behind"});
        assert_eq!(unsupported_version(&other), None);
    }

    #[test]
    fn slices_are_contiguous_and_cover_the_whole_window() {
        assert_eq!(SLICES.first().unwrap().0, 0);
        assert_eq!(SLICES.last().unwrap().1, WINDOW_SECS);
        for w in SLICES.windows(2) {
            assert_eq!(w[1].0, w[0].1 + 1, "gap or overlap between slices {:?} and {:?}", w[0], w[1]);
        }
    }
}
