//! 数字格式化辅助。

pub fn fmt_price(p: f64) -> String {
    if p == 0.0 {
        return "—".into();
    }
    if p >= 1000.0 {
        format!("{:>.2}", p)
    } else {
        format!("{:>.4}", p)
    }
}

pub fn fmt_qty(q: f64) -> String {
    if q == 0.0 {
        "—".into()
    } else if q >= 100.0 {
        format!("{:.1}", q)
    } else {
        format!("{:.4}", q)
    }
}

pub fn fmt_money(v: f64) -> String {
    if v.abs() >= 1000.0 {
        format!("{:+.2}", v)
    } else {
        format!("{:+.4}", v)
    }
}

pub fn fmt_pct_bps(bps: f64) -> String {
    format!("{:+.2} bps", bps)
}

pub fn fmt_signed(v: f64) -> String {
    format!("{:+.4}", v)
}

pub fn fmt_int(v: u64) -> String {
    format!("{}", v)
}

pub fn fmt_ms(v: f64) -> String {
    format!("{:.2} ms", v)
}

pub fn fmt_us(v: f64) -> String {
    format!("{:.0} µs", v)
}

pub fn fmt_secs_clock(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn fmt_ts_ms(ts_ms: i64) -> String {
    if ts_ms <= 0 {
        return "--:--:--.---".into();
    }
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ts_ms as f64));
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        date.get_hours(),
        date.get_minutes(),
        date.get_seconds(),
        date.get_milliseconds()
    )
}
