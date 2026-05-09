"""
Polymarket Market WebSocket 仅用 WS 订阅，无 REST。

官方文档: https://docs.polymarket.com/market-data/websocket/market-channel.md
端点: wss://ws-subscriptions-clob.polymarket.com/ws/market
订阅: { "assets_ids": ["<token_id>"], "type": "market", "custom_feature_enabled": true }
心跳: 每 10 秒发 "PING"，收 "PONG"。

========== WebSocket 单连接会返回的所有事件类型及字段 ==========

1) book
   触发: 首次订阅时 + 有成交影响订单簿时
   字段: event_type, market, asset_id, timestamp, hash,
         bids[], asks[] (每项: price, size),
         tick_size, last_trade_price

2) price_change
   触发: 新挂单或撤单
   字段: event_type, market, timestamp,
         price_changes[] (每项: asset_id, price, size, side, hash, best_bid, best_ask)
   size="0" 表示该档位被撤掉

3) tick_size_change
   触发: 市场 tick 变更（价格触及 >0.96 或 <0.04）
   字段: event_type, asset_id, market, old_tick_size, new_tick_size, timestamp

4) last_trade_price
   触发: 每笔成交（maker/taker 匹配）
   字段: event_type, asset_id, market, price, side, size, fee_rate_bps, timestamp

5) best_bid_ask  (需 custom_feature_enabled: true)
   触发: 最优买卖价变化
   字段: event_type, market, asset_id, best_bid, best_ask, spread, timestamp

6) new_market  (需 custom_feature_enabled: true)
   触发: 新市场创建
   字段: event_type, id, question, market, slug, description,
         assets_ids[], outcomes[], event_message, timestamp

7) market_resolved  (需 custom_feature_enabled: true)
   触发: 市场结算
   字段: event_type, id, question, market, slug, description,
         assets_ids[], outcomes[], winning_asset_id, winning_outcome,
         event_message, timestamp
"""
import asyncio
import websockets
import json
import time
import requests

def get_active_token_id():
    # 1. 对齐到 5 分钟 (300秒)
    now_ts = int(time.time())
    current_window_start = (now_ts // 300) * 300
    expected_slug = f"btc-updown-5m-{current_window_start}"
    
    print(f"[*] 当前时间戳: {now_ts}, 推算预期 Slug: {expected_slug}")
    
    # 2. 请求 Gamma API 精准获取 Token ID
    url = f"https://gamma-api.polymarket.com/markets?slug={expected_slug}"
    try:
        resp = requests.get(url, timeout=10)
        markets = resp.json()
        if markets and len(markets) > 0:
            target = markets[0]
            token_ids = json.loads(target['clobTokenIds'])
            # 拿到第一个作为 Up Token (通常是索引 0)
            up_token = token_ids[0]
            print(f"[+] 精准发现市场: {target['slug']}")
            print(f"[+] 目标 Token ID: {up_token}")
            return up_token, target['slug']
    except Exception as e:
        print(f"[!] 发现逻辑失败: {e}")
    
    return None, None


async def test_poly_ws():
    token_id, slug = get_active_token_id()
    if not token_id:
        print("[-] 无法获取活跃 Token ID，请检查网络或时间戳规则。")
        return

    url = "wss://ws-subscriptions-clob.polymarket.com/ws/market"
    async with websockets.connect(url) as websocket:
        # 订阅该 Token 的 OrderBook（新 API 使用 assets_ids + type: market）
        sub_msg = {
            "assets_ids": [str(token_id)],
            "type": "market",
            "custom_feature_enabled": True,
        }
        print(f"[*] 正在发送订阅请求: {sub_msg}")
        await websocket.send(json.dumps(sub_msg))

        async def ping_task():
            while True:
                await asyncio.sleep(10)
                await websocket.send("PING")

        ping = asyncio.create_task(ping_task())

        # 接收消息（仅 WebSocket，无 REST）
        print("[*] 仅 WebSocket，等待推送 (最多 5 条消息，字段说明见本文件顶部)...")
        msg_count = 0
        try:
            while msg_count < 5:
                try:
                    raw_resp = await asyncio.wait_for(websocket.recv(), timeout=25)
                    if raw_resp == "PONG":
                        continue
                    msgs = json.loads(raw_resp)
                    for m in (msgs if isinstance(msgs, list) else [msgs]):
                        et = m.get("event_type") or m.get("type")
                        if not et:
                            continue
                        msg_count += 1
                        # 简要摘要 + 完整 JSON（全部字段见文件顶部注释）
                        if et == "book":
                            b, a = (m.get("bids") or [{}])[0], (m.get("asks") or [{}])[0]
                            print(f"\n[WS] {et} | best_bid={b.get('price')} best_ask={a.get('price')} last_trade_price={m.get('last_trade_price')}")
                        elif et == "best_bid_ask":
                            print(f"\n[WS] {et} | best_bid={m.get('best_bid')} best_ask={m.get('best_ask')} spread={m.get('spread')}")
                        elif et == "last_trade_price":
                            print(f"\n[WS] {et} | price={m.get('price')} side={m.get('side')} size={m.get('size')}")
                        elif et == "price_change":
                            pc = (m.get("price_changes") or [{}])[0]
                            print(f"\n[WS] {et} | asset_id={pc.get('asset_id','')[:20]}... price={pc.get('price')} side={pc.get('side')} best_bid={pc.get('best_bid')} best_ask={pc.get('best_ask')}")
                        elif et == "tick_size_change":
                            print(f"\n[WS] {et} | old={m.get('old_tick_size')} new={m.get('new_tick_size')}")
                        elif et == "new_market":
                            print(f"\n[WS] {et} | slug={m.get('slug')} outcomes={m.get('outcomes')}")
                        elif et == "market_resolved":
                            print(f"\n[WS] {et} | winning_asset_id={m.get('winning_asset_id')} winning_outcome={m.get('winning_outcome')}")
                        else:
                            print(f"\n[WS] {et}")
                        print(json.dumps(m, indent=2, ensure_ascii=False))
                        if msg_count >= 5:
                            break
                except asyncio.TimeoutError:
                    print("[!] 等待超时。")
                    break
        finally:
            ping.cancel()
            try:
                await ping
            except asyncio.CancelledError:
                pass

if __name__ == "__main__":
    asyncio.run(test_poly_ws())
