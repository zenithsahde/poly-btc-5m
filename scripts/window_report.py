#!/usr/bin/env python3
"""
Dry-run window report for trades/ + orders/.

Inputs:
  trades/trades_<window_end>.csv
  orders/orders_<window_end>.csv

The report is intentionally dependency-free so it can run on the box that ran
the engine for 48h. Net PnL is an estimate from logged fills: matched UP/DOWN
pairs are valued at $1.00, unpaired residual inventory is shown separately and
not credited because these CSVs do not contain final oracle settlement.
"""

import argparse
import csv
import glob
import math
import os
from collections import defaultdict


TAKER_FEE_RATE = 0.072


def fnum(value, default=0.0):
    try:
        if value is None or value == "":
            return default
        return float(value)
    except ValueError:
        return default


def inum(value, default=0):
    try:
        if value is None or value == "":
            return default
        return int(float(value))
    except ValueError:
        return default


def window_from_path(path, prefix):
    name = os.path.basename(path)
    stem, _ = os.path.splitext(name)
    if not stem.startswith(prefix):
        return None
    try:
        return int(stem[len(prefix):])
    except ValueError:
        return None


def read_csvs(directory, prefix):
    rows_by_window = defaultdict(list)
    for path in sorted(glob.glob(os.path.join(directory, f"{prefix}*.csv"))):
        win = window_from_path(path, prefix)
        if win is None:
            continue
        with open(path, newline="") as f:
            for row in csv.DictReader(f):
                row["_path"] = path
                row["_window"] = win
                rows_by_window[win].append(row)
    return rows_by_window


def trade_side_stats(trades, side):
    side_rows = [r for r in trades if r.get("side") == side and r.get("direction") == "buy"]
    qty = sum(fnum(r.get("qty")) for r in side_rows)
    notional = sum(fnum(r.get("qty")) * fnum(r.get("price")) for r in side_rows)
    avg = notional / qty if qty > 0 else 0.0
    return qty, notional, avg


def fee_for_trade(row):
    qty = fnum(row.get("qty"))
    price = max(0.0, min(1.0, fnum(row.get("price"))))
    fee = qty * TAKER_FEE_RATE * price * (1.0 - price)
    return fee


def summarize_window(win, trades, orders):
    up_qty, up_notional, up_avg = trade_side_stats(trades, "UP")
    down_qty, down_notional, down_avg = trade_side_stats(trades, "DOWN")
    cash_paid = up_notional + down_notional

    taker_fee = 0.0
    maker_rebate = 0.0
    for r in trades:
        fee = fee_for_trade(r)
        if r.get("maker_taker") == "maker":
            maker_rebate += fee * 0.25
        else:
            taker_fee += fee

    pairs = min(up_qty, down_qty)
    avg_sum = (up_avg + down_avg) if up_qty > 0 and down_qty > 0 else 0.0
    merge_pnl_est = pairs * (1.0 - avg_sum) if pairs > 0 else 0.0
    matched_value = pairs * 1.0
    net_pnl_est = matched_value - cash_paid - taker_fee + maker_rebate
    cash_pnl = -cash_paid
    residual_up = max(0.0, up_qty - pairs)
    residual_down = max(0.0, down_qty - pairs)

    order_count = len(orders)
    filled_orders = [o for o in orders if fnum(o.get("filled_qty")) > 0.0]
    worst_breach = [o for o in orders if o.get("reject_reason") == "worst_breach"]
    partial = [
        o for o in orders
        if o.get("reject_reason") == "ioc_remainder"
        or (fnum(o.get("filled_qty")) > 0.0 and fnum(o.get("filled_qty")) < fnum(o.get("qty")))
    ]
    fill_rate = len(filled_orders) / order_count if order_count else 0.0
    worst_breach_rate = len(worst_breach) / order_count if order_count else 0.0
    partial_rate = len(partial) / order_count if order_count else 0.0

    fill_levels = [inum(o.get("fill_levels")) for o in filled_orders]
    avg_fill_levels = sum(fill_levels) / len(fill_levels) if fill_levels else 0.0
    max_fill_levels = max(fill_levels) if fill_levels else 0

    reason_stats = {}
    for reason in ("chase", "rebal"):
        subset = [o for o in orders if o.get("reason") == reason]
        filled = [o for o in subset if fnum(o.get("filled_qty")) > 0.0]
        reason_stats[reason] = {
            "orders": len(subset),
            "fill_rate": len(filled) / len(subset) if subset else 0.0,
            "worst_breach_rate": (
                sum(1 for o in subset if o.get("reject_reason") == "worst_breach") / len(subset)
                if subset else 0.0
            ),
            "filled_qty": sum(fnum(o.get("filled_qty")) for o in filled),
            "filled_notional": sum(fnum(o.get("filled_qty")) * fnum(o.get("vwap")) for o in filled),
        }

    return {
        "window_end": win,
        "trades": len(trades),
        "orders": order_count,
        "fill_rate": fill_rate,
        "worst_breach_rate": worst_breach_rate,
        "partial_rate": partial_rate,
        "cash_pnl": cash_pnl,
        "merge_pnl_est": merge_pnl_est,
        "net_pnl_est": net_pnl_est,
        "up_qty": up_qty,
        "down_qty": down_qty,
        "residual_up": residual_up,
        "residual_down": residual_down,
        "avg_fill_levels": avg_fill_levels,
        "max_fill_levels": max_fill_levels,
        "chase_orders": reason_stats["chase"]["orders"],
        "chase_fill_rate": reason_stats["chase"]["fill_rate"],
        "chase_worst_breach_rate": reason_stats["chase"]["worst_breach_rate"],
        "rebal_orders": reason_stats["rebal"]["orders"],
        "rebal_fill_rate": reason_stats["rebal"]["fill_rate"],
        "rebal_worst_breach_rate": reason_stats["rebal"]["worst_breach_rate"],
        "taker_fee": taker_fee,
        "maker_rebate": maker_rebate,
    }


def pct(x):
    return f"{x * 100:5.1f}%"


def money(x):
    return f"{x:+8.3f}"


def print_report(rows):
    if not rows:
        print("No trades/orders CSV files found.")
        return

    cumulative = 0.0
    print(
        "window_end   trades orders fill  worst partial  cash_pnl merge_est  net_est  "
        "resid(U/D) levels(avg/max)"
    )
    print("-" * 118)
    for r in rows:
        cumulative += r["net_pnl_est"]
        print(
            f"{r['window_end']} {r['trades']:7d} {r['orders']:6d} "
            f"{pct(r['fill_rate'])} {pct(r['worst_breach_rate'])} {pct(r['partial_rate'])} "
            f"{money(r['cash_pnl'])} {money(r['merge_pnl_est'])} {money(r['net_pnl_est'])} "
            f"{r['residual_up']:.0f}/{r['residual_down']:.0f} "
            f"{r['avg_fill_levels']:.2f}/{r['max_fill_levels']}"
        )
    print("-" * 118)
    total_orders = sum(r["orders"] for r in rows)
    total_trades = sum(r["trades"] for r in rows)
    weighted_fill = (
        sum(r["fill_rate"] * r["orders"] for r in rows) / total_orders
        if total_orders else 0.0
    )
    weighted_worst = (
        sum(r["worst_breach_rate"] * r["orders"] for r in rows) / total_orders
        if total_orders else 0.0
    )
    print(f"windows={len(rows)} trades={total_trades} orders={total_orders}")
    print(f"fill_rate={pct(weighted_fill)} worst_breach={pct(weighted_worst)} cumulative_net_est={money(cumulative)}")
    print("\nBy reason:")
    for reason in ("chase", "rebal"):
        order_key = f"{reason}_orders"
        fill_key = f"{reason}_fill_rate"
        breach_key = f"{reason}_worst_breach_rate"
        orders = sum(r[order_key] for r in rows)
        fill = sum(r[fill_key] * r[order_key] for r in rows) / orders if orders else 0.0
        breach = sum(r[breach_key] * r[order_key] for r in rows) / orders if orders else 0.0
        print(f"  {reason:5s}: orders={orders:5d} fill_rate={pct(fill)} worst_breach={pct(breach)}")
    print("\nNote: net_est values credit matched UP/DOWN pairs at $1.00 and ignore unpaired residual settlement.")


def write_csv(path, rows):
    if not rows:
        return
    fields = list(rows[0].keys()) + ["cum_net_pnl_est"]
    cumulative = 0.0
    with open(path, "w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=fields)
        writer.writeheader()
        for row in rows:
            cumulative += row["net_pnl_est"]
            out = dict(row)
            out["cum_net_pnl_est"] = cumulative
            writer.writerow(out)


def main():
    parser = argparse.ArgumentParser(description="Summarize dry-run trades and IOC order lifecycle CSVs.")
    parser.add_argument("--trades-dir", default="trades")
    parser.add_argument("--orders-dir", default="orders")
    parser.add_argument("--csv", help="Optional output CSV path for per-window summary.")
    args = parser.parse_args()

    trades = read_csvs(args.trades_dir, "trades_")
    orders = read_csvs(args.orders_dir, "orders_")
    windows = sorted(set(trades.keys()) | set(orders.keys()))
    rows = [summarize_window(w, trades.get(w, []), orders.get(w, [])) for w in windows]
    print_report(rows)
    if args.csv:
        write_csv(args.csv, rows)
        print(f"\nwrote {args.csv}")


if __name__ == "__main__":
    main()
