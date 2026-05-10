//! SSE 数据流：每 100ms 投影一帧 DashboardSnapshot 推送给客户端。

use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::response::sse::{Event, KeepAlive, Sse};
use futures::stream::Stream;
use tokio_stream::wrappers::IntervalStream;
use tokio_stream::StreamExt;

use crate::tui::app::AppState;

const TICK_MS: u64 = 100;

pub fn dashboard_stream(
    app_state: Arc<RwLock<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let interval = tokio::time::interval(Duration::from_millis(TICK_MS));
    let stream = IntervalStream::new(interval).map(move |_| {
        let snap = super::snapshot::build(&app_state);
        // serde_json 失败仅在 NaN/Inf 等非常情况；此处用兜底 Event::default 避免中断流
        let payload = serde_json::to_string(&snap).unwrap_or_else(|_| "{}".to_string());
        Ok(Event::default().event("snapshot").data(payload))
    });

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}
