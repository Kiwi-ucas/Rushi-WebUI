# Sidebar Plugin Area (v0.5.53) — Design & How to Add a Plugin

The bottom slot of the left sidebar used to be a fixed goal panel. v0.5.53
generalizes it into a **plugin area**: one shared chrome hosts any number of
registered plugins, switchable through a thin bar.

## Layout (top → bottom inside the sidebar)

```
#sidebar-inner (column flex)
├─ .sb-header / #context-bar / #session-list   (unchanged)
└─ #plugin-area                                  ← the shared chrome
   ├─ #plugin-bar                                 thin bar: ‹ [plugin ▾] ›
   │   #plugin-prev (‹) · #plugin-list (name + caret, toggles the menu) · #plugin-next (›)
   │   #plugin-menu (dropdown, shown on toggle; lists every registered plugin)
   └─ #plugin-view                                hosts the ACTIVE plugin's view
```

The height cap that previously lived on `#goal-panel` (30%, v0.5.52) now lives
on `#plugin-area` — so **every** plugin is capped, not just the goal. Within
the cap the active plugin manages its own layout/scrolling (the goal plugin
keeps its v0.5.52 behavior: `.goal-text` is the sole scroller; badge/meta/
actions stay pinned).

## What changed

Rust:
- **NEW `web-leptos/src/plugins.rs`** — the registry:
  - `PluginDef { id: &'static str, label: &'static str }`
  - `pub const PLUGINS: [PluginDef; N]` — every registered plugin
    (currently `goal`, `essence` and `rewind`).
  - helpers: `count()`, `label(id)`, `next_id(cur)`, `prev_id(cur)`
    (wrap-around; `next`/`prev` return the same id when there is only one
    plugin).
- `model.rs` — `AppState` gains two signals: `active_plugin`
  (`RwSignal<String>`, default `"goal"`) and `plugin_menu_open`
  (`RwSignal<bool>`, default `false`).
- `ui.rs` — `PluginArea` component (replaces the old `GoalPanel` mount at
  L1389): renders `#plugin-area` with the bar + menu + `#plugin-view`.
  `plugin_view(active, state)` is a `match` that dispatches to one function
  per plugin; `goal_plugin_view` is the goal's body (goal badge/meta/text +
  the goal action buttons, no longer wrapped in its own panel/header).
  The old `#goal-panel` wrapper and `.goal-header` are gone.
- `lib.rs` — registers `mod plugins;`.

CSS (`web-leptos/style.css`, no JS):
- `#plugin-area` carries the v0.5.52 cap: `flex: 0 1 auto; min-height: 0;
  max-height: 30%;` plus the card chrome (`background`, `box-shadow`,
  `border-top`).
- `#plugin-bar` (thin, `flex: 0 0 auto`, `position: relative`) + its three
  small buttons (`#plugin-prev` / `#plugin-list` / `#plugin-next`), with the
  `:disabled` state grayed out (`opacity: .35`) when there is only one
  plugin.
- `#plugin-menu` — the dropdown popup (absolute, `z-index: 30`, elevated
  card), one `.plugin-item` row per registered plugin, the active one
  marked `.plugin-item.on`.
- `#plugin-view` — `flex: 0 1 auto; min-height: 0` host; the active
  plugin's view renders inside it.
- `#goal-body` / `.goal-text` scroller + pinned `.goal-badge` / `.goal-meta`
  / `.goal-actions` rules are unchanged from v0.5.52; `.goal-text` stays in
  the capsule-in-groove scrollbar group.

## How to add a new plugin

> **Full referenceable ruleset: `docs/plugin-authoring-rules.md`** — the
> seven-rule contract (registry → view → layout → scrollbar → data →
> lifecycle → styling) with copy-paste skeletons. The short version is
> below.

1. **Register it** in `web-leptos/src/plugins.rs`:
   add one `PluginDef { id: "mymod", label: "My Mod" }` to the `PLUGINS`
   array and bump the array length `[PluginDef; N]`.
2. **Give it a view** in `ui.rs`: add a `fn mymod_plugin_view(state:
   AppState) -> impl IntoView` (its content lives inside `#plugin-view`,
   so it manages its own flex/scrolling within the shared cap) and add a
   match arm in `plugin_view`.
3. The switch bar picks it up automatically: the arrows and the dropdown
   iterate `PLUGINS`, so no other UI change is needed.
4. Pure-CSS chrome: the bar/menu/view are shared; add any
   plugin-specific styles after `#plugin-view .plugin-empty` in
   `style.css`.

## Decisions (user-confirmed)
- **Compile-time const array + `match`** for rendering (no runtime registry,
  no string dispatch in the view code).
- **Arrows gray out / disable** when only one plugin is registered
  (`count() <= 1` → `:disabled`).
- **The plugin list is a small dropdown popup** (not an inline row), toggled
  by the middle button; the active plugin name + caret shows on the bar.
- **`goal-header` deleted** — the bar's middle button shows the active
  plugin name in the former `.goal-header` typography (the bar button keeps
  its uppercase/letterspacing look).

## Build & deploy
- `trunk build` → `dist/style-<hash>.css` + `dist/index.html` (served by the
  running debug `rushi-web` on :8480 — refresh to pick it up, no restart).
- Served CSS verified: `#plugin-area { … max-height: 30% }`, `#plugin-bar`,
  `#plugin-menu`, `#plugin-view`, `.plugin-item`, `.plugin-caret` present;
  `#goal-body .goal-text` in all 5 capsule scrollbar lists.

## Verify on screen
1. Bar shows `‹ [GOAL ▾] ›`; with one plugin the arrows are grayed out.
2. Clicking the middle button opens the dropdown; the active item is
   highlighted; picking it closes the menu.
3. A long goal prompt still tops out at ~30% of the sidebar and scrolls
   inside; badge/meta/actions stay pinned; session cards stay visible.
4. Light + dark theme: bar/menu/scrollbar relief consistent with the rest
   of the sidebar.
