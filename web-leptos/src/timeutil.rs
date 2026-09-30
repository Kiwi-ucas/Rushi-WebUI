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

    // v0.5.48: the HH:MM:SS stamp is no longer rendered (the cards show
    // the full date + time via `ts_full`); kept as the legacy formatter.
    #[allow(dead_code)]
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

    /// v0.5.48: the full local date + time (`YYYY-MM-DD HH:MM:SS`) for
    /// the transcript cards' top-right corner (the sidebar cards no
    /// longer carry a timestamp at all).
    pub(crate) fn ts_full(raw: &str) -> String {
        let Some(ms) = to_ms(raw) else {
            return String::new();
        };
        let d = Date::new(&JsValue::from_f64(ms));
        let y = d.get_full_year();
        let mo = d.get_month() + 1;
        let day = d.get_date();
        let h = d.get_hours();
        let m = d.get_minutes();
        let s = d.get_seconds();
        format!("{y:04}-{mo:02}-{day:02} {h:02}:{m:02}:{s:02}")
    }

    pub(crate) fn now_iso() -> String {
        js_sys::Date::new_0().to_iso_string().into()
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod wasm_time {
    #[allow(dead_code)]
    pub(crate) fn ts(raw: &str) -> String {
        raw.trim().to_string()
    }
    pub(crate) fn ts_full(raw: &str) -> String {
        raw.trim().to_string()
    }
    pub(crate) fn now_iso() -> String {
        String::new()
    }
}

#[allow(dead_code)]
pub fn ts(raw: &str) -> String {
    wasm_time::ts(raw)
}

/// Full local date + time (`YYYY-MM-DD HH:MM:SS`). See the wasm side.
pub fn ts_full(raw: &str) -> String {
    wasm_time::ts_full(raw)
}

pub fn now_iso() -> String {
    wasm_time::now_iso()
}
