//! SSE 客户端：在浏览器内连接 `/api/stream`，每收到一条 snapshot 写入 signal。

use leptos::prelude::*;
use shared_types::DashboardSnapshot;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use web_sys::{EventSource, MessageEvent};

/// 启动 SSE 监听。返回值的 [`SseHandle`] 通过 [`Drop`] 在组件卸载时关闭连接。
pub fn connect(target: RwSignal<Option<DashboardSnapshot>>) -> SseHandle {
    let es = EventSource::new("/api/stream").expect("无法创建 EventSource('/api/stream')");

    let on_message: Closure<dyn FnMut(MessageEvent)> = Closure::new(move |evt: MessageEvent| {
        let Some(data_js) = evt.data().as_string() else {
            return;
        };
        match serde_json::from_str::<DashboardSnapshot>(&data_js) {
            Ok(snap) => target.set(Some(snap)),
            Err(e) => web_sys::console::warn_1(&format!("SSE 解码失败: {e}").into()),
        }
    });

    // EventSource 默认对 message event 触发；后端用 .event("snapshot") 命名了，故监听 "snapshot"
    es.add_event_listener_with_callback("snapshot", on_message.as_ref().unchecked_ref())
        .expect("addEventListener('snapshot') 失败");

    let on_error: Closure<dyn FnMut(web_sys::Event)> = Closure::new(|_evt| {
        // EventSource 自带重连；这里仅记录一次，避免日志泛滥
    });
    es.set_onerror(Some(on_error.as_ref().unchecked_ref()));

    SseHandle {
        es,
        _on_message: on_message,
        _on_error: on_error,
    }
}

pub struct SseHandle {
    es: EventSource,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(web_sys::Event)>,
}

impl Drop for SseHandle {
    fn drop(&mut self) {
        self.es.close();
    }
}
