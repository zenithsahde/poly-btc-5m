#!/usr/bin/env python3
"""
激变态快照延迟分析脚本（仅用标准库）

从 excited_<window_end_ts>.csv 中分析「谁先变」与有效延迟：
- 正延迟：FV 先动，Poly 后跟上；仅当方向一致且 delay <= tau_max 时计为有效。
- 负延迟：Poly 先动，我们 FV 后动；仅当同向且间隔 <= tau_max 时计为有效。

用法:
  python scripts/analyze_excited_snapshots.py [--tau-max-ms 3000] [--epsilon 0.03] [--min-move 0.02] excited_snapshots/excited_1771848900.csv
  python scripts/analyze_excited_snapshots.py excited_snapshots/*.csv
"""
from __future__ import annotations

import argparse
import csv
import sys
from pathlib import Path

# 默认参数（与文档/引擎一致）
DEFAULT_EPSILON_GAP = 0.03   # 视为「有缺口」的 FV 与 Poly 差
DEFAULT_EPSILON_CATCH = 0.03 # 视为「跟上」的阈值
DEFAULT_TAU_MAX_MS = 3000    # 最大有效延迟（ms），超过则模型可能已漂移
DEFAULT_MIN_MOVE = 0.02      # 显著变动/跟动幅度（可选）


def _float(s: str) -> float:
    try:
        return float(s)
    except (TypeError, ValueError):
        return 0.0


def load_and_prepare(csv_path: Path) -> list[dict]:
    """读取 CSV，计算 poly_up_mid，剔除 Poly 全 0 的行，按 t_ms 排序。返回 list of dict。"""
    rows: list[dict] = []
    with open(csv_path, newline="", encoding="utf-8") as f:
        r = csv.DictReader(f)
        for rec in r:
            t_ms = int(_float(rec.get("t_ms", 0)))
            fv_up = _float(rec.get("fv_up", 0))
            fv_down = _float(rec.get("fv_down", 0))
            up_bid = _float(rec.get("poly_up_bid", 0))
            up_ask = _float(rec.get("poly_up_ask", 0))
            down_bid = _float(rec.get("poly_down_bid", 0))
            down_ask = _float(rec.get("poly_down_ask", 0))
            if up_bid > 0 and up_ask > 0:
                poly_up_mid = (up_bid + up_ask) / 2.0
            elif down_bid > 0 and down_ask > 0:
                poly_up_mid = 1.0 - (down_bid + down_ask) / 2.0
            else:
                continue
            if poly_up_mid <= 0:
                continue
            rows.append({
                "t_ms": t_ms,
                "fv_up": fv_up,
                "fv_down": fv_down,
                "poly_up_mid": poly_up_mid,
            })
    rows.sort(key=lambda x: x["t_ms"])
    return rows


def analyze_positive_delays(
    rows: list[dict],
    epsilon_gap: float,
    epsilon_catch: float,
    tau_max_ms: float,
    min_follow: float | None,
) -> tuple[list[dict], list[dict]]:
    """
    正延迟：FV 先与 Poly 形成缺口，Poly 后跟上。
    返回 (all_events, valid_events)。
    """
    all_events: list[dict] = []
    valid_events: list[dict] = []

    in_lead = False
    lead_start_ms: int | None = None
    fv_lead: float | None = None
    p_lead: float | None = None

    for row in rows:
        t_ms = row["t_ms"]
        fv_up = row["fv_up"]
        p_mid = row["poly_up_mid"]

        if not in_lead:
            gap = abs(fv_up - p_mid)
            if gap > epsilon_gap:
                in_lead = True
                lead_start_ms = t_ms
                fv_lead = fv_up
                p_lead = p_mid
            continue

        if t_ms - lead_start_ms > tau_max_ms:
            in_lead = False
            lead_start_ms = None
            fv_lead = None
            p_lead = None
            continue

        if fv_lead is None or p_lead is None:
            continue

        if abs(p_mid - fv_lead) < epsilon_catch:
            delay_ms = t_ms - lead_start_ms
            direction_ok = (fv_lead - p_lead) * (p_mid - p_lead) >= 0
            within_tau = delay_ms <= tau_max_ms
            follow_ok = True
            if min_follow is not None:
                follow_ok = abs(p_mid - p_lead) >= min_follow

            evt = {
                "delay_ms": delay_ms,
                "t_lead": lead_start_ms,
                "t_catch": t_ms,
                "fv_lead": fv_lead,
                "p_lead": p_lead,
                "p_catch": p_mid,
                "direction_ok": direction_ok,
                "within_tau": within_tau,
                "follow_ok": follow_ok,
            }
            all_events.append(evt)
            if direction_ok and within_tau and follow_ok:
                valid_events.append(evt)

            in_lead = False
            lead_start_ms = None
            fv_lead = None
            p_lead = None

    return all_events, valid_events


def analyze_negative_delays(
    rows: list[dict],
    min_move: float,
    tau_max_ms: float,
) -> tuple[list[dict], list[dict]]:
    """
    负延迟：Poly 先显著变动，随后 FV 同向显著变动。
    返回 (all_events, valid_events)。
    """
    all_events: list[dict] = []
    valid_events: list[dict] = []

    prev_p: float | None = None
    prev_fv: float | None = None
    poly_move_ts: list[tuple[int, float, float, int]] = []  # (t_ms, p_mid, fv_up, +1/-1)

    for row in rows:
        t_ms = row["t_ms"]
        fv_up = row["fv_up"]
        p_mid = row["poly_up_mid"]

        if prev_p is not None:
            dp = p_mid - prev_p
            if abs(dp) >= min_move:
                direction = 1 if dp > 0 else -1
                poly_move_ts.append((t_ms, p_mid, fv_up, direction))

        prev_p = p_mid
        prev_fv = fv_up

    for t_poly, p_at_poly, fv_at_poly, direction in poly_move_ts:
        found = False
        for r in rows:
            if r["t_ms"] <= t_poly:
                continue
            t_fv = r["t_ms"]
            delay_ms = t_fv - t_poly
            if delay_ms > tau_max_ms:
                break
            dfv = r["fv_up"] - fv_at_poly
            if (direction > 0 and dfv >= min_move) or (direction < 0 and dfv <= -min_move):
                evt = {
                    "delay_ms": delay_ms,
                    "t_poly": t_poly,
                    "t_fv": t_fv,
                    "direction": "up" if direction > 0 else "down",
                    "within_tau": delay_ms <= tau_max_ms,
                }
                all_events.append(evt)
                if evt["within_tau"]:
                    valid_events.append(evt)
                found = True
                break
        # if not found, could optionally record as "no FV follow" for stats

    return all_events, valid_events


def stats_delays(events: list[dict], delay_key: str = "delay_ms") -> dict:
    if not events:
        return {"count": 0, "min_ms": None, "max_ms": None, "avg_ms": None}
    delays = [e[delay_key] for e in events]
    return {
        "count": len(delays),
        "min_ms": min(delays),
        "max_ms": max(delays),
        "avg_ms": sum(delays) / len(delays),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="激变态快照延迟分析（有效延迟 = 方向一致 + ≤ tau_max）")
    parser.add_argument(
        "csv_paths",
        nargs="+",
        type=Path,
        help="excited_<window_end_ts>.csv 路径，可多个",
    )
    parser.add_argument("--tau-max-ms", type=float, default=DEFAULT_TAU_MAX_MS, help="最大有效延迟 ms")
    parser.add_argument("--epsilon", type=float, default=DEFAULT_EPSILON_GAP, help="缺口/跟上阈值")
    parser.add_argument("--min-move", type=float, default=DEFAULT_MIN_MOVE, help="显著变动/跟动幅度")
    parser.add_argument("--no-min-follow", action="store_true", help="不要求跟动幅度，只方向+tau")
    args = parser.parse_args()

    epsilon_gap = args.epsilon
    epsilon_catch = args.epsilon
    tau_max_ms = args.tau_max_ms
    min_move = args.min_move
    min_follow = None if args.no_min_follow else min_move

    all_pos: list[dict] = []
    all_pos_valid: list[dict] = []
    all_neg: list[dict] = []
    all_neg_valid: list[dict] = []

    for path in args.csv_paths:
        if not path.exists():
            print(f"跳过不存在: {path}", file=sys.stderr)
            continue
        rows = load_and_prepare(path)
        if not rows:
            print(f"无有效行（Poly 全 0）: {path}", file=sys.stderr)
            continue
        window = path.stem.replace("excited_", "")
        pos_all, pos_valid = analyze_positive_delays(
            rows, epsilon_gap, epsilon_catch, tau_max_ms, min_follow
        )
        neg_all, neg_valid = analyze_negative_delays(rows, min_move, tau_max_ms)
        for e in pos_all:
            e["window"] = window
        for e in pos_valid:
            e["window"] = window
        for e in neg_all:
            e["window"] = window
        for e in neg_valid:
            e["window"] = window
        all_pos.extend(pos_all)
        all_pos_valid.extend(pos_valid)
        all_neg.extend(neg_all)
        all_neg_valid.extend(neg_valid)

    def fmt(s: dict) -> str:
        if s["count"] == 0:
            return "count=0 —"
        return "count=%d  min=%.0fms  max=%.0fms  avg=%.0fms" % (
            s["count"], s["min_ms"], s["max_ms"], s["avg_ms"]
        )

    print("======== 正延迟（FV 先动，Poly 跟上）=========")
    print("  全部（含方向错误/超 tau/跟动不足）:", fmt(stats_delays(all_pos)))
    print("  有效（方向一致 + ≤tau_max + 跟动≥min_move）:", fmt(stats_delays(all_pos_valid)))
    print("  参数: epsilon=%s  tau_max_ms=%s  min_follow=%s" % (epsilon_catch, tau_max_ms, min_follow))

    # 测算：多少 ms 内可保证 100%% 正延迟有效（方向一致）
    invalid_delays = [e["delay_ms"] for e in all_pos if not e["direction_ok"]]
    if not all_pos:
        print("  100%% 有效范围: 无正延迟样本")
    elif not invalid_delays:
        max_d = max(e["delay_ms"] for e in all_pos)
        print("  100%% 有效范围: 全部样本方向一致，可设 tau_max <= %.0f ms 仍 100%% 有效" % max_d)
    else:
        min_invalid = min(invalid_delays)
        n_invalid = len(invalid_delays)
        print("  100%% 有效范围: 若要求 100%% 方向有效，应设 tau_max < %d ms（最小「方向错误」延迟 = %d ms，共 %d 次超该值）" % (min_invalid, min_invalid, n_invalid))
        # 若当前 tau_max 已 >= min_invalid，给出收紧建议
        if tau_max_ms >= min_invalid:
            print("  建议: 将 --tau-max-ms 设为 %d 可得到 100%% 有效正延迟" % (min_invalid - 1 if min_invalid > 0 else 0))

    print("\n======== 负延迟（Poly 先动，FV 后动）=========")
    print("  全部:", fmt(stats_delays(all_neg)))
    print("  有效（间隔 ≤ tau_max）:", fmt(stats_delays(all_neg_valid)))
    print("  参数: min_move=%s  tau_max_ms=%s" % (min_move, tau_max_ms))

    return 0


if __name__ == "__main__":
    sys.exit(main())
