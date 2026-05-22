#!/usr/bin/env python3
"""
Marketable Limit Order 回测器（v0.4.11 设计）.

核心思路（用户提出）：
- 不分 taker/maker，统一用 limit order @ FV - SAFETY_MARGIN
- best_ask 已 ≤ 目标价 → 立即 fill (相当于 taker)
- best_ask > 目标价 → 挂 maker 单，等价格跌到目标价 fill
- FV 移动 ≥ REPRICE_TICK 时 cancel + 重挂
- TIMEOUT 秒未 fill → cancel
- 配对腿目标价 = FV_对侧 - SAFETY_MARGIN

回测假设：
- maker fill 模拟：如果 best_ask 触及挂单价（best_ask ≤ 挂单价）→ fill
- maker 25% rebate（fee × 0.25 反给我们）
- 触发条件：force-balance + 偏仓时主动配平
"""
import sys
sys.path.insert(0, '/Users/zenith/btc-poly/market-maker-5m/scripts')
from collections import deque
import numpy as np
from backtest_guards import (State, MAKER_BUY_CHASE_MIN_QTY, MAX_BUY_PRICE,
                              MIN_EXPIRY_MIN_FOR_CHASE, MERGE_AVG_SUM_MAX,
                              FORCE_MERGE_EXPIRY_MIN, MERGE_INTERVAL_MS,
                              MERGE_TRIGGER_PAIR_QTY, FV_HISTORY_LEN,
                              TAKER_FEE_RATE, load_data)

DATA_DIR = sys.argv[1] if len(sys.argv) > 1 else "/tmp/aerlan_live"


def fv_rising(hist, current):
    if len(hist) < 3: return False
    return current > sum(hist) / len(hist)


def run_limit(df, gap_min, safety, timeout_ms=5000, reprice_tick=2):
    """两腿统一 limit order @ FV - safety."""
    s = State()
    last_window = None; last_binance = 0.0; last_strike = 0.0
    # 挂单状态：{"price": x, "placed_ts": t, "qty": q}, None 表示无挂单
    pending_up = None
    pending_down = None
    counts = {"taker": 0, "maker": 0, "cancelled": 0, "expired": 0, "reprice": 0}

    TICK = 0.01

    for row in df.itertuples():
        ts = int(row.t_ms); win_end = int(row.window_end_ts)
        binance_mid = float(row.binance_mid)
        fv_up = float(row.fv_up); fv_down = float(row.fv_down)
        up_bid = float(row.poly_up_bid); up_ask = float(row.poly_up_ask)
        down_bid = float(row.poly_down_bid); down_ask = float(row.poly_down_ask)
        market_state = row.market_state
        expiry_min = float(row.expiry_min); strike = float(row.strike); source = row.source

        if last_window is not None and win_end != last_window:
            # 窗口切换 cancel 所有挂单
            if pending_up: counts["cancelled"] += 1
            if pending_down: counts["cancelled"] += 1
            pending_up = None
            pending_down = None
            s.settle_window(last_binance, last_strike, ts)
        last_window = win_end; last_binance = binance_mid; last_strike = strike

        in_excited = (market_state == "E")
        in_chase_w = expiry_min >= MIN_EXPIRY_MIN_FOR_CHASE

        # 计算两腿"目标价"
        target_up = max(0.0, fv_up - safety)    # buy UP 目标价
        target_dn = max(0.0, fv_down - safety)  # buy DOWN 目标价

        def apply_fill_with_fee(side, price, qty, is_maker):
            """统一 fill 应用：taker 收 fee，maker 退 25% rebate."""
            p = max(0.0, min(1.0, price))
            fee = qty * TAKER_FEE_RATE * p * (1-p)
            if is_maker:
                s.total_rebate = getattr(s, 'total_rebate', 0.0) + fee * 0.25
                # rebate 减少总成本（cash_received += rebate）
                s.cash_received += fee * 0.25
                counts["maker"] += 1
            else:
                s.total_fee += fee
                counts["taker"] += 1
            s.cash_paid += price * qty
            if side == "UP":
                s.pos_up.buy(price, qty)
                s.last_taker_up_ts = ts
            else:
                s.pos_down.buy(price, qty)
                s.last_taker_down_ts = ts
            s.max_qty_up = max(s.max_qty_up, s.pos_up.qty)
            s.max_qty_down = max(s.max_qty_down, s.pos_down.qty)

        # ==== 检查挂单 fill 状态（每帧）====
        if pending_up is not None:
            # Maker fill 条件：best_ask_up ≤ 挂单价
            if up_ask > 0 and up_ask <= pending_up["price"]:
                apply_fill_with_fee("UP", pending_up["price"], pending_up["qty"], is_maker=True)
                pending_up = None
            elif ts - pending_up["placed_ts"] > timeout_ms:
                pending_up = None
                counts["expired"] += 1
            elif abs(target_up - pending_up["price"]) >= reprice_tick * TICK:
                pending_up = None
                counts["reprice"] += 1

        if pending_down is not None:
            if down_ask > 0 and down_ask <= pending_down["price"]:
                apply_fill_with_fee("DOWN", pending_down["price"], pending_down["qty"], is_maker=True)
                pending_down = None
            elif ts - pending_down["placed_ts"] > timeout_ms:
                pending_down = None
                counts["expired"] += 1
            elif abs(target_dn - pending_down["price"]) >= reprice_tick * TICK:
                pending_down = None
                counts["reprice"] += 1

        # ==== chase 触发判定 + 决定 limit order ====
        if source == "B":
            up_mid = (up_bid+up_ask)/2 if (up_bid>0 and up_ask>0) else 0.0
            down_mid = (down_bid+down_ask)/2 if (down_bid>0 and down_ask>0) else 0.0
            up_chase = (in_excited and in_chase_w and up_mid>0
                       and (fv_up - up_mid) >= gap_min and fv_rising(s.fv_up_hist, fv_up))
            down_chase = (in_excited and in_chase_w and down_mid>0
                         and (fv_down - down_mid) >= gap_min and fv_rising(s.fv_down_hist, fv_down))

            # force-balance: 只允许买持仓 ≤ 对侧的方向
            balance_ok_up = s.pos_up.qty <= s.pos_down.qty
            balance_ok_down = s.pos_down.qty <= s.pos_up.qty

            # 触发条件：chase 信号 OR (偏仓 + 价合适)
            imbalanced_up = s.pos_down.qty > s.pos_up.qty   # UP 落后
            imbalanced_dn = s.pos_up.qty > s.pos_down.qty   # DOWN 落后

            should_buy_up = (up_chase and balance_ok_up) or (imbalanced_up and target_up > 0)
            should_buy_dn = (down_chase and balance_ok_down) or (imbalanced_dn and target_dn > 0)

            # 决定挂单
            if should_buy_up and pending_up is None:
                if target_up > MAX_BUY_PRICE or target_up <= 0:
                    pass  # 守门拒
                elif up_ask > 0 and up_ask <= target_up:
                    # 立即 fill (taker)
                    apply_fill_with_fee("UP", up_ask, MAKER_BUY_CHASE_MIN_QTY, is_maker=False)
                else:
                    # 挂 maker 单 @ target_up
                    pending_up = {"price": target_up, "placed_ts": ts, "qty": MAKER_BUY_CHASE_MIN_QTY}

            if should_buy_dn and pending_down is None:
                if target_dn > MAX_BUY_PRICE or target_dn <= 0:
                    pass
                elif down_ask > 0 and down_ask <= target_dn:
                    apply_fill_with_fee("DOWN", down_ask, MAKER_BUY_CHASE_MIN_QTY, is_maker=False)
                else:
                    pending_down = {"price": target_dn, "placed_ts": ts, "qty": MAKER_BUY_CHASE_MIN_QTY}

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


def main():
    df = load_data(DATA_DIR)
    print(f"Loaded {len(df):,} snapshots from {DATA_DIR}\n")

    print(f"{'方案':<40} {'PnL':>10} {'merge':>7} {'fee':>6} {'taker':>6} {'maker':>6} {'expired':>7} {'reprice':>7}")
    print("-" * 100)

    for gap in [0.05, 0.07, 0.10]:
        for margin in [0.01, 0.02, 0.03, 0.04, 0.05]:
            pnl, st, ct = run_limit(df, gap, margin, timeout_ms=5000, reprice_tick=2)
            label = f"gap={gap:.2f} margin={margin:.2f}"
            print(f"{label:<40} ${pnl:+9.2f}  {st.merge_pnl:+6.1f}  {st.total_fee:6.1f}  "
                  f"{ct['taker']:6d}  {ct['maker']:6d}  {ct['expired']:7d}  {ct['reprice']:7d}")
        print()


if __name__ == "__main__":
    main()
