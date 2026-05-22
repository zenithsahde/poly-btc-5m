#!/usr/bin/env python3
"""
verify_chainlink_fv.py — Standalone verifier (no engine touch).

Per-source self-contained FV: each price source (Chainlink vs Binance) computes
its own FV from its own S, K, and realized σ — no contamination from Polymarket.
Compares each to Polymarket's live order book.

  FV_chainlink = N(d2)  with  S = chainlink price now,
                              K = chainlink price at window start,
                              σ = realized vol from chainlink 60s history
  FV_binance   = N(d2)  with  S = binance price now,
                              K = binance 5m candle open,
                              σ = realized vol from binance 60s history

Logs 1ms snapshots to CSV. Auto-rolls tokens / strikes at each 5m window.
Auto-stops after N full windows.
"""

import asyncio
import csv
import hashlib
import hmac
import json
import os
import sys
import time
from collections import deque
from math import erf, log, sqrt
from typing import Optional

import aiohttp
from aiohttp import web
import websockets

# ---------- Config ----------

CHAINLINK_WS_BASE = "wss://ws.dataengine.chain.link"
CHAINLINK_REST_BASE = "https://api.dataengine.chain.link"
BINANCE_WS_URL = "wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker"
POLY_WS_URL = "wss://ws-subscriptions-clob.polymarket.com/ws/market"

API_KEY = os.environ.get("CHAINLINK_DS_API_KEY", "").strip()
API_SECRET = os.environ.get("CHAINLINK_DS_API_SECRET", "").strip()
BTC_FEED_ID = os.environ.get(
    "CHAINLINK_DS_FEED_ID_BTCUSD",
    # Account-specific BTC/USD on external-polymarket-6298193 (~$77k verified).
    "0x00039d9e45394f473ab1f050a1b963e6b05351e52d71e507509ada0c95ed75b8",
).strip()
CSV_PATH = os.environ.get("CSV_PATH", "verify_chainlink_fv.csv")
N_WINDOWS = int(os.environ.get("N_WINDOWS", "10"))
SNAPSHOT_INTERVAL_S = float(os.environ.get("SNAPSHOT_INTERVAL_MS", "1")) / 1000.0
FLUSH_EVERY_N_ROWS = 2000
SIGMA_LOOKBACK_MS = 900_000  # 15 min — wider vol regime, fewer per-window σ artifacts
SIGMA_FALLBACK = 0.60
SIGMA_FLOOR = 0.10           # math safety; main σ signal comes from Deribit + realized
SIGMA_CEIL = 2.0
VOL_RISK_PREMIUM = 1.3
DERIBIT_IV_WEIGHT = 0.7      # iter 4 was best: σ ≈ 0.22 from Deribit IV × 0.7
DERIBIT_REFRESH_S = 60       # how often to re-fetch Deribit IV

PRICE_SCALE = 10 ** 18
SECONDS_PER_YEAR = 365.25 * 24 * 3600


# ---------- Black-Scholes binary option ----------

def norm_cdf(x: float) -> float:
    return 0.5 * (1.0 + erf(x / sqrt(2.0)))


def fv_binary_up(s: float, k: float, sigma_annual: float, t_seconds: float) -> Optional[float]:
    if t_seconds is None or t_seconds <= 0 or sigma_annual is None or sigma_annual <= 0:
        return None
    if s is None or k is None or s <= 0 or k <= 0:
        return None
    t_years = t_seconds / SECONDS_PER_YEAR
    d2 = (log(s / k) - 0.5 * sigma_annual ** 2 * t_years) / (sigma_annual * sqrt(t_years))
    return max(0.0, min(1.0, norm_cdf(d2)))


# ---------- Chainlink Data Streams: HMAC signing ----------

def sign_request(method: str, full_path: str, body: bytes) -> dict:
    ts_ms = str(int(time.time() * 1000))
    body_hash = hashlib.sha256(body or b"").hexdigest()
    msg = " ".join([method.upper(), full_path, body_hash, API_KEY, ts_ms])
    sig = hmac.new(API_SECRET.encode("utf-8"), msg.encode("utf-8"), hashlib.sha256).hexdigest()
    return {
        "Authorization": API_KEY,
        "X-Authorization-Timestamp": ts_ms,
        "X-Authorization-Signature-SHA256": sig,
    }


# ---------- Chainlink Data Streams: report decoding ----------

def decode_full_report(payload: bytes) -> bytes:
    if len(payload) < 128:
        raise ValueError(f"payload too short: {len(payload)} bytes")
    offset = int.from_bytes(payload[96 + 24: 128], "big")
    if offset < 128 or offset + 32 > len(payload):
        raise ValueError(f"invalid offset {offset}")
    length = int.from_bytes(payload[offset + 24: offset + 32], "big")
    start = offset + 32
    if start + length > len(payload):
        raise ValueError(f"invalid length {length}")
    return payload[start: start + length]


def decode_report_v3(blob: bytes) -> dict:
    WORD = 32
    if len(blob) < 9 * WORD:
        raise ValueError(f"blob too short: {len(blob)} bytes")
    benchmark_raw = int.from_bytes(blob[6 * WORD + 8: 7 * WORD], "big", signed=True)
    bid_raw = int.from_bytes(blob[7 * WORD + 8: 8 * WORD], "big", signed=True)
    ask_raw = int.from_bytes(blob[8 * WORD + 8: 9 * WORD], "big", signed=True)
    return {
        "feed_id": "0x" + blob[0:WORD].hex(),
        "valid_from": int.from_bytes(blob[1 * WORD + 28: 2 * WORD], "big"),
        "observations_ts": int.from_bytes(blob[2 * WORD + 28: 3 * WORD], "big"),
        "expires_at": int.from_bytes(blob[5 * WORD + 28: 6 * WORD], "big"),
        "price_usd": benchmark_raw / PRICE_SCALE,
        "bid_usd": bid_raw / PRICE_SCALE,
        "ask_usd": ask_raw / PRICE_SCALE,
    }


# ---------- Realized volatility (per source) ----------

def realized_sigma_annual(history: deque) -> Optional[float]:
    """Annualized realized vol from a rolling deque of (ts_ms, price) entries."""
    if len(history) < 10:
        return None
    now_ms = history[-1][0]
    cutoff = now_ms - SIGMA_LOOKBACK_MS
    # Walk from end to find samples within lookback
    samples = []
    for ts, p in reversed(history):
        if ts < cutoff:
            break
        samples.append((ts, p))
    if len(samples) < 10:
        return None
    samples.reverse()
    var_sum = 0.0
    n = 0
    for i in range(1, len(samples)):
        t0, p0 = samples[i - 1]
        t1, p1 = samples[i]
        dt_s = (t1 - t0) / 1000.0
        if dt_s <= 0 or p0 <= 0 or p1 <= 0:
            continue
        r = log(p1 / p0)
        var_sum += r * r / dt_s
        n += 1
    if n == 0:
        return None
    var_per_sec = var_sum / n
    sigma_realized = sqrt(max(var_per_sec, 0.0) * SECONDS_PER_YEAR)
    sigma_adjusted = sigma_realized * VOL_RISK_PREMIUM
    return max(SIGMA_FLOOR, min(SIGMA_CEIL, sigma_adjusted))


# ---------- Shared state ----------

class State:
    def __init__(self):
        self.chainlink_price: Optional[float] = None
        self.chainlink_ts: Optional[int] = None
        self.chainlink_obs_ts: Optional[int] = None
        self.binance_price: Optional[float] = None
        self.binance_ts: Optional[int] = None
        self.poly_up_bid: Optional[float] = None
        self.poly_up_ask: Optional[float] = None
        self.poly_down_bid: Optional[float] = None
        self.poly_down_ask: Optional[float] = None
        self.poly_ts: Optional[int] = None
        self.window_end_ts: Optional[int] = None
        self.token_up: Optional[str] = None
        self.token_down: Optional[str] = None
        self.stop_event: Optional[asyncio.Event] = None
        self.subscription_epoch: int = 0
        # Per-source strikes (set at each window rollover)
        self.chainlink_strike: Optional[float] = None
        self.binance_strike: Optional[float] = None
        # Rolling price history for realized-vol per source
        self.chainlink_history: deque = deque(maxlen=200000)
        self.binance_history: deque = deque(maxlen=200000)
        # Deribit vol smile — independent forward σ source (NOT Poly back-solve)
        self.deribit_surface: Optional[list] = None  # list of (log_moneyness, iv_decimal)
        self.deribit_iv: Optional[float] = None       # ATM IV derived from surface, for display
        self.deribit_iv_ts: Optional[int] = None


# ---------- Discovery helpers ----------

async def discover_poly_market(session: aiohttp.ClientSession, window_start: Optional[int] = None) -> dict:
    if window_start is None:
        now_ts = int(time.time())
        window_start = (now_ts // 300) * 300
    window_end_ts = window_start + 300
    slug = f"btc-updown-5m-{window_start}"
    url = f"https://gamma-api.polymarket.com/markets?slug={slug}"
    async with session.get(url, timeout=aiohttp.ClientTimeout(total=10)) as r:
        r.raise_for_status()
        markets = await r.json()
    if not markets:
        raise RuntimeError(f"no market found for slug={slug}")
    m = markets[0]
    token_ids = json.loads(m["clobTokenIds"])
    return {
        "slug": m["slug"],
        "up_token": token_ids[0],
        "down_token": token_ids[1],
        "window_end_ts": window_end_ts,
    }


async def fetch_binance_strike(session: aiohttp.ClientSession, window_start_ts: int) -> float:
    """BTCUSDT 5m candle open at window_start_ts (rounded to whole dollar — engine convention)."""
    url = (
        "https://api.binance.com/api/v3/klines"
        f"?symbol=BTCUSDT&interval=5m&startTime={window_start_ts * 1000}&limit=1"
    )
    async with session.get(url, timeout=aiohttp.ClientTimeout(total=10)) as r:
        r.raise_for_status()
        data = await r.json()
    if not data:
        raise RuntimeError(f"Binance kline empty for {window_start_ts}")
    return round(float(data[0][1]))


async def fetch_chainlink_strike(session: aiohttp.ClientSession, window_start_ts: int) -> float:
    """Chainlink BTC/USD report AT window_start_ts via REST timestamp lookup."""
    full_path = f"/api/v1/reports?feedID={BTC_FEED_ID}&timestamp={window_start_ts}"
    url = CHAINLINK_REST_BASE + full_path
    headers = sign_request("GET", full_path, b"")
    async with session.get(url, headers=headers, timeout=aiohttp.ClientTimeout(total=10)) as r:
        if r.status != 200:
            raise RuntimeError(f"chainlink strike HTTP {r.status}: {(await r.text())[:200]}")
        data = await r.json()
    hs = data["report"]["fullReport"]
    if hs.startswith("0x"):
        hs = hs[2:]
    blob = decode_full_report(bytes.fromhex(hs))
    return decode_report_v3(blob)["price_usd"]


def find_strike_from_history(history: deque, target_ts_ms: int) -> Optional[float]:
    """Find the tick in history closest to target_ts_ms. None if empty."""
    if not history:
        return None
    best_diff = float("inf")
    best_price = None
    for ts, p in history:
        diff = abs(ts - target_ts_ms)
        if diff < best_diff:
            best_diff = diff
            best_price = p
    return best_price


async def fetch_deribit_vol_surface(session: aiohttp.ClientSession) -> Optional[list]:
    """Fetch Deribit BTC nearest-expiry vol smile: list of (log_moneyness, iv_decimal).
    NOT a Poly back-solve — uses BTC option market on Deribit as independent σ source.
    log_moneyness = ln(deribit_strike / deribit_underlying); call+put IVs at same strike averaged.
    """
    from datetime import datetime, timezone
    url = "https://www.deribit.com/api/v2/public/get_book_summary_by_currency?currency=BTC&kind=option"
    async with session.get(url, timeout=aiohttp.ClientTimeout(total=10)) as r:
        r.raise_for_status()
        data = await r.json()
    items = data.get("result", [])
    if not items:
        return None
    now_ts = time.time()
    parsed = []
    for d in items:
        try:
            mark_iv = float(d.get("mark_iv") or 0)
            underlying = float(d.get("underlying_price") or 0)
            if mark_iv <= 0 or underlying <= 0:
                continue
            parts = d["instrument_name"].split("-")
            if len(parts) < 4:
                continue
            exp = datetime.strptime(parts[1], "%d%b%y").replace(tzinfo=timezone.utc).timestamp()
            if exp <= now_ts:
                continue
            strike = float(parts[2])
            parsed.append((exp, strike, underlying, mark_iv))
        except Exception:
            continue
    if not parsed:
        return None
    nearest_exp = min(p[0] for p in parsed)
    by_strike: dict = {}
    underlying = None
    for exp, strike, u, iv in parsed:
        if exp != nearest_exp:
            continue
        if underlying is None:
            underlying = u
        by_strike.setdefault(strike, []).append(iv)
    if not by_strike or not underlying:
        return None
    surface = []
    for strike, ivs in by_strike.items():
        iv_avg = (sum(ivs) / len(ivs)) / 100.0  # mark_iv is percent → decimal
        log_mny = log(strike / underlying)
        surface.append((log_mny, iv_avg))
    surface.sort(key=lambda x: x[0])
    return surface


def interp_iv_for_moneyness(surface: list, log_mny: float) -> Optional[float]:
    """Linear-interpolate IV on the Deribit smile at given log-moneyness.
    Out of surface range → nearest edge IV (flat extrapolation)."""
    if not surface:
        return None
    if log_mny <= surface[0][0]:
        return surface[0][1]
    if log_mny >= surface[-1][0]:
        return surface[-1][1]
    for i in range(1, len(surface)):
        x0, y0 = surface[i - 1]
        x1, y1 = surface[i]
        if x0 <= log_mny <= x1:
            if x1 == x0:
                return y0
            t = (log_mny - x0) / (x1 - x0)
            return y0 + t * (y1 - y0)
    return None


async def deribit_iv_task(state: "State"):
    """Refresh state.deribit_surface every DERIBIT_REFRESH_S seconds."""
    timeout = aiohttp.ClientTimeout(total=10)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        while not state.stop_event.is_set():
            try:
                surf = await fetch_deribit_vol_surface(session)
                if surf:
                    state.deribit_surface = surf
                    state.deribit_iv_ts = int(time.time() * 1000)
                    atm = interp_iv_for_moneyness(surf, 0.0)
                    state.deribit_iv = atm  # for display
                    iv_min = min(s[1] for s in surf)
                    iv_max = max(s[1] for s in surf)
                    print(f"[deribit] surface {len(surf)} pts  ATM={atm:.3f}  "
                          f"IV[{iv_min:.3f}-{iv_max:.3f}]  log_mny[{surf[0][0]:+.3f},{surf[-1][0]:+.3f}]",
                          file=sys.stderr)
            except Exception as e:
                print(f"[deribit] {type(e).__name__}: {e}", file=sys.stderr)
            await asyncio.sleep(DERIBIT_REFRESH_S)


async def _refine_strikes_via_api(state: "State", ws_start_ts: int):
    """Background: retry API strike fetch ~40s; overwrite in-memory fallback when accurate values arrive."""
    timeout = aiohttp.ClientTimeout(total=10)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        bi_done = False
        cl_done = False
        for _ in range(20):
            if not bi_done:
                try:
                    v = await fetch_binance_strike(session, ws_start_ts)
                    state.binance_strike = v
                    bi_done = True
                    print(f"[refine] binance strike → ${v}", file=sys.stderr)
                except Exception:
                    pass
            if not cl_done:
                try:
                    v = await fetch_chainlink_strike(session, ws_start_ts)
                    state.chainlink_strike = v
                    cl_done = True
                    print(f"[refine] chainlink strike → ${v:.2f}", file=sys.stderr)
                except Exception:
                    pass
            if bi_done and cl_done:
                return
            await asyncio.sleep(2)
        if not (bi_done and cl_done):
            print(f"[refine] partial: bi={bi_done} cl={cl_done} — keeping in-memory fallback",
                  file=sys.stderr)


# ---------- Snapshot builder (used by csv + web) ----------

def compute_snapshot(state: State) -> dict:
    ts_ms = int(time.time() * 1000)
    t_left = (state.window_end_ts - ts_ms / 1000) if state.window_end_ts else None

    # σ strategy (iter 6 / Option D): per-source σ = Deribit vol-smile IV at log(K/S) × weight.
    # Each source uses its OWN K and S → vol-smile-aware: extreme positions use wing IV,
    # ATM positions use ATM IV. Fully independent of Polymarket (no back-solve).
    surf = state.deribit_surface
    sigma_cl = sigma_bi = SIGMA_FALLBACK
    if surf:
        if state.chainlink_strike and state.chainlink_price and state.chainlink_price > 0:
            lm = log(state.chainlink_strike / state.chainlink_price)
            iv = interp_iv_for_moneyness(surf, lm)
            if iv:
                sigma_cl = iv * DERIBIT_IV_WEIGHT
        if state.binance_strike and state.binance_price and state.binance_price > 0:
            lm = log(state.binance_strike / state.binance_price)
            iv = interp_iv_for_moneyness(surf, lm)
            if iv:
                sigma_bi = iv * DERIBIT_IV_WEIGHT
    else:
        sigma_cl = realized_sigma_annual(state.chainlink_history) or SIGMA_FALLBACK
        sigma_bi = realized_sigma_annual(state.binance_history) or SIGMA_FALLBACK
    sigma_cl = max(SIGMA_FLOOR, min(SIGMA_CEIL, sigma_cl))
    sigma_bi = max(SIGMA_FLOOR, min(SIGMA_CEIL, sigma_bi))

    fv_cl = None
    fv_bi = None
    if t_left is not None and t_left > 0:
        fv_cl = fv_binary_up(state.chainlink_price, state.chainlink_strike, sigma_cl, t_left)
        fv_bi = fv_binary_up(state.binance_price, state.binance_strike, sigma_bi, t_left)

    cl_minus_bi = (
        state.chainlink_price - state.binance_price
        if (state.chainlink_price is not None and state.binance_price is not None)
        else None
    )

    return {
        "ts_ms": ts_ms,
        "chainlink_price": state.chainlink_price,
        "chainlink_obs_ts": state.chainlink_obs_ts,
        "binance_price": state.binance_price,
        "chainlink_strike": state.chainlink_strike,
        "binance_strike": state.binance_strike,
        "sigma_chainlink": sigma_cl,
        "sigma_binance": sigma_bi,
        "window_end_ts": state.window_end_ts,
        "t_seconds_left": t_left,
        "poly_up_bid": state.poly_up_bid,
        "poly_up_ask": state.poly_up_ask,
        "poly_down_bid": state.poly_down_bid,
        "poly_down_ask": state.poly_down_ask,
        "fv_chainlink_up": fv_cl,
        "fv_binance_up": fv_bi,
        "chainlink_minus_binance": cl_minus_bi,
    }


# ---------- Chainlink Data Streams WebSocket ----------

async def chainlink_ws(state: State):
    full_path = f"/api/v1/ws?feedIDs={BTC_FEED_ID}"
    url = CHAINLINK_WS_BASE + full_path
    print(f"[chainlink-ws] connecting {url[:80]}...", file=sys.stderr)
    while not state.stop_event.is_set():
        try:
            headers = sign_request("GET", full_path, b"")
            hdr_list = [(k, v) for k, v in headers.items()]
            ck = {"ping_interval": 20, "ping_timeout": 15, "max_size": 4 * 1024 * 1024}
            try:
                ws_cm = websockets.connect(url, additional_headers=hdr_list, **ck)
            except TypeError:
                ws_cm = websockets.connect(url, extra_headers=hdr_list, **ck)
            async with ws_cm as ws:
                print("[chainlink-ws] connected", file=sys.stderr)
                async for msg in ws:
                    if state.stop_event.is_set():
                        return
                    try:
                        data = json.loads(msg)
                        rep = data.get("report") or {}
                        fr = rep.get("fullReport") or ""
                        if not fr:
                            continue
                        if fr.startswith("0x"):
                            fr = fr[2:]
                        blob = decode_full_report(bytes.fromhex(fr))
                        dec = decode_report_v3(blob)
                        state.chainlink_price = dec["price_usd"]
                        state.chainlink_obs_ts = dec["observations_ts"]
                        state.chainlink_ts = int(time.time() * 1000)
                        state.chainlink_history.append((state.chainlink_ts, state.chainlink_price))
                    except Exception as e:
                        print(f"[chainlink-ws] decode err: {type(e).__name__}: {e}", file=sys.stderr)
        except Exception as e:
            print(f"[chainlink-ws] {type(e).__name__}: {e} — reconnecting", file=sys.stderr)
            await asyncio.sleep(2)


# ---------- Binance WS ----------

async def binance_ws(state: State):
    print("[binance] connecting", file=sys.stderr)
    while not state.stop_event.is_set():
        try:
            async with websockets.connect(BINANCE_WS_URL, ping_interval=20, ping_timeout=10) as ws:
                async for msg in ws:
                    if state.stop_event.is_set():
                        return
                    try:
                        d = json.loads(msg)
                        p = d.get("data", d)
                        bid = float(p.get("b", 0))
                        ask = float(p.get("a", 0))
                        if bid > 0 and ask > 0:
                            state.binance_price = (bid + ask) / 2
                            state.binance_ts = int(time.time() * 1000)
                            state.binance_history.append((state.binance_ts, state.binance_price))
                    except (ValueError, KeyError):
                        continue
        except Exception as e:
            print(f"[binance] {type(e).__name__}: {e} — reconnecting", file=sys.stderr)
            await asyncio.sleep(2)


# ---------- Polymarket WS (resubscribes on rollover) ----------

async def polymarket_ws(state: State):
    while not state.stop_event.is_set():
        while not (state.token_up and state.token_down) and not state.stop_event.is_set():
            await asyncio.sleep(0.1)
        if state.stop_event.is_set():
            return

        my_epoch = state.subscription_epoch
        token_up, token_down = state.token_up, state.token_down
        books = {token_up: {"bids": {}, "asks": {}}, token_down: {"bids": {}, "asks": {}}}
        sub = {"type": "Market", "assets_ids": [token_up, token_down]}
        print(f"[poly] sub UP={token_up[:14]}... DOWN={token_down[:14]}... epoch={my_epoch}", file=sys.stderr)

        def _update_mid(token: str):
            b = books[token]
            bids = [p for p, s in b["bids"].items() if s > 0]
            asks = [p for p, s in b["asks"].items() if s > 0]
            if not bids or not asks:
                return
            best_bid = max(bids)
            best_ask = min(asks)
            if token == token_up:
                state.poly_up_bid = best_bid
                state.poly_up_ask = best_ask
            elif token == token_down:
                state.poly_down_bid = best_bid
                state.poly_down_ask = best_ask
            state.poly_ts = int(time.time() * 1000)

        try:
            async with websockets.connect(POLY_WS_URL, ping_interval=20, ping_timeout=10) as ws:
                await ws.send(json.dumps(sub))
                while not state.stop_event.is_set():
                    if state.subscription_epoch != my_epoch:
                        print(f"[poly] epoch {my_epoch}→{state.subscription_epoch}", file=sys.stderr)
                        break
                    try:
                        msg = await asyncio.wait_for(ws.recv(), timeout=1.0)
                    except asyncio.TimeoutError:
                        continue
                    try:
                        data = json.loads(msg)
                    except ValueError:
                        continue
                    events = data if isinstance(data, list) else [data]
                    for ev in events:
                        et = ev.get("event_type") or ev.get("type")
                        tok = ev.get("asset_id")
                        if tok not in books:
                            continue
                        if et == "book":
                            books[tok]["bids"] = {float(x["price"]): float(x["size"]) for x in ev.get("bids", [])}
                            books[tok]["asks"] = {float(x["price"]): float(x["size"]) for x in ev.get("asks", [])}
                            _update_mid(tok)
                        elif et == "price_change":
                            for ch in ev.get("changes", []):
                                p = float(ch["price"])
                                size = float(ch["size"])
                                side_key = "bids" if ch["side"].lower() == "buy" else "asks"
                                if size == 0:
                                    books[tok][side_key].pop(p, None)
                                else:
                                    books[tok][side_key][p] = size
                            _update_mid(tok)
        except Exception as e:
            print(f"[poly] {type(e).__name__}: {e}", file=sys.stderr)
            await asyncio.sleep(1)
        state.poly_up_bid = state.poly_up_ask = None
        state.poly_down_bid = state.poly_down_ask = None


# ---------- Window watcher: refresh both strikes per window + auto-stop ----------

async def window_watcher(state: State, n_stop: int):
    start_ts = time.time()
    first_full_start = ((int(start_ts) // 300) + 1) * 300
    target_stop_ts = first_full_start + n_stop * 300
    print(f"[watcher] will stop at unix {target_stop_ts} "
          f"(~{(target_stop_ts - start_ts)/60:.1f}min, {n_stop} full windows)",
          file=sys.stderr)

    seen_starts = set()
    if state.window_end_ts:
        seen_starts.add(state.window_end_ts - 300)

    timeout = aiohttp.ClientTimeout(total=10)
    async with aiohttp.ClientSession(timeout=timeout) as session:
        while not state.stop_event.is_set():
            now = time.time()
            if now >= target_stop_ts:
                print(f"[watcher] {n_stop} full windows done — STOPPING", file=sys.stderr)
                state.stop_event.set()
                return

            current_start = (int(now) // 300) * 300
            if current_start not in seen_starts:
                seen_starts.add(current_start)
                print(f"[watcher] window rollover → {current_start} (full seen={len(seen_starts) - 1})", file=sys.stderr)
                # Refresh poly tokens
                try:
                    m = await discover_poly_market(session, window_start=current_start)
                    state.token_up = m["up_token"]
                    state.token_down = m["down_token"]
                    state.window_end_ts = m["window_end_ts"]
                    state.subscription_epoch += 1
                    print(f"[watcher] new tokens up={m['up_token'][:14]}...", file=sys.stderr)
                except Exception as e:
                    print(f"[watcher] poly discover failed: {e}", file=sys.stderr)
                # Step 1: in-memory fallback (zero-latency; WS history always has data after first ticks)
                ws_start_ms = current_start * 1000
                cl_mem = find_strike_from_history(state.chainlink_history, ws_start_ms)
                bi_mem = find_strike_from_history(state.binance_history, ws_start_ms)
                if cl_mem is not None:
                    state.chainlink_strike = cl_mem
                if bi_mem is not None:
                    state.binance_strike = bi_mem
                print(f"[watcher] mem strikes: CL={state.chainlink_strike} BI={state.binance_strike}", file=sys.stderr)
                # Step 2: background API refinement — overwrites mem fallback when accurate
                asyncio.create_task(_refine_strikes_via_api(state, current_start))

            await asyncio.sleep(0.5)


# ---------- CSV logger (1ms snapshots) ----------

async def csv_logger(state: State):
    new_file = not os.path.exists(CSV_PATH) or os.path.getsize(CSV_PATH) == 0
    f = open(CSV_PATH, "a", newline="", buffering=1024 * 1024)
    w = csv.writer(f)
    cols = [
        "ts_ms",
        "chainlink_price", "chainlink_obs_ts", "binance_price",
        "chainlink_strike", "binance_strike",
        "sigma_chainlink", "sigma_binance",
        "window_end_ts", "t_seconds_left",
        "poly_up_bid", "poly_up_ask",
        "poly_down_bid", "poly_down_ask",
        "fv_chainlink_up", "fv_binance_up",
        "chainlink_minus_binance",
    ]
    if new_file:
        w.writerow(cols)
    print(f"[csv] writing → {CSV_PATH} @ every {SNAPSHOT_INTERVAL_S*1000:.1f}ms", file=sys.stderr)
    n = 0
    last_stat = time.time()
    while not state.stop_event.is_set():
        s = compute_snapshot(state)
        w.writerow([s.get(c) for c in cols])
        n += 1
        if n % FLUSH_EVERY_N_ROWS == 0:
            f.flush()
        if time.time() - last_stat > 30:
            sig_cl = f"{s['sigma_chainlink']:.3f}" if s["sigma_chainlink"] else "NA"
            sig_bi = f"{s['sigma_binance']:.3f}" if s["sigma_binance"] else "NA"
            print(f"[csv] rows={n} CL={s['chainlink_price']} BI={s['binance_price']} "
                  f"σ_CL={sig_cl} σ_BI={sig_bi} "
                  f"K_CL={s['chainlink_strike']} K_BI={s['binance_strike']}",
                  file=sys.stderr)
            last_stat = time.time()
        await asyncio.sleep(SNAPSHOT_INTERVAL_S)
    f.flush()
    f.close()
    print(f"[csv] closed, total rows={n}", file=sys.stderr)


# ---------- Web UI ----------

HTML_PAGE = """<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Chainlink vs Binance vs Polymarket</title>
<style>
*{box-sizing:border-box}
body{font-family:-apple-system,BlinkMacSystemFont,'SF Mono',Menlo,monospace;background:#0d1117;color:#e6edf3;padding:24px;margin:0}
h1{color:#58a6ff;font-size:20px;margin:0 0 4px 0}
.sub{color:#8b949e;font-size:12px;margin-bottom:20px}
.grid{display:grid;grid-template-columns:repeat(2,1fr);gap:14px;max-width:1200px}
.card{background:#161b22;padding:14px 18px;border-radius:8px;border:1px solid #30363d}
.card.wide{grid-column:span 2}
.label{color:#8b949e;font-size:11px;text-transform:uppercase;letter-spacing:.6px;margin-bottom:6px}
.value{font-size:26px;font-weight:600;color:#c9d1d9;font-variant-numeric:tabular-nums;line-height:1.1}
.delta{font-size:13px;color:#8b949e;margin-top:6px;font-variant-numeric:tabular-nums}
.tinyrow{display:flex;justify-content:space-between;font-size:12px;color:#8b949e;margin-top:6px;font-variant-numeric:tabular-nums}
.poly-grid{display:grid;grid-template-columns:90px repeat(2,1fr);gap:6px 24px;align-items:center}
.poly-grid .h{color:#8b949e;font-size:11px;text-transform:uppercase;letter-spacing:.6px}
.poly-grid .v{font-size:18px;font-variant-numeric:tabular-nums;color:#c9d1d9}
.up{color:#3fb950;font-weight:600}.dn{color:#f85149;font-weight:600}
.green{color:#3fb950}.red{color:#f85149}.warn{color:#d29922}
.tag{display:inline-block;padding:2px 8px;border-radius:4px;background:#21262d;color:#8b949e;font-size:10px;text-transform:uppercase;letter-spacing:.6px;margin-left:8px}
.tag.live{background:#1f6feb;color:white}
.footer{color:#6e7681;font-size:11px;margin-top:18px;max-width:1200px}
</style>
</head>
<body>
<h1>Chainlink <span class="tag live">settlement</span> vs Binance <span class="tag">engine</span> vs Polymarket <span class="tag">live book</span></h1>
<div class="sub">Per-source: each side uses its OWN price + own strike + own realized σ (no cross-contamination). window ends <span id="winend">—</span> · t-left <span id="tleft">—</span>s · refresh 500ms</div>
<div class="grid">
  <div class="card">
    <div class="label">Chainlink BTC/USD <span class="tag live">settlement src</span></div>
    <div class="value" id="cl">—</div>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:14px;margin-top:14px;padding-top:12px;border-top:1px solid #21262d">
      <div>
        <div style="color:#8b949e;font-size:10px;text-transform:uppercase;letter-spacing:0.6px;margin-bottom:4px">strike (Chainlink @ window-start)</div>
        <div style="color:#c9d1d9;font-size:20px;font-variant-numeric:tabular-nums;font-weight:600" id="cl-k">—</div>
      </div>
      <div>
        <div style="color:#8b949e;font-size:10px;text-transform:uppercase;letter-spacing:0.6px;margin-bottom:4px">σ annualized (60s realized)</div>
        <div style="color:#c9d1d9;font-size:20px;font-variant-numeric:tabular-nums;font-weight:600" id="cl-sig">—</div>
      </div>
    </div>
    <div style="color:#6e7681;font-size:11px;margin-top:10px;font-variant-numeric:tabular-nums">obs_ts <span id="cl-obs">—</span></div>
  </div>
  <div class="card">
    <div class="label">Binance BTCUSDT mid</div>
    <div class="value" id="bi">—</div>
    <div style="display:grid;grid-template-columns:1fr 1fr;gap:14px;margin-top:14px;padding-top:12px;border-top:1px solid #21262d">
      <div>
        <div style="color:#8b949e;font-size:10px;text-transform:uppercase;letter-spacing:0.6px;margin-bottom:4px">strike (Binance 5m open)</div>
        <div style="color:#c9d1d9;font-size:20px;font-variant-numeric:tabular-nums;font-weight:600" id="bi-k">—</div>
      </div>
      <div>
        <div style="color:#8b949e;font-size:10px;text-transform:uppercase;letter-spacing:0.6px;margin-bottom:4px">σ annualized (60s realized)</div>
        <div style="color:#c9d1d9;font-size:20px;font-variant-numeric:tabular-nums;font-weight:600" id="bi-sig">—</div>
      </div>
    </div>
    <div style="color:#6e7681;font-size:11px;margin-top:10px;font-variant-numeric:tabular-nums">CL−BI <span id="delta">—</span></div>
  </div>
  <div class="card">
    <div class="label">FV (Chainlink) → P(UP wins)</div>
    <div class="value green" id="fv-cl">—</div>
    <div class="delta">distance to poly_up_mid: <span id="gap-cl">—</span></div>
  </div>
  <div class="card">
    <div class="label">FV (Binance) → P(UP wins)</div>
    <div class="value warn" id="fv-bi">—</div>
    <div class="delta">distance to poly_up_mid: <span id="gap-bi">—</span></div>
  </div>
  <div class="card wide">
    <div class="label">Polymarket order book</div>
    <div class="poly-grid">
      <div></div><div class="h up">UP</div><div class="h dn">DOWN</div>
      <div class="h">Best Ask</div><div class="v" id="up-ask">—</div><div class="v" id="dn-ask">—</div>
      <div class="h">Best Bid</div><div class="v" id="up-bid">—</div><div class="v" id="dn-bid">—</div>
      <div class="h">Mid</div><div class="v" id="up-mid">—</div><div class="v" id="dn-mid">—</div>
    </div>
  </div>
</div>
<div class="footer" id="footer">connecting...</div>
<script>
function fmt(v,d){return v==null?'—':Number(v).toFixed(d)}
function fmtUsd(v){return v==null?'—':'$'+Number(v).toLocaleString('en-US',{minimumFractionDigits:2,maximumFractionDigits:2})}
function fmtUsdInt(v){return v==null?'—':'$'+Number(v).toLocaleString()}
function fmtPct(v){return v==null?'—':(v*100).toFixed(2)+'%'}
function fmtDelta(v){if(v==null)return '—';const sign=v>=0?'+':'';const cls=v>0.5?'green':(v<-0.5?'red':'');return '<span class="'+cls+'">'+sign+Number(v).toFixed(2)+'</span>'}
function fmtGap(v){if(v==null)return '—';return (v>=0?'+':'')+Number(v).toFixed(4)}
async function refresh(){
  try{
    const r=await fetch('/api/state');const d=await r.json();
    document.getElementById('cl').textContent=fmtUsd(d.chainlink_price);
    document.getElementById('cl-k').textContent=fmtUsdInt(d.chainlink_strike);
    document.getElementById('cl-sig').textContent=fmt(d.sigma_chainlink,3);
    document.getElementById('cl-obs').textContent=d.chainlink_obs_ts||'—';
    document.getElementById('bi').textContent=fmtUsd(d.binance_price);
    document.getElementById('bi-k').textContent=fmtUsdInt(d.binance_strike);
    document.getElementById('bi-sig').textContent=fmt(d.sigma_binance,3);
    document.getElementById('delta').innerHTML=fmtDelta(d.chainlink_minus_binance);
    document.getElementById('fv-cl').textContent=fmtPct(d.fv_chainlink_up);
    document.getElementById('fv-bi').textContent=fmtPct(d.fv_binance_up);
    const upMid=(d.poly_up_bid!=null&&d.poly_up_ask!=null)?(d.poly_up_bid+d.poly_up_ask)/2:null;
    const dnMid=(d.poly_down_bid!=null&&d.poly_down_ask!=null)?(d.poly_down_bid+d.poly_down_ask)/2:null;
    document.getElementById('gap-cl').textContent=(d.fv_chainlink_up!=null&&upMid!=null)?fmtGap(d.fv_chainlink_up-upMid):'—';
    document.getElementById('gap-bi').textContent=(d.fv_binance_up!=null&&upMid!=null)?fmtGap(d.fv_binance_up-upMid):'—';
    document.getElementById('winend').textContent=d.window_end_ts||'—';
    document.getElementById('tleft').textContent=d.t_seconds_left==null?'—':Number(d.t_seconds_left).toFixed(1);
    document.getElementById('up-ask').textContent=fmt(d.poly_up_ask,4);
    document.getElementById('dn-ask').textContent=fmt(d.poly_down_ask,4);
    document.getElementById('up-bid').textContent=fmt(d.poly_up_bid,4);
    document.getElementById('dn-bid').textContent=fmt(d.poly_down_bid,4);
    document.getElementById('up-mid').textContent=fmt(upMid,4);
    document.getElementById('dn-mid').textContent=fmt(dnMid,4);
    document.getElementById('footer').textContent='last update '+new Date(d.ts_ms).toLocaleTimeString();
  }catch(e){document.getElementById('footer').textContent='fetch error: '+e.message}
}
refresh();setInterval(refresh,500);
</script>
</body>
</html>
"""


async def web_server(state: State):
    async def handle_root(request):
        return web.Response(text=HTML_PAGE, content_type="text/html")

    async def handle_state(request):
        return web.json_response(compute_snapshot(state))

    app = web.Application()
    app.router.add_get("/", handle_root)
    app.router.add_get("/api/state", handle_state)
    runner = web.AppRunner(app)
    await runner.setup()
    port = int(os.environ.get("WEB_PORT", "8765"))
    site = web.TCPSite(runner, "0.0.0.0", port)
    await site.start()
    print(f"[web] listening on http://0.0.0.0:{port}/", file=sys.stderr)
    try:
        await state.stop_event.wait()
    finally:
        await runner.cleanup()


# ---------- Main ----------

async def main():
    if not API_KEY or not API_SECRET:
        print("ERROR: CHAINLINK_DS_API_KEY / CHAINLINK_DS_API_SECRET env vars not set", file=sys.stderr)
        sys.exit(1)

    state = State()
    state.stop_event = asyncio.Event()

    # Initial discovery + per-source strikes
    async with aiohttp.ClientSession() as ds:
        try:
            m = await discover_poly_market(ds)
            state.token_up = m["up_token"]
            state.token_down = m["down_token"]
            state.window_end_ts = m["window_end_ts"]
            print(f"[init] slug={m['slug']} up={state.token_up[:14]}... end_ts={state.window_end_ts}", file=sys.stderr)
        except Exception as e:
            print(f"[init] poly discovery failed: {e}", file=sys.stderr)
        if state.window_end_ts:
            ws_start = state.window_end_ts - 300
            try:
                state.binance_strike = await fetch_binance_strike(ds, ws_start)
                print(f"[init] binance strike = ${state.binance_strike}", file=sys.stderr)
            except Exception as e:
                print(f"[init] binance strike failed: {e}", file=sys.stderr)
            try:
                state.chainlink_strike = await fetch_chainlink_strike(ds, ws_start)
                print(f"[init] chainlink strike = ${state.chainlink_strike:.2f}", file=sys.stderr)
            except Exception as e:
                print(f"[init] chainlink strike failed: {e}", file=sys.stderr)

    print(
        f"[verify] start | feedID={BTC_FEED_ID[:14]}... "
        f"K_CL={state.chainlink_strike} K_BI={state.binance_strike} "
        f"window_end={state.window_end_ts} snap={SNAPSHOT_INTERVAL_S*1000:.1f}ms n_windows={N_WINDOWS}",
        file=sys.stderr,
    )

    tasks = [
        asyncio.create_task(chainlink_ws(state), name="chainlink"),
        asyncio.create_task(binance_ws(state), name="binance"),
        asyncio.create_task(polymarket_ws(state), name="poly"),
        asyncio.create_task(csv_logger(state), name="csv"),
        asyncio.create_task(window_watcher(state, N_WINDOWS), name="watcher"),
        asyncio.create_task(web_server(state), name="web"),
        asyncio.create_task(deribit_iv_task(state), name="deribit"),
    ]

    await state.stop_event.wait()
    print("[main] stop_event set — cancelling tasks", file=sys.stderr)
    for t in tasks:
        t.cancel()
    await asyncio.gather(*tasks, return_exceptions=True)
    print("[main] all tasks finished", file=sys.stderr)


if __name__ == "__main__":
    try:
        asyncio.run(main())
    except KeyboardInterrupt:
        print("\n[verify] interrupted", file=sys.stderr)
