//! v0.5.53: the sidebar plugin area.
//!
//! The sidebar's lower slot (formerly the fixed goal panel) is a generic
//! plugin slot: a thin switch bar (`‹ [plugin ▾] ›`) above the active
//! plugin's view. The slot has a height cap; each plugin view manages its
//! own layout/scrolling inside it.
//!
//! Registering a future plugin: add a `PluginDef` to `PLUGINS` below, add a
//! match arm in `ui::plugin_view`, and mount the view under `#plugin-view`.
//! Nothing else in the chrome (cap, bar, menu) changes.

/// One sidebar plugin. `id` is the routing key stored in
/// `AppState::active_plugin`; `label` is what the switch bar shows.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PluginDef {
    pub id: &'static str,
    pub label: &'static str,
}

/// The registry. Compile-time: one line per plugin. Every loaded plugin is
/// listed here, so the shared plugin bar/menu always shows the loaded set.
pub const PLUGINS: [PluginDef; 3] = [
    PluginDef { id: "goal", label: "goal" },
    PluginDef { id: "essence", label: "essence" },
    PluginDef { id: "rewind", label: "rewind" },
];

pub fn count() -> usize {
    PLUGINS.len()
}

pub fn label(id: &str) -> &'static str {
    PLUGINS
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.label)
        .unwrap_or("?")
}

/// Wraparound navigation (with one plugin both return the same id; the bar
/// disables the arrows when `count() <= 1`).
pub fn next_id(cur: &str) -> &'static str {
    let i = position(cur);
    PLUGINS[(i + 1) % PLUGINS.len()].id
}

pub fn prev_id(cur: &str) -> &'static str {
    let i = position(cur);
    PLUGINS[(i + PLUGINS.len() - 1) % PLUGINS.len()].id
}

fn position(id: &str) -> usize {
    PLUGINS.iter().position(|p| p.id == id).unwrap_or(0)
}
