//! Trunk 入口：挂载 leptos 根组件到 body。

mod app;
mod components;
mod format;
mod sse;

use app::App;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
