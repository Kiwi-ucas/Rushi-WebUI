# Plugin Authoring Rules (rushi-webui shared plugin area)

The referenceable rules for adding a new plugin to the shared sidebar
plugin area (`#plugin-area`, v0.5.53). Follow all seven rules; each maps to
a concrete file. A copy-paste skeleton is at the bottom.

**Currently registered plugins:** `goal` (first, default), `essence`
(read-only view of the harness session-essence store) and `rewind` (the
conversation history tree — see `rewind-plugin.md`; it is also the worked
example of a plugin that owns a whole *rebuilt expanded view* in addition to
its sidebar panel).

---

## Rule 1 — Register it (compile-time)

`web-leptos/src/plugins.rs` is the single source of truth. Add one entry and
bump the array length. No other UI change is needed — the bar arrows and the
dropdown iterate `PLUGINS`.

```rust
pub const PLUGINS: [PluginDef; 3] = [
    PluginDef { id: "goal", label: "goal" },
    PluginDef { id: "essence", label: "essence" },
    PluginDef { id: "rewind", label: "rewind" },
    PluginDef { id: "mymod", label: "My Mod" },   // ← new (N = 4)
];
```

- `id` — stable ASCII key. It is the `match` key in `plugin_view` and the
  value stored in `AppState::active_plugin`.
- `label` — the text shown in the bar (`#plugin-list`) and the dropdown row.
- Keep `id`/`label` `&'static str` (the registry is a `const` array).

## Rule 2 — Give it a view function

In `ui.rs`: write `fn mymod_plugin_view(state: AppState) -> impl IntoView`
(or `AnyView`) and add a match arm to the dispatcher:

```rust
fn plugin_view(active: RwSignal<String>, state: AppState) -> AnyView {
    match active.get().as_str() {
        "goal"    => goal_plugin_view(state).into_any(),
        "essence" => essence_plugin_view(state).into_any(),
        "rewind" => crate::rewind::rewind_plugin_view(state).into_any(),
        "mymod"   => mymod_plugin_view(state).into_any(),   // ← new
        _         => view! { <div class="plugin-empty">{ "plugin not found" }</div> }.into_any(),
    }
}
```

The switch bar / 30% cap / dropdown are **shared chrome** — your view renders
inside `#plugin-view` and must NOT re-declare them (no outer panel, no
header).

## Rule 3 — Layout contract inside the shared 30% cap

Your view is one flex-column root (e.g. `#mymod-body`) with exactly:
- **one** scroller child: `flex: 0 1 auto; min-height: 0; overflow-y: auto;`
  (the `.goal-text` / `.essence-groups` pattern).
- **pinned** footers/actions: `flex: 0 0 auto`.
- a `flex: 0 0 auto` empty/loading placeholder for the no-data case.

The shared `#plugin-area` already caps every plugin at `max-height: 30%` of
the sidebar column, so you only ever need to make the *content* scroll.

## Rule 4 — Register the scroller in the capsule-scrollbar group

`style.css` has 5 unified scrollbar rules (1 Firefox `scrollbar-width/color`
+ 4 webkit `::-webkit-scrollbar*`). Any new scroller's selector must be
appended to **all five** so its scrollbar matches every other scroller:

```
#transcript, …, #goal-body .goal-text, .essence-groups, #mymod-body .mymod-list { scrollbar-width: thin; … }
#mymod-body .mymod-list::-webkit-scrollbar { … }
#mymod-body .mymod-list::-webkit-scrollbar-button { … }
#mymod-body .mymod-list::-webkit-scrollbar-track { … }
#mymod-body .mymod-list::-webkit-scrollbar-thumb { … }
```

## Rule 5 — Data: server endpoint + client loader

For data that lives on disk (session files), add a **read-only** endpoint —
mirror `bin/rushi-web/src/essence.rs`:

```rust
// essence.rs — one module per data source
pub struct MyModEntry { /* serde fields, all `#[serde(default)]` */ }
#[derive(Serialize)] pub struct MyModView { pub entries: Vec<MyModEntry> }
pub fn read(session_dir: &Path) -> MyModView {
    let d = match fs::read_to_string(session_dir.join("mymod.json")) {
        Ok(d) if !d.trim().is_empty() => d, _ => return MyModView { entries: vec![] },
    };
    Ok::<_,_>(serde_json::from_str(&d)).unwrap_or_else(|_| MyModView { entries: vec![] })
}
```

Wire it in `bin/rushi-web/src/main.rs`:
```rust
mod mymod;                                                    // module decl
.route("/api/sessions/{id}/mymod", get(get_mymod))            // route
async fn get_mymod(State(st), Path(id)) -> impl IntoResponse {
    (StatusCode::OK, Json(mymod::read(&st.sessions.session_dir(&id)))).into_response()
}
```

Client loader in `web-leptos/src/api.rs`:
```rust
pub async fn load_mymod(id: &str) -> Result<Vec<MyModEntry>, String> {
    let res = Request::get(&format!("/api/sessions/{id}/mymod")).send().await.map_err(|e| e.to_string())?;
    let text = res.text().await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    serde_json::from_value(v.get("entries").cloned().unwrap_or(Value::Array(vec![]))).map_err(|e| e.to_string())
}
```

## Rule 6 — Fetch on mount + on session change; degrade gracefully

Inside the view, hold a local `RwSignal` and an `Effect` keyed on
`active_session` (the view is re-mounted when the plugin is selected, so this
covers "mount + session switch" in one place):

```rust
fn mymod_plugin_view(state: AppState) -> impl IntoView {
    let entries = create_rw_signal::<Option<Vec<crate::model::MyModEntry>>>(None);
    let sess = state.active_session;
    Effect::new(move || {
        let s = match sess.get() { Some(s) => s.clone(), None => { entries.set(None); return; } };
        let ent = entries;
        spawn_local(async move {
            let v = api::load_mymod(&s).await.unwrap_or_default();
            ent.set(Some(v));
        });
    });
    view! { /* … */ }
}
```

- `unwrap_or_default()` → an empty list, never an error, so a missing/
  malformed file just shows the placeholder.
- The harness owns all *writes* to the session store; the web server is
  read-only (never write from the endpoint).

## Rule 7 — Styling stays in the relief system

- Directional, top-lit shadows only: raised `--shadow`, compact `--shadow-sm`,
  hover/lift `--shadow-hover`, recessed `--groove`. **Never an
  omni-directional (`0 0 …`) halo.**
- No 1px `--border` dividers under the bar (deliberately removed, v0.5.45);
  separate with shadow tokens.
- Use the token set (`--bg`, `--bg-elev`, `--text`, `--text-muted`,
  `--accent`, …); never hard-code hex so the dark theme just overrides
  tokens. Compact raised card = `--shadow`; off/demoted = `--groove`.

## Verify (after any change)

1. `cargo build -p rushi-web` (server) **and** `trunk build` (frontend).
2. `curl` the served CSS (new selector present) and the new endpoint on a
   probe port (`/api/sessions/<name>/mymod` → JSON list).
3. **Refresh :8480** — the frontend reads `dist/` from disk, but a new
   *route* needs the server binary restarted (the old binary 404s it).
4. Check light + dark theme; confirm the long-content scroll stays inside the
   30% cap and pinned footers stay visible.

---

## Copy-paste skeleton (all 7 rules in one block)

**plugins.rs** — add to `PLUGINS` (+ bump N).

**model.rs** — `pub struct MyModEntry { #[serde(default)] pub … }`
(+ `Clone, Debug, Deserialize`), and any shared `RwSignal` in `AppState`.

**api.rs** — `pub async fn load_mymod(id) -> Result<Vec<MyModEntry>, String>`
(see Rule 5).

**ui.rs** —
```rust
fn plugin_view(/* add the "mymod" arm */)
fn mymod_plugin_view(state: AppState) -> impl IntoView { /* Rule 6 */ }
fn mymod_entry_view(e: crate::model::MyModEntry) -> impl IntoView { /* row */ }
```

**bin/rushi-web/src/** — `mymod.rs` (Rule 5) + `main.rs` module/route/handler.

**style.css** — `#mymod-body` (flex column, `min-height:0`), the scroller
class, the pinned footer, entry/chip relief (Rule 7); append the scroller to
all 5 scrollbar lists (Rule 4).
