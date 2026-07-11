# `maki agents` — Claude Code-style multi-session dashboard

A supervisor screen that lists sessions grouped by status (**Needs input / Working /
Completed**), lets you spawn new sessions from a task-prompt box, and navigate between them
with the arrow keys — while sessions run concurrently in the background.

Entry point: **`maki agents`** (new subcommand, sibling to `maki acp`). Plain `maki` is
unchanged (opens a single session as today).

---

## A. Tier 1 — the dashboard view (over existing session storage)

### A1. Persisted session status

`maki-storage/src/sessions.rs`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    #[default]
    Idle,        // exists, not running, not waiting
    Working,     // agent loop actively running
    NeedsInput,  // paused on a PermissionRequest / question
    Completed,   // last run ended (Done)
    Error,       // last run errored
}
```

- Add `pub status: SessionStatus` to `SessionMeta` (with `#[serde(default)]` so old sessions
  load as `Idle`).
- Add `pub status: SessionStatus` **and** `pub summary: Option<String>` (one-line "what it
  did") to `SessionSummary`, and populate both in `Session::list_in()`.
- Bump `Session::version` handling is not required — `serde(default)` covers back-compat.

### A2. Where status transitions are written

Hook the existing `AgentEvent` stream (`maki-agent/src/types.rs:528`). In the event loop
handler (`maki-ui/src/event_loop.rs`, the `agent_rx` drain around line 303), set + persist
status per session:

| Trigger | New status |
|---|---|
| A prompt is submitted / run starts | `Working` |
| `AgentEvent::PermissionRequest` (or a pending question) | `NeedsInput` |
| `AgentEvent::Done { .. }` | `Completed` |
| `AgentEvent::Error { .. }` | `Error` |
| User sends a new message to a `Completed`/`Idle` session | `Working` |

Persistence uses the existing `StorageWriter` (`maki-ui/src/storage_writer.rs`) so it's async
and off the render path. The one-line `summary` = reuse `generate_title()` logic or the last
assistant text snippet (already available in the session log).

### A3. The dashboard component

New file `maki-ui/src/components/session_dashboard.rs`.

- Data: `Session::list_in(cwd, &storage)` → `Vec<SessionSummary>`, partitioned into three
  groups (NeedsInput, Working, Completed/Idle/Error) in that priority order (mirrors the
  Claude Code screenshot).
- Rendering: reuse the row style from `session_picker.rs` (`format_relative_time`,
  title + detail). Group headers: `Needs input`, `Working`, `Completed`. A `+ New session`
  affordance / task-prompt box at the bottom ("Describe a task for a new session").
- It is a **full-screen view**, not a modal overlay (unlike `session_picker`), so it becomes
  its own `AppMode`/screen rather than an `Overlay`.

### A4. Navigation (per your spec)

Handled in the dashboard's key handler + the session view's input handler.

**On the dashboard:**
- `Up` / `Down` — move selection between listed sessions (across group boundaries).
- `Right` / `Enter` — open the highlighted session (switch to its live view).
- `Ctrl+X` — delete the highlighted session (`Session::delete_from`), with the existing
  confirm pattern from `session_picker::handle_delete_key`.
- Typing into the task-prompt box + `Enter` — spawn a new session (see B4).

**Inside a session view:**
- If the **input box is non-empty**: arrows behave normally (Left/Right move the text cursor,
  Up/Down recall history / scroll) — unchanged.
- If the **input box is empty**:
  - `Left` — return to the dashboard.
  - `Up` / `Down` — switch directly to the previous/next session (dashboard order) without
    going through the board (Claude Code parity; optional, can be gated behind a config flag).
  - `Right` — no-op (already in the session).
- `Esc` remains as-is (rewind); `Left`-on-empty is the clean "back" gesture.

Implementation note: the "empty input" check is `app.input_box.is_empty()`. The arrow keys are
already routed through `App::handle_key`; add a branch that, when in a session view and input
is empty, converts Left/Up/Down into dashboard-navigation `AgentCommand`s / view switches.

### A5. Tier-1 acceptance

- `maki agents` opens a full-screen board listing current-dir sessions grouped by status.
- Selecting + `Right`/`Enter` opens a session; `Left` (empty input) returns.
- `Ctrl+X` deletes; status labels reflect the last known state (persisted, survives restart).
- Plain `maki` unchanged.

---

## B. Tier 2 — real background concurrent sessions

Today maki is **one `App` + one `AgentHandles` per process** (`src/cmd/tui.rs` →
`maki_ui::run` returns a single `session_id`). Tier 2 turns the event loop into a
**supervisor** over N sessions, each with its own agent runner, all progressing concurrently.

Good news: the runtime is already **`smol` async tasks + `flume` channels** (verified —
`event_loop.rs:106 smol::spawn`, `agent/mod.rs` unbounded flume channels). Nothing here is
single-threaded by design; the constraint is purely that the current `App` models one session.

### B1. Session registry

New `SessionRuntime` per live session:

```rust
struct SessionRuntime {
    id: String,
    app: App,                 // one App per session (holds its own SessionState + chats)
    handles: AgentHandles,    // its own cmd_tx / agent_rx / agent_tx / answer_tx
    status: SessionStatus,
    summary: Option<String>,
}
```

Supervisor state:

```rust
struct Supervisor {
    sessions: Vec<SessionRuntime>,   // or IndexMap<String, SessionRuntime>
    focused: Option<usize>,          // None => dashboard is showing
    dashboard: SessionDashboard,
    // shared, cloneable resources (see B3)
}
```

`AgentHandles::spawn` (`maki-ui/src/agent/mod.rs:62`) already creates an independent set of
channels + a `smol` task per call — so **spawning N of them just works**; we currently only
call it once. Each session keeps running because its task lives on the smol executor
regardless of which one is focused.

### B2. Event multiplexing

The current loop does `self.handles.agent_rx.try_recv()` (`event_loop.rs:304`) for the single
session. Supervisor version:

- Poll **every** session's `agent_rx` each tick (round-robin `try_recv` over the registry), or
  merge them: give every `AgentHandles` the same `agent_tx` but tag `Envelope` with a
  `session_id` so one `agent_rx` drains all. (Tagging is cleaner; `Envelope` already carries a
  `run_id` — extend with `session_id`.)
- Route each event to the owning `SessionRuntime.app` for its own message/tool state.
- **Also** derive status transitions (A2) at this choke point and push them into the
  dashboard, so "Working → NeedsInput → Completed" updates live even for unfocused sessions.
- Only the **focused** session (or the dashboard) is rendered each frame; background sessions
  update state but don't draw. This keeps the 60 FPS render cheap.

### B3. Shared vs per-session resources

Per session (already per-`AgentHandles` or per-`App`): messages, tool outputs, cmd/answer
channels, permission run-ids, input draft, subagent chats.

Shared (clone the `Arc`/reader into each): `StateDir` storage, `StorageWriter`,
`PermissionManager`, model registry / `model_slot`, MCP readers, Lua `PluginHost` /
`event_handle`, theme + `ui_config`, clipboard. These are already `Arc`-wrapped in `App`
(`storage`, `permissions`, `usage_slot`, `shared_tool_outputs`, `lua_event_handle`), so
sharing is mostly passing the same `Arc` to each `App::new`.

Concurrency is **unlimited** per your decision — no slot cap. (A `max_working` throttle could
be added later purely as a scheduling gate before `Working`; not built now.)

### B4. Spawning from the dashboard

Task-prompt box submit → supervisor:
1. `Session::new(model, cwd)` + `Session::save`.
2. `AgentHandles::spawn(...)` with shared resources → new `SessionRuntime` (`status =
   Working`).
3. Push the initial prompt into that session's queue (same path as `initial_prompt` in
   `event_loop.rs`).
4. Stay on the dashboard (session runs in background) — matches the screenshot where you can
   fire off a task and keep supervising.

### B5. Focus switching

- Dashboard `Right`/`Enter` → `focused = Some(idx)`; render that session's `App`.
- Session `Left` (empty input) → `focused = None`; render dashboard.
- Session `Up`/`Down` (empty input) → `focused = idx ± 1` (dashboard order).
- Ctrl+X on dashboard → cancel that session's runner (`AgentCommand::CancelAll` on its
  `cmd_tx`), drop its `SessionRuntime`, `Session::delete_from`.

### B6. Shutdown / resume

- On quit: send `CancelAll` to every session, flush `StorageWriter`, persist each session's
  status. Print the resume hints for any still-`Working` sessions.
- Status persisted in A1 means the board reconstructs correctly next `maki agents`.

### B7. Tier-2 acceptance

- Multiple sessions show `Working` simultaneously and progress without being focused.
- A session hitting a permission prompt flips to `NeedsInput` on the board while you're
  elsewhere; opening it shows the prompt.
- Spawning from the dashboard starts a background run and returns you to the board.

---

## C. Delivery order (bite-sized, each compiles + is testable)

1. **`SessionStatus` + schema** (A1) — enum, `SessionMeta`/`SessionSummary` fields,
   `list_in` populates them. Unit test round-trips old (no-field) and new JSON. *No UI yet.*
2. **`maki agents` subcommand** (CLI) — `Command::Agents` in `src/cli.rs`, dispatch in
   `src/cmd/mod.rs` → new `cmd/agents.rs` that (initially) just launches the normal UI. Wires
   the entry point before the view exists.
3. **Dashboard component, read-only** (A3) — render grouped list from `list_in`, no nav.
4. **Dashboard navigation** (A4) — Up/Down/Right/Enter/Ctrl+X + `Left`-back; empty-input
   arrow rule in the session view.
5. **Status writes** (A2) — hook `AgentEvent`s to persist status; board reflects it after a
   run (single-session still).
6. **Supervisor skeleton** (B1–B2) — registry + event multiplexing for **one** session routed
   through the new path (behavior identical, but architecture is now N-capable).
7. **Spawn + focus switching** (B4–B5) — task-prompt box, background start, focus in/out.
8. **Concurrency hardening** (B3, B6) — shared `Arc`s verified, clean shutdown, resume.
9. **Polish** — live status updates on the board while unfocused (B2 status push), timestamps,
   empty-state ("Describe a task for a new session").

Steps 1–5 = Tier 1 (usable dashboard). Steps 6–9 = Tier 2 (true concurrency).

## D. Risk notes

- **Biggest risk:** `App` assumes it *is* the session (single `state`, `cmd_tx`). Cleanest
  path is one `App` per `SessionRuntime` (B1) rather than teaching one `App` about many
  sessions — reuses all existing per-session logic untouched.
- **Render cost:** only draw focused view/dashboard; background sessions update state only.
- **Back-compat:** `serde(default)` on new fields; old sessions load as `Idle`.
- **Lua/MCP singletons:** confirm `PluginHost`/`event_handle` are safe to share across
  sessions (they're already `Arc` in `App`); if a plugin assumes one session, gate multi
  behind the `agents` subcommand only.

---

## E. Implementation progress

### Done (branch `feat/agents-dashboard`)

- **Step 1 — `SessionStatus` schema** (`maki-storage/src/sessions.rs`)
  - Added `SessionStatus` enum (`Idle`/`Working`/`NeedsInput`/`Completed`/`Error`, default
    `Idle`, `snake_case` serde).
  - Added `status: SessionStatus` + `summary: Option<String>` to `SessionMeta`
    (`skip_serializing_if` default so old sessions stay byte-identical).
  - Added `status` + `summary` to `SessionSummary`.
  - Extended `ScanRecord::Meta` + `read_last_meta` to surface status/summary from the JSONL
    tail so `list_in()` is still O(tail) cheap.
  - Legacy `.json` sessions default to `Idle`.
  - Tests: `session_status_serde_round_trip` (all variants),
    `scan_surfaces_status_and_defaults_idle_for_legacy` (back-compat + surfacing).

- **Step 2 — `maki agents` subcommand**
  - `Command::Agents { model }` in `src/cli.rs`; dispatch in `src/cmd/mod.rs`.
  - `tui::run` refactored to `run_inner(cli, dashboard)`; new `run_dashboard()` entry.
  - Threaded `dashboard: bool` through `EventLoopParams` -> `App.dashboard`.
  - No behavior change yet: `maki agents` currently launches the normal UI with the flag set.

### Not yet built (next increments)

- Step 3: `session_dashboard` component (grouped, read-only render).
- Step 4: dashboard navigation + empty-input arrow rule in the session view.
- Step 5: status writes on `AgentEvent`s.
- Steps 6-9: Tier-2 supervisor, spawn box, focus switching, concurrency hardening.

### BUILD NOTE

No Rust toolchain was available in the authoring environment, so these edits are **not yet
compiler-verified**. Before the next step, run on a machine with cargo:

```
cargo build -p maki-storage
cargo clippy --all --tests -- -D warnings
cargo nextest run -p maki-storage
```
