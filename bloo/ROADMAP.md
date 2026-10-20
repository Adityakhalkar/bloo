# Bloo roadmap and designs

Bloo is a thin fork of Zed. Every change is one commit on `mine`, rebased on each upstream release.
New features live in new crates; upstream files get only registration lines (root `Cargo.toml`
members/deps, `crates/zed/Cargo.toml`, one `init(cx)` in `crates/zed/src/main.rs`, the
`test_action_namespaces` list in `crates/zed/src/zed.rs`, and `keymaps/bloo.json` loaded from
`zed::load_default_keymap`). Key bindings set from code get wiped by `cx.clear_key_bindings()`
on keymap reload, so bindings go in `assets/keymaps/bloo.json`.

Designs below came from read-only research at upstream `2c626b3397` (2026-10-20). Line numbers drift.

## Done
- Rebrand: bundle names/ids `dev.bloo.Bloo*`, `ReleaseChannel::display_name`/`app_id`, app icons.
- Auto-update off (`ReleaseChannel::poll_for_updates` → false).
- Notebook editor on for everyone (`NotebookFeatureFlag::enabled_for_all`).
- Feature-flag overrides honored in release builds (`FeatureFlagStore::overrides_enabled`), and the
  Settings UI shows the flags page. `rename-tool` on for everyone (staff-dogfooded upstream).
  `lsp-tool` and `diff-review` are off even for Zed staff, so they stay opt-in.
- `crates/bloo_extras` (registered by one `bloo_extras::init(cx)` in `main.rs`):
  - `vercel_status`: ▲ deploy state for the current branch in the status bar (design §1).
  - `peek_definition`: ⌥F12 inline peek; ⌘↵ open, F4/⇧F4 cycle, Esc close (design §6).
  - `file_history`: snapshot on every save incl. agent saves; `file_history: show file history`
    picker, Enter = diff, ⌘↵ = restore (design §5). No global size cap yet; per-file limits only.
- `assets/keymaps/bloo.json`, loaded last in `zed::load_default_keymap`.
- Motion (Apple-style; all off when Zed's `reduce_motion` setting or macOS Reduce motion is on):
  - Tokens in `crates/ui/src/styles/motion.rs`: `apple_spring(response, damping_ratio)`,
    `reposition_spring()` (0.32 s, no overshoot), 180 ms ease-out-quint entrances drifting 8 px.
  - Smooth navigation jumps: `editor/src/scroll.rs` `ScrollJump` springs the *painted* scroll top
    toward autoscroll targets ≥ 3 rows (only the last 40 rows of long jumps animate). Trackpad and
    wheel input cancel it and continue from the presented position. Disabled under `test-support`.
    Paint hook: one block in `editor/src/element.rs` after `snapshot.scroll_position()`.
  - Entrances: modals (`workspace/src/modal_layer.rs`), dock panel content (`dock.rs`),
    notifications (`workspace.rs` `render_notifications`), peek definition, Vercel status changes.
  - Cursor glide on by default (`assets/settings/default.json` `cursor_animation.enabled`).
  - macOS Reduce motion is read at startup and on settings changes (`zed.rs` `init_reduce_motion`).
    Toggling it in System Settings while Bloo runs needs a settings change or restart to apply.
  - Not done: exit animations (GPUI drops the element immediately), page up/down easing.
- Not yet verified in a running app: everything in `bloo_extras` (needs the full Xcode build).

## 1. Vercel deploy status (status bar) — crate `bloo_extras::vercel_status`, ~250 LOC
- `workspace::StatusItemView` (`workspace/src/status_bar.rs`); add via `observe_new::<Workspace>` →
  `status_bar().update(|sb| sb.add_left_item(..))`. Hide when worktree root has no `.vercel/project.json`.
- Branch: `project.active_repository(cx)` → `repo.read(cx).branch`. Refresh on
  `GitStoreEvent::ActiveRepositoryChanged` / `RepositoryEvent::HeadChanged` (see `title_bar.rs`).
- Data: `vercel api "/v6/deployments?projectId=..&teamId=..&limit=20"` via `util::command::new_command`,
  filter on `meta.githubCommitRef == branch`. Poll 15 s while BUILDING/QUEUED, 60 s otherwise.
- Click: open `https://{url}` when READY, else `inspectorUrl`. On auth error tell user to run `vercel login`.

## 2. Type hierarchy — ~120 LOC project side + UI
- Mirror `crates/call_hierarchy` (`CallHierarchyMode`, `fetch_calls`, `fetch_root`).
- lsp-types fork has `TypeHierarchyPrepare/Supertypes/Subtypes`; `ServerCapabilities` lacks the field,
  so send the request and treat MethodNotFound/empty as unsupported.
- New `project/src/lsp_store/type_hierarchy.rs` (needs `open_local_buffer_via_lsp`, pub(crate)).
- Useful for clangd, jdtls, gopls, sourcekit; not rust-analyzer or TS servers.

## 3. Rename preview — ~300 LOC
- `Editor::rename` → `confirm_rename` → `Project::perform_rename` applies edits AND file ops immediately,
  so preview must compute the `WorkspaceEdit` with a direct `lsp::request::Rename`, apply to
  `buffer.branch(cx)` copies, diff with `BufferDiff`, show in a multibuffer item with Apply/Cancel.
  Apply re-runs `project.perform_rename` + `open_project_transaction`.
- Trigger: `ConfirmRenameWithPreview` on `cmd-enter` in `"Editor && renaming"`.

## 4. Tests panel + coverage gutter — ~450 + ~250 LOC
- Runnables: `snapshot.runnable_ranges`, `inventory.list_tasks` filtered by tags,
  `task_store.task_context_for_location`, `workspace.schedule_resolved_task_with_completion`
  (returns pass/fail). Panel via `workspace.add_panel` from `observe_new::<Workspace>`.
- Coverage: parse `lcov.info` / `coverage/coverage-final.json`; paint with
  `Editor::highlight_gutter::<T>` or `highlight_rows::<T>`; watch with `fs.watch`.

## 5. Local file history — crate `file_history`, ~650 LOC
- Hook `language::BufferEvent::Saved` (covers editor saves, format-on-save, agent `edit_session` saves).
  Wire with `observe_new::<Project>` → `buffer_store` `BufferAdded` → subscribe per buffer.
- Exact content: `snapshot.rope_for_version(buffer.saved_version())`; write a "baseline" entry from the
  previous saved version when history is empty or diverged.
- Skip `file.is_private()`, >2 MiB, non-local. Store under `paths::data_dir()/file_history/<sha2(path)>/`
  as zstd blobs + `meta.json`. Dedupe by hash, coalesce saves <10 s apart, keep 100/file, 30 days, 256 MiB.
- UI: picker modal (copy `line_ending_selector` registration); diff via
  `git_ui_core::file_diff_view::{FileDiffView, build_buffer_diff}` with an in-memory `Buffer::local`.

## 6. Peek definition — crate `peek_definition`, ~500 LOC
- Template: `crates/lsp_locations` (registers on every full editor). Fetch with
  `Editor::definition_locations_of_kind(GotoDefinitionKind::Symbol, cx)`.
- Block: `insert_blocks` with `BlockPlacement::Below`, `BlockStyle::Sticky`, render an inner
  `Editor::for_multibuffer` like `agent_ui/src/inline_assistant.rs`. Configure like `picker_preview`.
- Escape closes (register `editor::actions::Cancel`, else `cx.propagate()`); `cmd-enter` opens target
  via `navigate_to_hover_links`. alt-f12 is taken by `GoToDefinitionSplit`.

## 7. Browser pane + element picker — crate `browser`, ~2k LOC incl. JS
- No GPUI native-view API. Spike: get the GPUIView via `HasWindowHandle::window_handle(window)` →
  `AppKit.ns_view`, `addSubview` a WKWebView, position from a `canvas` paint closure (flip y; GPUIView
  is not flipped). Hide/show from Item hooks (cached panes skip paint).
- Deps: `objc2-web-kit 0.3.2` (matches objc2 0.3 family) or `wry` `build_as_child`.
- Known issues: native view draws above all GPUI overlays (menus, palette, drag previews) → hide and show
  a snapshot while overlays are active; focus must be handed back with `makeFirstResponder(gpuiView)`;
  forward copy/paste actions to the webview; ~100–200 MB per WebContent process.
- Proper fix later: `Window::paint_native_view` frame list in GPUI (~300 LOC).
- Picker: injected user script; React 19 removed `_debugSource`, so resolve `_debugStack` via source
  maps (react-grab/bippy approach). Send to agent: terminal threads get `@file:line text`
  (`format_selection_for_terminal`), native threads get a buffer mention (`MessageEditor::insert_selections`).

## Feature flags worth knowing
Toggle any of these from settings: `"feature_flags": {"<name>": "on"}`.
`lsp-tool` (agent FindReferences / GoToDefinition / code actions), `diff-review` (review comments in
diffs sent to the agent), `acp-beta` (ACP token usage), `create-thread-tool` (agent spawns sibling
threads), `agent-thread-worktree-label`, `debugger-history`.
