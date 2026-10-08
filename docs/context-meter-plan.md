# 上下文计量器（ContextMeter）方案 — 把 `~242K / 262K` 变成按钮 + 点击开浮层

状态：**方案待批**（2026-10-06 调研）
调研对象：dsh 0.1.6-alpha.2（`/opt/homebrew/lib/node_modules/@deepseek-ai/dsh`）

---

## 0. 需求

1. 每条 assistant message 尾部的 `242607 in | 227 out | 242432 cached` **太冗杂** → 去掉。
2. 这些信息以**图形化**方式并入顶部上下文条。
3. 顶部 `~242K / 262K` 那串文字**变成按钮**，**点开浮层看详情**。
4. 浮层的形态照 dsh 的成熟设计。

---

## 1. dsh 的原版设计（源码级）

### 1.1 组件：`ContextMeter`
`@deepseek-ai/dsh-client-ui-conversation/lib/client.js:15818-16026`（骨架 JSX）/ CSS 见 15826 行内联
（doc 注释原文：*"Composer context-occupancy meter: a ring and percentage below the card fed by the
`contextPressure` projection, with a click-open panel of the heuristic `contextBreakdown` composition
(system prompt, tools, conversation). Renders nothing until a provider reports both pressure and a route capacity."*）

**触发按钮（trigger）**
- `<button class=trigger>` 外包 `Tooltip`（`side:"top"`, `delayMs:200`, 展开时禁用）；
- `aria-label` = 本地化句子（`context.aria`，如 `45% of context used`）；`aria-haspopup="dialog"` + `aria-expanded`；
- 内容 = **14px 环形**（`viewBox 0 0 14 14`，`r=5.5`，`stroke-width:2`，`stroke-linecap:round`，
  `strokeDasharray = C·pct C`，`transform: rotate(-90 7 7)`）+ 百分比文字；
- 样式：`inline-flex`，`border-radius:24px`，`padding:1px 8px`，tertiary 文字色，`tabular-nums`，
  13px；hover / `[aria-expanded=true]` → 背景 `--interactive-bg-hover` + 文字 secondary。

**浮层（panel）**
- `createPortal` 到 `document.body`，`position:fixed`，`z-index:1100`；
- `width: min(264px, 100vw - 24px)`，`border-radius:12px`，`padding:12px`，菜单底色 + prominent 阴影，
  `font-size:12px`，`line-height:20px`；`role="dialog"` + `aria-label`（`context.used`）；
- 未定位前 `visibility:hidden`（防闪跳）。

**浮层内容（自上而下）**
1. `header`（flex, gap 6）：`headline`(=句子前半) + `percent`(主色 500) + `headline`(后半) +
   `figures`（`margin-left:auto`，`~{used} / {window}`，tabular-nums）——**`~50K / 1M` 就在这里**；
   句子按 `\0` 槽切分，保证各语言语序（`45% of context used` / `上下文已用 45%`）。
2. `bar`：**4px 高分段条**，`border-radius:999px`，`gap:1px`，`display:flex`，`overflow:hidden`；
   每段 `width = pct × 该段占比`，`min-width:2px`，`border-radius:1px`，色 = `--meter-tint`。
3. `dl.rows`：三行 `dt`(色块 8×8 + 名称) / `dd`(`~X`)，行内 `space-between`：
   - `context.system` → `--dsw-static-neutral-bluish-400`
   - `context.tools`  → `#a78bfa`
   - `context.messages` → `--dsw-static-blue-450`
4. 无 breakdown 时：单段灰色，宽度 = pct。

### 1.2 定位原语：`useAnchoredPosition`
`@deepseek-ai/dsh-client-ui-primitives/lib/index.js:2281`
- `left = anchorRect.left`；`top = side==="top" ? rect.top - gap - height : rect.bottom + gap`；
- 夹取到视口：`left ∈ [margin, vw - width - margin]`，`top` 同理（**只 clamp，不 flip**）；
- 重定位时机：`scroll`（capture）、`resize`、`ResizeObserver(panel)`；
- ContextMeter 传 `{side:"top", gap:8, margin:12}`（它在输入框下方，所以向上开）。

### 1.3 关闭语义：`useDismissOnOutsidePointer`
`…/primitives/lib/index.js:2346`：`document.pointerdown` → 点击落在 root 与 panel 之外则关闭；
外加 **Escape**（keydown）与「数据变为不可用 → 自动关闭」。

### 1.4 数据口径：`@deepseek-ai/dsh-token-meter`
（README + `lib/index.js`；三个会话投影，replay 计算、确定性、不调模型）
- `contextPressure` = `{ pressureTokens?（最近一次 provider 报的 prompt 大小）,
  projectedTokens?（**下一次请求**的 prompt 预估值）, contextWindow? }`
  → 展示用量 = `projectedTokens ?? pressureTokens`；`percent = round(used / window × 100)`（上限 100）。
- `contextBreakdown` = `{ systemTokens, toolsTokens, messageTokens }` —— **启发式组合**（非账单值）。
- `tokenUsage` = 全会话累计 `uncachedInputTokens / cacheReadTokens / cacheWriteTokens / outputTokens`；
  `billedInputTokens = uncached + cacheRead + cacheWrite`（**三个互斥桶**）。
- 估算启发式：`CHARS_PER_TOKEN = 4`；每块 `BLOCK_OVERHEAD`；系统提示 = `ceil(chars/4) + 4`；
  消息块递归计价；provider 报的 usage **仅在请求信封一致时复用**。
- 上一轮详情面板（同包 `:3568` / `:4095`，`TurnUsagePanel`）字段：
  `模型 · cacheHit% · Uncached input(=input) · Cache read · Cache write(为 0 时不显示) · Output(+reasoning)`。

---

## 2. rushi 的现状与差距

**已有（`web-leptos/src/ui.rs`）**
- 顶部 `#ctx-row`：`[«] context [ #ctx-track(6px) > #ctx-fill ] #ctx-pct #ctx-k [面板按钮]`；
- `#ctx-pct` = `(ctx_used / budget) × 100`（>`90%` danger / `>70%` warn / 否则 accent）；
- `#ctx-k` = `~242K / 262K`（`format_ctx`，1000 进制；纯文本，无光标样式）；
- `#ctx-rounds` = 14px 的轮次芯片条（26×10，信息在 `title`，点击进 round 视图）；
- 数据：`ctx_used` ← assistant_message 的 `usage.input_tokens`（`transcript.rs:497-501`，**顶部条的唯一来源**）；
  打开旧会话由 `ws::rebuild_ctx_bookkeeping()`（`ws.rs:885`）从事件重建，`win.rs:288` 装载；
  `budget()` ← `state.model_ctx`（按模型名查表，fallback 262144）。
- 事件里有 **provider 报的** `input_tokens / output_tokens / cached_tokens`（`transcript.rs:482-497`），
  即 dsh `tokenUsage` 的 DeepSeek 形态（DeepSeek 只报 hit/miss，没有 cache-write 桶）。

**差距**
- rushi 的事件流里**没有请求信封**（无 system prompt 正文、无工具 schema、无 `request/context` 记录）
  → **无法像 dsh 那样精确分解**；
- 但：`in` 是 provider ground truth；消息文本可数；内核 config 里有 `[system_prompt].text` 与
  `[limits]`（`context_budget_tokens = 262144`、`compact_reserve_tokens = 16384` →
  压缩线 = 245760）；**残差法**可以三段齐全且总和精确。
- `rushi-web` 的 `KernelConfig`（`bin/rushi-web/src/main.rs:1105`）目前只解析
  `paths.sessions_root / web.host / web.port`，`system_prompt`、`limits` **没暴露给前端**。

---

## 3. 落地方案（P1）

### 3.1 触发
- `#ctx-k` 文本改成 `<button id="ctx-k-btn">`（内容仍是 `~242K / 262K`）；
- `#ctx-pct` 保留（阈值一眼可见）；
- 属性：`aria-haspopup="dialog"`、`aria-expanded`、`title="context usage"`；
- 样式：`cursor:pointer`、`border-radius:24px`、`padding:1px 8px`；hover / 展开态 → `--bg-elev` + `--shadow`
  （沿用 rushi 的新拟态语言，而非 dsh 的 hover 底色）。

### 3.2 浮层
- 新组件 `ContextPanel`（放 `ui.rs`，与 `ctx_row` 同文件；也可独立 `ctxmeter.rs`）；
- `position:fixed` 挂在 `document.body`（Leptos portal 或直接 append 到 body 的容器）；
- **向下开**（`top = rect.bottom + 8`；rushi 的条在顶部）——`gap 8 / margin 12 / 夹取 / scroll+resize+RO 重定位`
  照抄 dsh 1.2 的规则；未定位前 `visibility:hidden`；
- 关闭：点击外部（`pointerdown`）、Escape、切换会话；
- 尺寸：`width: min(264px, 100vw - 24px)`、`border-radius:12px`、`padding:12px`、`font-size:12px/20px`、
  `z-index` 高于右侧面板。

### 3.3 内容（dsh 结构 + rushi 的两个补充）
```
┌ 上下文已用 45%                    ~242K / 262K ┐
│ ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░░░░░░░░░░░░░░░░░░░ │  4px 分段条
│ ■ 系统提示                       ~24K          │
│ ■ 工具与注入                     ~36K          │
│ ■ 对话                           ~182K         │
│ ─────────────────────────────────────────────  │
│ cache 命中                       99.9%         │   上一轮
│ 未命中输入(新增)                 175           │
│ 缓存读取                         242,432       │
│ 输出                             227           │
└────────────────────────────────────────────────┘
```
- 头部句子「上下文已用 45%」：`%` 主色，其余 muted；右侧 `~used / window` tabular-nums
  （**就是从按钮搬进来的那串数字**，dsh 同款）；
- 分段条：三段宽度 = `pct × 占比`，各段 `min-width:2px`，段间 1px 间隙；
- 图例三行 + 色块：建议用 rushi 主题派生的三色（accent / accent-2 / muted 蓝灰），
  若要 dsh 原味则用 `#a78bfa` 等三色；
- 分隔线 + 「上一轮」`dl`：**cache 命中% / 未命中输入 / 缓存读取 / 输出**（dsh 的 TurnUsage 字段，
  cache-write 恒 0 故不显示）；
- 可选：分段条上加 **1px 压缩线标记**（`(budget - reserve) / budget`，即 93.75% of 262144 = 245760），
  一眼看出「离自动压缩还有多远」——rushi 独有、dsh 没有。

### 3.4 数据与计算
- 新增 state：`ctx_cached`、`ctx_out`（`model.rs`；写入点 `transcript.rs` 的 usage 分支；
  重建点 `ws.rs::rebuild_ctx_bookkeeping` / `win.rs:288`；重置点 `ui.rs:714/822`）；
- 分解（**只在面板打开时算一次**，Memo/`open` 触发，绝不进帧回调——遵守 flat 模式帧路径只读的既有约束）：
  - `messages ≈ Σ(加载窗口内消息与工具文本的 chars)/3.5`（英文 4、中文偏低估 → 用 3.5 折中）；
  - `system ≈ ceil(system_prompt.chars/4) + 4`（dsh 的公式；需要 `rushi-web` 暴露该字段，见 §5 决策 ①）；
  - `tools = in − messages − system`（**残差**；吸收 hooks 注入、时区/essence 注入与估算误差）
    → **三段之和恒等于 provider 报的 `in`**（诚实且不会自相矛盾）；
  - 若走两段方案：`对话 = messages`，`其余 = in − messages`。

### 3.5 消息尾部
- 删除 `usage_badge` 渲染（`transcript.rs:522-525`）；
- **保留** `transcript.rs:497-501` 的 `ctx_used.set(...)`（顶部条的命脉）；
- 想留退路：把该行塞进消息的 `title`（hover 可查，零占位）。

---

## 4. 实现清单

| 文件 | 改动 |
|---|---|
| `web-leptos/src/ui.rs` | `#ctx-k` → 按钮；新增 `ContextPanel` + 定位/关闭逻辑；`#ctx-pct/#ctx-track` 不动 |
| `web-leptos/style.css` | `#ctx-k-btn`（hover/expanded）、`.ctx-panel`、`.ctx-bar/.ctx-seg`、`.ctx-row/.ctx-swatch`、压缩线（≈60 行） |
| `web-leptos/src/model.rs` | `ctx_cached` / `ctx_out` signals |
| `web-leptos/src/transcript.rs` | usage 分支写入两个新 signal；不再渲染 `usage_badge` |
| `web-leptos/src/ws.rs` / `win.rs` | 重建/装载时恢复 cached/out |
| `bin/rushi-web/src/main.rs`（仅三段方案） | `KernelConfig` 增解析 `[system_prompt].text` + `[limits]`（含 `config.session.*.toml` 覆盖），并在现有 API 里回传（≈30 行） |

---

## 5. 待拍板

1. **两段 vs 三段**：两段 = 零服务端改动、立刻可做；三段 = 需上面那 ~30 行服务端改动（更接近 dsh）。
2. **配色**：照抄 dsh 三色，还是融进 rushi 新拟态（推荐后者，三色由 `--accent` 派生）。
3. **按钮内容**：`~242K / 262K`（你的原话）/ 只显示 `%` / dsh 原版环形+%（推荐第一种，= 当前 `#ctx-k` 原位改造）。
4. **上一轮小节**（cache 命中% / 未命中 / 缓存读取 / 输出）是否保留（推荐保留，这正是尾部那行要去的地方）。
5. **压缩线标记**要不要（推荐要）。
6. **消息尾部那行**：删除（推荐）还是降级为 hover `title`。
7. 面板宽度/打开方向：264px / 向下（推荐，照 dsh 尺寸、方向取反）。

---

## 6. 验证计划（实现后）

- CDP 探针：点击开 → 面板出现且已定位（无 `visibility:hidden` 残留）；外点关；Esc 关；切会话关；
- 数字校验：面板「上一轮」四项 == 最新 assistant_message 的 `usage`；
- 定位：把窗口缩到面板放不下 → 检查 clamp（不越界）；
- 性能：面板关闭时 `requestAnimationFrame` 帧路径零额外 DOM（`__rushiPile()` 的 dbg 计数对比）；
- 两模式：flat（默认）与 `?pile=1` 都过一遍；
- 视觉：消息卡片高度减少该行高度（17px × 每条）。
