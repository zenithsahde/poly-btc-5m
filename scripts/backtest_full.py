#!/usr/bin/env python3
"""完整版差异化守门 + 被动配平路径（path 3）."""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
import numpy as np
from backtest_guards import (State, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE,
                              MIN_EXPIRY_MIN_FOR_CHASE, MERGE_AVG_SUM_MAX,
                              FORCE_MERGE_EXPIRY_MIN, MERGE_INTERVAL_MS,
                              MERGE_TRIGGER_PAIR_QTY, load_data)

DATA_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/aerlan_live"
df = load_data(DATA_DIR)
print(f"Loaded {len(df):,} snapshots\n")


def fv_rising(hist, current):
    if len(hist) < 3: return False
    return current > sum(hist) / len(hist)


def run_full(df, gap_min, leg2_max, passive_max, throttle_ms=1000):
    """3 路径: leg1 (chase + 对侧空) | leg2_chase (chase + 配对) | leg2_passive (无 chase + 配对)."""
    s = State()
    last_window = None; last_binance = 0.0; last_strike = 0.0
    counts = {"leg1": 0, "leg2_chase": 0, "leg2_passive": 0}

    for row in df.itertuples():
        ts = int(row.t_ms); win_end = int(row.window_end_ts)
        binance_mid = float(row.binance_mid)
        fv_up = float(row.fv_up); fv_down = float(row.fv_down)
        up_bid = float(row.poly_up_bid); up_ask = float(row.poly_up_ask)
        down_bid = float(row.poly_down_bid); down_ask = float(row.poly_down_ask)
        market_state = row.market_state
        expiry_min = float(row.expiry_min); strike = float(row.strike); source = row.source

        if last_window is not None and win_end != last_window:
            s.settle_window(last_binance, last_strike, ts)
        last_window = win_end; last_binance = binance_mid; last_strike = strike

        in_excited = (market_state == "E"); in_chase_w = expiry_min >= MIN_EXPIRY_MIN_FOR_CHASE

        if source == "B":
            up_mid = (up_bid+up_ask)/2 if (up_bid>0 and up_ask>0) else 0.0
            down_mid = (down_bid+down_ask)/2 if (down_bid>0 and down_ask>0) else 0.0

            up_chase = (in_excited and in_chase_w and up_mid>0
                       and (fv_up - up_mid) >= gap_min and fv_rising(s.fv_up_hist, fv_up))
            down_chase = (in_excited and in_chase_w and down_mid>0
                         and (fv_down - down_mid) >= gap_min and fv_rising(s.fv_down_hist, fv_down))

            # 路径触发判定
            def can_buy_up():
                price_ok = 0 < up_ask <= MAX_BUY_PRICE
                if not price_ok: return None
                throttle = ts - s.last_taker_up_ts >= throttle_ms
                if not throttle: return None
                # leg1: chase + 对侧空 + force-balance
                if up_chase and s.pos_down.qty == 0.0 and s.pos_up.qty <= s.pos_down.qty:
                    return "leg1"
                # leg2_chase: chase + 对侧有仓 + 健康
                if up_chase and s.pos_down.qty > 0.0 and (up_ask + s.pos_down.avg) < leg2_max:
                    return "leg2_chase"
                # leg2_passive: 无 chase + **偏仓 UP 落后** + 价健康
                if (not up_chase) and in_excited and in_chase_w \
                   and s.pos_down.qty > s.pos_up.qty \
                   and (up_ask + s.pos_down.avg) < passive_max:
                    return "leg2_passive"
                return None

            def can_buy_dn():
                price_ok = 0 < down_ask <= MAX_BUY_PRICE
                if not price_ok: return None
                throttle = ts - s.last_taker_down_ts >= throttle_ms
                if not throttle: return None
                if down_chase and s.pos_up.qty == 0.0 and s.pos_down.qty <= s.pos_up.qty:
                    return "leg1"
                if down_chase and s.pos_up.qty > 0.0 and (down_ask + s.pos_up.avg) < leg2_max:
                    return "leg2_chase"
                # leg2_passive: 无 chase + **偏仓 DOWN 落后** + 价健康
                if (not down_chase) and in_excited and in_chase_w \
                   and s.pos_up.qty > s.pos_down.qty \
                   and (down_ask + s.pos_up.avg) < passive_max:
                    return "leg2_passive"
                return None

            up_path = can_buy_up()
            dn_path = can_buy_dn()
            # 优先 leg2_passive（防漏配平），其次 chase
            if up_path:
                s.apply_fill("UP", up_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
                counts[up_path] += 1
            if dn_path:
                s.apply_fill("DOWN", down_ask, MAKER_BUY_CHASE_MIN_QTY, ts)
                counts[dn_path] += 1

            s.fv_up_hist.append(fv_up); s.fv_down_hist.append(fv_down)

        if source == "P":
            mergeable = s.mergeable()
            throttle_ok = ts - s.last_merge_ts >= MERGE_INTERVAL_MS
            avg_sum_now = s.avg_sum()
            force_merge = expiry_min < FORCE_MERGE_EXPIRY_MIN
            avg_ok = force_merge or avg_sum_now < MERGE_AVG_SUM_MAX
            if mergeable >= MERGE_TRIGGER_PAIR_QTY and throttle_ok and avg_ok:
                pq = max(np.floor(mergeable), MERGE_TRIGGER_PAIR_QTY)
                s.apply_merge(pq, ts, force=force_merge)

    if last_window is not None:
        s.settle_window(last_binance, last_strike, last_window*1000)
    pnl = s.cash_received - s.cash_paid - s.total_fee
    return pnl, s, counts


print(f"{'方案':<40} {'PnL':>10} {'merge':>7} {'fee':>6} {'L1':>4} {'L2c':>4} {'L2p':>4}")
print("-" * 90)

# 完整版扫描
for gap in [0.05, 0.07, 0.10]:
    for leg2 in [0.98, 1.00]:
        for passive in [0.96, 0.98]:
            if passive >= leg2: continue  # passive 必须更严
            pnl, st, ct = run_full(df, gap, leg2, passive)
            label = f"gap={gap:.2f} L2<{leg2:.2f} pass<{passive:.2f}"
            print(f"{label:<40} ${pnl:+9.2f}  {st.merge_pnl:+6.1f}  {st.total_fee:6.1f}  "
                  f"{ct['leg1']:4d}  {ct['leg2_chase']:4d}  {ct['leg2_passive']:4d}")
