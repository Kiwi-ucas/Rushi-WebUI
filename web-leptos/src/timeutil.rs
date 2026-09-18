//! Time helpers mirroring the legacy `ts()` / `new Date().toISOString()`.

#[cfg(target_arch = "wasm32")]
mod wasm_time {
    use js_sys::Date;
    use wasm_bindgen::JsValue;

    /// Parse a legacy ts value: all-digit string is epoch (<=11 digits =
    /// seconds, 13 = ms); anything else is an ISO/JS date string.
    fn to_ms(raw: &str) -> Option<f64> {
        let s = raw.trim();
        if s.is_empty() {
            return None;
        }
        if s.chars().all(|c| c.is_ascii_digit()) {
            let n: u64 = s.parse().ok()?;
            return Some(if s.len() <= 11 {
                n as f64 * 1000.0
            } else {
                n as f64
            });
        }
        let d = Date::new(&JsValue::from(s));
        if d.get_time().is_nan() {
            return None;
        }
        Some(d.get_time())
    }

    pub(crate) fn ts(raw: &str) -> String {
        let Some(ms) = to_ms(raw) else {
            return String::new();
        };
        let d = Date::new(&JsValue::from_f64(ms));
        let h = d.get_hours();
        let m = d.get_minutes();
        let s = d.get_seconds();
        format!("{h:02}:{m:02}:{s:02}")
    }

    pub(crate) fn now_iso() -> String {
        js_sys::Date::new_0().to_iso_string().into()
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod wasm_time {
    pub(crate) fn ts(raw: &str) -> String {
        raw.trim().to_string()
    }
    pub(crate) fn now_iso() -> String {
        String::new()
    }
}

pub fn ts(raw: &str) -> String {
    wasm_time::ts(raw)
}

pub fn now_iso() -> String {
    wasm_time::now_iso()
}
