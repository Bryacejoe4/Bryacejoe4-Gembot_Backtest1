// src/main.rs
//
// Entry point for the Gembot call backtest.
//
// For every call in the input CSV this reconstructs the token's on-chain price path across the full 48 hours after the call, then simulates the scale-out exit strategy three times from that same path: entering at the first trade at or after the call, 15 seconds after it, and 30 seconds after it. 
// One run therefore produces the baseline, the entry-delay sensitivity, and the stress tables the report needs.
//
// Results stream to the screen as each call finishes, and a per-call results CSV is written at the end next to the input file.

mod models;
mod path;
mod simulator;

use anyhow::{Context, Result};
use models::{CallRecord, ExitLeg, Outcome, PricePoint, SimResult};
use reqwest::Client;
use std::collections::BTreeMap;

const ENTRY_SOL: f64 = 0.1;
const DELAYS_SECS: [i64; 3] = [0, 15, 30];
/// Share of the unsold tokens' marked value assumed to be realisable. 0.0 is the "moonbag valued at zero" stress test.
const HAIRCUTS: [f64; 5] = [1.0, 0.85, 0.75, 0.5, 0.0];
const TIER_ORDER: [&str; 5] = ["ultra_risky", "risky", "balanced", "precision", "conservative"];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Scored,
    NoTrades,
    LookupFailed,
}

struct Row {
    call: CallRecord,
    status: Status,
    /// One simulation per entry delay, same order as DELAYS_SECS. Empty unless the call was scored.
    sims: Vec<SimResult>,
    requests: usize,
    failed: usize,
    points: usize,
    span_hours: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url = std::env::var("RPC_URL").context("RPC_URL must be set, e.g. your Helius endpoint")?;
    let csv_path = std::env::args().nth(1).context("usage: gembot-backtest <path-to-calls.csv>")?;

    let calls = read_calls(&csv_path)?;
    println!("Loaded {} calls from {}\n", calls.len(), csv_path);

    let http = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("failed to build HTTP client");
    let exit_legs = simulator::default_exit_legs();

    let total = calls.len();
    let mut rows: Vec<Row> = Vec::with_capacity(total);
    for (i, call) in calls.into_iter().enumerate() {
        eprintln!("--- [{}/{}] {} ({}) ---", i + 1, total, &call.mint_address[..8.min(call.mint_address.len())], call.tier);

        let row = match path::fetch_path(&http, &rpc_url, &call.mint_address, call.call_timestamp).await {
            Ok(fetched) => build_row(call, fetched, &exit_legs),
            Err(e) => {
                eprintln!("  [ERROR] {e}");
                Row { call, status: Status::LookupFailed, sims: vec![], requests: 0, failed: 1, points: 0, span_hours: 0.0 }
            }
        };
        println!("{}", stream_line(i + 1, total, &row));
        rows.push(row);
    }

    print_report(&rows, &exit_legs);

    let out_path = results_path(&csv_path);
    write_results_csv(&out_path, &rows)?;
    println!("\nPer-call results table written to {out_path}");
    Ok(())
}

fn read_calls(path: &str) -> Result<Vec<CallRecord>> {
    let mut reader = csv::Reader::from_path(path).with_context(|| format!("could not open {path}"))?;
    let mut calls = Vec::new();
    for record in reader.deserialize() {
        let call: CallRecord = record.context("malformed row in calls CSV")?;
        calls.push(call);
    }
    Ok(calls)
}

fn build_row(call: CallRecord, fetched: path::PathResult, exit_legs: &[ExitLeg]) -> Row {
    let points = fetched.points.len();
    let mut row = Row {
        status: Status::NoTrades,
        sims: vec![],
        requests: fetched.requests,
        failed: fetched.failed_requests,
        points,
        span_hours: fetched.span_hours,
        call,
    };

    if fetched.points.is_empty() {
        row.status = if fetched.failed_requests > 0 { Status::LookupFailed } else { Status::NoTrades };
        return row;
    }

    let sims: Vec<SimResult> = DELAYS_SECS
        .iter()
        .map(|&delay| {
            let cutoff = row.call.call_timestamp + delay;
            let delayed: Vec<PricePoint> = fetched.points.iter().filter(|p| p.timestamp >= cutoff).cloned().collect();
            simulator::simulate_call(&row.call.mint_address, &row.call.tier, &delayed, ENTRY_SOL, exit_legs)
        })
        .collect();

    if sims[0].outcome == Outcome::NoData {
        row.status = Status::NoTrades;
    } else {
        row.status = Status::Scored;
        row.sims = sims;
    }
    row
}

/// Median of the values, or 0.0 when empty.
fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mid = v.len() / 2;
    if v.len() % 2 == 1 { v[mid] } else { (v[mid - 1] + v[mid]) / 2.0 }
}

/// Average P&L per call after removing the `k` best calls. Shows whether the result depends on a handful of outsized winners.
fn avg_without_top(values: &[f64], k: usize) -> f64 {
    if values.len() <= k {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(|a, b| b.partial_cmp(a).unwrap());
    let rest = &v[k..];
    rest.iter().sum::<f64>() / rest.len() as f64
}

fn pct(sol: f64) -> f64 {
    sol / ENTRY_SOL * 100.0
}

fn stream_line(i: usize, total: usize, row: &Row) -> String {
    let head = format!("[{}/{}] {} [{}]", i, total, row.call.mint_address, row.call.tier);
    match row.status {
        Status::NoTrades => format!("{head} -- NO PUMP.FUN/PUMPSWAP TRADES FOUND ({} requests, 0 failed)", row.requests),
        Status::LookupFailed => format!("{head} -- LOOKUP FAILED ({} of {} requests failed)", row.failed, row.requests),
        Status::Scored => {
            let d0 = &row.sims[0];
            let delayed = |k: usize| match row.sims[k].outcome {
                Outcome::NoData => "n/a".to_string(),
                _ => format!("{:+.4}", row.sims[k].total_pnl_sol),
            };
            format!(
                "{head} pnl={:+.4} SOL ({:+.1}%)  d15={} d30={}  span={:.1}h pts={} failed={}",
                d0.total_pnl_sol, d0.total_pnl_pct, delayed(1), delayed(2), row.span_hours, row.points, row.failed
            )
        }
    }
}

fn tier_groups(rows: &[Row]) -> Vec<(String, Vec<&Row>)> {
    let mut map: BTreeMap<String, Vec<&Row>> = BTreeMap::new();
    for r in rows {
        map.entry(r.call.tier.clone()).or_default().push(r);
    }
    let mut ordered: Vec<(String, Vec<&Row>)> = Vec::new();
    for t in TIER_ORDER {
        if let Some(v) = map.remove(t) {
            ordered.push((t.to_string(), v));
        }
    }
    ordered.extend(map); // any tier name not in the expected list, alphabetical
    ordered
}

fn scored<'a>(rows: &[&'a Row]) -> Vec<&'a Row> {
    rows.iter().copied().filter(|r| r.status == Status::Scored).collect()
}

fn avg(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// P&L of a scored call if only `share` of the unsold tokens' marked value turns out to be realisable.
fn pnl_with_haircut(sim: &SimResult, share: f64) -> f64 {
    sim.total_pnl_sol - (1.0 - share) * sim.moonbag_value
}

/// P&L if only the reserved moonbag fraction (not the legs that simply never fired) is worth zero.
fn pnl_reserve_zero(sim: &SimResult, legs: &[ExitLeg]) -> f64 {
    let fired = sim.legs_filled.len().min(legs.len());
    let fired_fraction: f64 = legs.iter().take(fired).map(|l| l.sell_fraction).sum();
    let reserve = 1.0 - legs.iter().map(|l| l.sell_fraction).sum::<f64>();
    let remaining = 1.0 - fired_fraction;
    let reserve_share = if remaining > 0.0 { (reserve / remaining).min(1.0) } else { 0.0 };
    sim.total_pnl_sol - sim.moonbag_value * reserve_share
}

fn print_report(rows: &[Row], legs: &[ExitLeg]) {
    let all: Vec<&Row> = rows.iter().collect();
    let sc = scored(&all);
    let no_trades = rows.iter().filter(|r| r.status == Status::NoTrades).count();
    let failed = rows.iter().filter(|r| r.status == Status::LookupFailed).count();
    let wins = sc.iter().filter(|r| r.sims[0].outcome == Outcome::Win).count();
    let total_pnl: f64 = sc.iter().map(|r| r.sims[0].total_pnl_sol).sum();

    println!("\n\n=== OVERALL ===");
    println!("Total calls:             {}", rows.len());
    println!("Scored:                  {}", sc.len());
    println!("No Pump.fun/PumpSwap trades: {no_trades}");
    println!("Lookup failed:           {failed}");
    println!("Wins / losses (scored):  {} / {}", wins, sc.len() - wins);
    println!("Total P&L (SOL):         {total_pnl:+.4}");
    println!("Average per scored call: {:+.1}%", pct(avg(sc.iter().map(|r| r.sims[0].total_pnl_sol))));

    println!("\n=== BY TIER (entry at the first trade after the call) ===");
    println!(
        "{:<13}{:>6}{:>8}{:>9}{:>7}{:>8}{:>9}{:>10}{:>11}{:>11}",
        "tier", "calls", "scored", "no_pump", "failed", "win%", "avg%", "median%", "ex-top5%", "total_SOL"
    );
    let groups = tier_groups(rows);
    for (tier, rs) in &groups {
        let s = scored(rs);
        let w = s.iter().filter(|r| r.sims[0].outcome == Outcome::Win).count();
        let pnls: Vec<f64> = s.iter().map(|r| r.sims[0].total_pnl_sol).collect();
        println!(
            "{:<13}{:>6}{:>8}{:>9}{:>7}{:>7.1}%{:>8.1}%{:>9.1}%{:>10.1}%{:>11.4}",
            tier,
            rs.len(),
            s.len(),
            rs.iter().filter(|r| r.status == Status::NoTrades).count(),
            rs.iter().filter(|r| r.status == Status::LookupFailed).count(),
            if s.is_empty() { 0.0 } else { w as f64 / s.len() as f64 * 100.0 },
            pct(avg(pnls.iter().copied())),
            pct(median(pnls.clone())),
            pct(avg_without_top(&pnls, 5)),
            pnls.iter().sum::<f64>(),
        );
    }

    println!("\n=== STRESS: average % per call if only part of the unsold tokens' marked value is realisable ===");
    println!("(100% = as simulated, 0% = every unsold token worth nothing)");
    print!("{:<13}{:>6}", "tier", "n");
    for h in HAIRCUTS {
        print!("{:>9}", format!("{}%", (h * 100.0) as i32));
    }
    println!("{:>14}{:>12}", "reserve=0", "break-even");
    for (tier, rs) in &groups {
        let s = scored(rs);
        if s.is_empty() { continue; }
        print!("{:<13}{:>6}", tier, s.len());
        for h in HAIRCUTS {
            print!("{:>8.1}%", pct(avg(s.iter().map(|r| pnl_with_haircut(&r.sims[0], h)))));
        }
        let reserve_zero = pct(avg(s.iter().map(|r| pnl_reserve_zero(&r.sims[0], legs))));
        let sum_pnl: f64 = s.iter().map(|r| r.sims[0].total_pnl_sol).sum();
        let sum_mv: f64 = s.iter().map(|r| r.sims[0].moonbag_value).sum();
        let breakeven = if sum_mv > 0.0 { format!("{:.0}%", (1.0 - sum_pnl / sum_mv) * 100.0) } else { "n/a".to_string() };
        println!("{:>13.1}%{:>12}", reserve_zero, breakeven);
    }

    println!("\n=== ENTRY DELAY: average % per call, same calls in every column ===");
    println!("(only calls with a trade available at all three entry times)");
    println!("{:<13}{:>6}{:>10}{:>10}{:>10}{:>12}", "tier", "n", "0s", "15s", "30s", "dropped");
    for (tier, rs) in &groups {
        let s = scored(rs);
        let common: Vec<&&Row> = s.iter().filter(|r| r.sims.iter().all(|x| x.outcome != Outcome::NoData)).collect();
        if common.is_empty() { continue; }
        println!(
            "{:<13}{:>6}{:>9.1}%{:>9.1}%{:>9.1}%{:>12}",
            tier,
            common.len(),
            pct(avg(common.iter().map(|r| r.sims[0].total_pnl_sol))),
            pct(avg(common.iter().map(|r| r.sims[1].total_pnl_sol))),
            pct(avg(common.iter().map(|r| r.sims[2].total_pnl_sol))),
            s.len() - common.len(),
        );
    }

    println!("\n=== PRICE-PATH COVERAGE (scored calls) ===");
    let mut spans: Vec<f64> = sc.iter().map(|r| r.span_hours).collect();
    spans.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if !spans.is_empty() {
        let q = |f: f64| spans[((spans.len() - 1) as f64 * f) as usize];
        let reach = |h: f64| spans.iter().filter(|&&s| s >= h).count() as f64 / spans.len() as f64 * 100.0;
        println!("Hours covered, 10th / 50th / 90th percentile: {:.1} / {:.1} / {:.1}", q(0.1), q(0.5), q(0.9));
        println!("Reach hour 24: {:.0}%   reach hour 40: {:.0}%   reach hour 47: {:.0}%", reach(24.0), reach(40.0), reach(47.0));
        println!("Requests per call (median): {}", {
            let mut r: Vec<usize> = rows.iter().map(|r| r.requests).collect();
            r.sort();
            r[r.len() / 2]
        });
    }

    println!("\n=== PER-CALL DETAIL ===");
    for r in rows {
        match r.status {
            Status::NoTrades => println!("{} [{}] -- NO PUMP.FUN/PUMPSWAP TRADES FOUND", r.call.mint_address, r.call.tier),
            Status::LookupFailed => println!("{} [{}] -- LOOKUP FAILED ({} of {} requests failed)", r.call.mint_address, r.call.tier, r.failed, r.requests),
            Status::Scored => {
                let s = &r.sims[0];
                println!(
                    "{} [{}] ts={}  entry={:.9} SOL  pnl={:+.4} SOL ({:+.1}%)  moonbag={:.4} SOL  span_h={:.1} pts={}",
                    r.call.mint_address, r.call.tier, s.entry_timestamp, s.entry_price, s.total_pnl_sol, s.total_pnl_pct, s.moonbag_value, r.span_hours, r.points
                );
                for (i, (multiplier, sol)) in s.legs_filled.iter().enumerate() {
                    println!("    leg {}: fired at {:.2}x, returned {:.4} SOL", i + 1, multiplier, sol);
                }
                for (k, delay) in DELAYS_SECS.iter().enumerate().skip(1) {
                    match r.sims[k].outcome {
                        Outcome::NoData => println!("    delay {delay}s: no trade available"),
                        _ => println!("    delay {delay}s: pnl={:+.4} SOL ({:+.1}%)", r.sims[k].total_pnl_sol, r.sims[k].total_pnl_pct),
                    }
                }
            }
        }
    }
}

fn results_path(input: &str) -> String {
    let p = std::path::Path::new(input);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("calls");
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join(format!("{stem}_results.csv")).to_string_lossy().into_owned(),
        _ => format!("{stem}_results.csv"),
    }
}

fn write_results_csv(path: &str, rows: &[Row]) -> Result<()> {
    let mut w = csv::Writer::from_path(path).with_context(|| format!("could not create {path}"))?;
    w.write_record([
        "mint", "tier", "called_at_unix", "status", "entry_price_sol", "pnl_sol", "pnl_pct", "legs_fired", "legs_returned_sol",
        "moonbag_value_sol", "pnl_sol_delay15", "pnl_sol_delay30", "span_hours", "price_points", "requests", "failed_requests",
    ])?;
    for r in rows {
        let status = match r.status {
            Status::Scored => "scored",
            Status::NoTrades => "no_pump_trades_found",
            Status::LookupFailed => "lookup_failed",
        };
        let mut rec: Vec<String> = vec![r.call.mint_address.clone(), r.call.tier.clone(), r.call.call_timestamp.to_string(), status.to_string()];
        if r.status == Status::Scored {
            let s = &r.sims[0];
            let delayed = |k: usize| match r.sims[k].outcome {
                Outcome::NoData => String::new(),
                _ => format!("{:.6}", r.sims[k].total_pnl_sol),
            };
            rec.extend([
                format!("{:.12}", s.entry_price),
                format!("{:.6}", s.total_pnl_sol),
                format!("{:.2}", s.total_pnl_pct),
                s.legs_filled.len().to_string(),
                format!("{:.6}", s.legs_filled.iter().map(|l| l.1).sum::<f64>()),
                format!("{:.6}", s.moonbag_value),
                delayed(1),
                delayed(2),
            ]);
        } else {
            rec.extend(std::iter::repeat(String::new()).take(8));
        }
        rec.extend([format!("{:.2}", r.span_hours), r.points.to_string(), r.requests.to_string(), r.failed.to_string()]);
        w.write_record(&rec)?;
    }
    w.flush()?;
    Ok(())
}
