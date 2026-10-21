use std::{
    cell::RefCell,
    collections::{BTreeSet, HashMap},
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
};

use anyhow::{Context as _, Result};
use editor::Editor;
use gpui::{
    App, AppContext as _, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle,
    Focusable, Task, WeakEntity, Window, actions,
};
use git_ui_core::file_diff_view::{FileDiffView, build_buffer_diff};
use language::{Buffer, BufferEditSource, BufferEvent};
use picker::{Picker, PickerDelegate};
use project::{Project, buffer_store::BufferStoreEvent};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

use crate::vercel_status::{format_age, now_ms};

actions!(
    file_history,
    [
        /// Lists earlier saved versions of the current file, including saves made by agents.
        ShowFileHistory,
        /// Lists recent agent sessions and puts every file an agent changed back the way it was.
        RewindAgentChanges,
    ]
);

/// Larger files are skipped; they are rarely hand-edited and would dominate the history's disk use.
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRIES_PER_FILE: usize = 100;
const MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Saves closer together than this replace each other (autosave and format-on-save bursts).
const COALESCE_MS: u64 = 10_000;
/// Agent saves with gaps shorter than this belong to the same agent session.
const AGENT_SESSION_GAP_MS: u64 = 10 * 60 * 1000;
/// The journal is compacted to its newest lines once it grows past the limit.
const JOURNAL_MAX_LINES: usize = 5_000;
const JOURNAL_KEEP_LINES: usize = 2_000;

/// Snapshot writes must not interleave, or two saves could both rewrite `meta.json` or the journal.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(track_project).detach();
    cx.observe_new(register_editor).detach();
    cx.observe_new(register_workspace).detach();
}

fn history_root() -> PathBuf {
    paths::data_dir().join("file_history")
}

// ---------------------------------------------------------------------------------------------
// Recording

/// Who produced a recorded version of a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Author {
    #[default]
    You,
    Agent,
    /// Written to disk by another program (a terminal agent, git, a formatter) and reloaded.
    Outside,
}

struct BufferTrack {
    saved_version: clock::Global,
    /// Whether any edit since the last save came from an agent.
    agent_edited: bool,
}

type TrackedBuffers = Rc<RefCell<HashMap<EntityId, BufferTrack>>>;

fn track_project(project: &mut Project, _window: Option<&mut Window>, cx: &mut Context<Project>) {
    if !project.is_local() {
        return;
    }
    let tracked = TrackedBuffers::default();
    let buffer_store = project.buffer_store().clone();
    let existing: Vec<_> = buffer_store.read(cx).buffers().collect();
    for buffer in &existing {
        track_buffer(buffer, &tracked, cx);
    }
    cx.subscribe(&buffer_store, move |_, _, event, cx| {
        if let BufferStoreEvent::BufferAdded(buffer) = event {
            track_buffer(buffer, &tracked, cx);
        }
    })
    .detach();
}

fn track_buffer(buffer: &Entity<Buffer>, tracked: &TrackedBuffers, cx: &mut Context<Project>) {
    tracked.borrow_mut().insert(
        buffer.entity_id(),
        BufferTrack {
            saved_version: buffer.read(cx).saved_version().clone(),
            agent_edited: false,
        },
    );
    let tracked = tracked.clone();
    cx.subscribe(buffer, move |_, buffer, event, cx| match event {
        BufferEvent::Edited {
            source: BufferEditSource::Agent,
        } => {
            if let Some(track) = tracked.borrow_mut().get_mut(&buffer.entity_id()) {
                track.agent_edited = true;
            }
        }
        BufferEvent::Saved => {
            let author = match tracked.borrow().get(&buffer.entity_id()) {
                Some(track) if track.agent_edited => Author::Agent,
                _ => Author::You,
            };
            record_version(&buffer, &tracked, author, cx);
        }
        BufferEvent::Reloaded => record_version(&buffer, &tracked, Author::Outside, cx),
        _ => {}
    })
    .detach();
}

/// Records the buffer's current saved version, with the previous saved version as a baseline
/// when the history doesn't already end with it.
fn record_version(
    buffer: &Entity<Buffer>,
    tracked: &TrackedBuffers,
    author: Author,
    cx: &mut Context<Project>,
) {
    let buffer_ref = buffer.read(cx);
    let saved_version = buffer_ref.saved_version().clone();
    let previous_version = tracked
        .borrow_mut()
        .insert(
            buffer.entity_id(),
            BufferTrack {
                saved_version: saved_version.clone(),
                agent_edited: false,
            },
        )
        .map(|track| track.saved_version);

    let Some(file) = buffer_ref.file() else {
        return;
    };
    if file.is_private() {
        return;
    }
    let Some(local_file) = file.as_local() else {
        return;
    };
    let abs_path = local_file.abs_path(cx);
    let snapshot = buffer_ref.text_snapshot();
    // The content as written, even if the user kept typing while the save was in flight.
    let current = snapshot.rope_for_version(&saved_version);
    if current.len() > MAX_FILE_BYTES {
        return;
    }
    let previous = previous_version.map(|version| snapshot.rope_for_version(&version).to_string());
    let current = current.to_string();
    let root = history_root();
    cx.background_spawn(async move {
        record(&root, &abs_path, previous.as_deref(), &current, now_ms(), author)
            .with_context(|| format!("recording file history for {}", abs_path.display()))
    })
    .detach_and_log_err(cx);
}

// ---------------------------------------------------------------------------------------------
// Storage: <root>/<sha256(path)[..16]>/meta.json plus one zstd blob per entry, and
// <root>/journal.jsonl listing every recorded version across files (for agent sessions).

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    /// The file as it was before the first recorded save, or after it changed outside Bloo.
    Baseline,
    Save,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    /// Unix time in milliseconds; also the blob's file name.
    id: u64,
    hash: String,
    bytes: usize,
    kind: EntryKind,
    #[serde(default)]
    author: Author,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    path: PathBuf,
    entries: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct JournalLine {
    id: u64,
    path: PathBuf,
    author: Author,
}

fn file_dir(root: &Path, abs_path: &Path) -> PathBuf {
    let digest = Sha256::digest(abs_path.to_string_lossy().as_bytes());
    root.join(&hex::encode(digest)[..16])
}

fn content_hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

fn load_meta(dir: &Path) -> Result<Meta> {
    match fs::read(dir.join("meta.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("parsing meta.json"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Meta::default()),
        Err(error) => Err(error).context("reading meta.json"),
    }
}

fn write_meta(dir: &Path, meta: &Meta) -> Result<()> {
    let temp_path = dir.join("meta.json.tmp");
    fs::write(&temp_path, serde_json::to_vec_pretty(meta)?)?;
    fs::rename(&temp_path, dir.join("meta.json"))?;
    Ok(())
}

fn blob_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id}.zst"))
}

fn write_blob(dir: &Path, id: u64, text: &str) -> Result<()> {
    let compressed = zstd::encode_all(text.as_bytes(), 3)?;
    fs::write(blob_path(dir, id), compressed)?;
    Ok(())
}

fn remove_blob(dir: &Path, id: u64) {
    fs::remove_file(blob_path(dir, id)).log_err();
}

fn journal_path(root: &Path) -> PathBuf {
    root.join("journal.jsonl")
}

fn append_journal(root: &Path, line: &JournalLine) -> Result<()> {
    let path = journal_path(root);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{}", serde_json::to_string(line)?)?;
    drop(file);

    let contents = fs::read_to_string(&path)?;
    let line_count = contents.lines().count();
    if line_count > JOURNAL_MAX_LINES {
        let kept: Vec<&str> = contents
            .lines()
            .skip(line_count - JOURNAL_KEEP_LINES)
            .collect();
        let temp_path = root.join("journal.jsonl.tmp");
        fs::write(&temp_path, kept.join("\n") + "\n")?;
        fs::rename(&temp_path, &path)?;
    }
    Ok(())
}

fn read_journal(root: &Path) -> Result<Vec<JournalLine>> {
    match fs::read_to_string(journal_path(root)) {
        Ok(contents) => Ok(contents
            .lines()
            .filter_map(|line| serde_json::from_str(line).log_err())
            .collect()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error).context("reading journal.jsonl"),
    }
}

fn record(
    root: &Path,
    abs_path: &Path,
    previous: Option<&str>,
    current: &str,
    now_ms: u64,
    author: Author,
) -> Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("file history lock poisoned"))?;
    let dir = file_dir(root, abs_path);
    fs::create_dir_all(&dir)?;
    let mut meta = load_meta(&dir)?;
    meta.path = abs_path.to_path_buf();

    let next_id = |meta: &Meta| {
        meta.entries
            .last()
            .map_or(now_ms, |last| now_ms.max(last.id + 1))
    };

    if let Some(previous) = previous {
        let previous_hash = content_hash(previous);
        let diverged = meta
            .entries
            .last()
            .is_none_or(|last| last.hash != previous_hash);
        if diverged && previous != current {
            let id = next_id(&meta);
            write_blob(&dir, id, previous)?;
            meta.entries.push(Entry {
                id,
                hash: previous_hash,
                bytes: previous.len(),
                kind: EntryKind::Baseline,
                author: Author::You,
            });
        }
    }

    let current_hash = content_hash(current);
    if meta
        .entries
        .last()
        .is_some_and(|last| last.hash == current_hash)
    {
        return write_meta(&dir, &meta);
    }

    // Only coalesce saves by the same author, so an agent's save never hides one of yours.
    if let Some(last) = meta.entries.last()
        && last.kind == EntryKind::Save
        && last.author == author
        && now_ms.saturating_sub(last.id) < COALESCE_MS
    {
        let replaced = last.id;
        meta.entries.pop();
        remove_blob(&dir, replaced);
    }

    let id = next_id(&meta);
    write_blob(&dir, id, current)?;
    meta.entries.push(Entry {
        id,
        hash: current_hash,
        bytes: current.len(),
        kind: EntryKind::Save,
        author,
    });

    let oldest_allowed = now_ms.saturating_sub(MAX_AGE_MS);
    let excess = meta.entries.len().saturating_sub(MAX_ENTRIES_PER_FILE);
    let mut index = 0;
    meta.entries.retain(|entry| {
        let keep = index >= excess && entry.id >= oldest_allowed;
        index += 1;
        if !keep {
            remove_blob(&dir, entry.id);
        }
        keep
    });

    write_meta(&dir, &meta)?;
    append_journal(
        root,
        &JournalLine {
            id,
            path: abs_path.to_path_buf(),
            author,
        },
    )
}

fn list_entries(root: &Path, abs_path: &Path) -> Result<Vec<Entry>> {
    let mut entries = load_meta(&file_dir(root, abs_path))?.entries;
    entries.reverse();
    Ok(entries)
}

fn load_entry(root: &Path, abs_path: &Path, id: u64) -> Result<String> {
    let compressed = fs::read(blob_path(&file_dir(root, abs_path), id))?;
    let bytes = zstd::decode_all(compressed.as_slice())?;
    String::from_utf8(bytes).context("snapshot is not valid UTF-8")
}

/// A run of agent saves with no long pause between them.
#[derive(Clone, Debug, PartialEq)]
struct AgentSession {
    /// Id of the session's first agent save; files are restored to the version before it.
    start: u64,
    end: u64,
    paths: Vec<PathBuf>,
}

/// Groups the journal's agent saves into sessions, newest first.
fn agent_sessions(journal: &[JournalLine]) -> Vec<AgentSession> {
    let mut agent_lines: Vec<&JournalLine> = journal
        .iter()
        .filter(|line| line.author == Author::Agent)
        .collect();
    agent_lines.sort_by_key(|line| line.id);

    let mut sessions: Vec<(u64, u64, BTreeSet<PathBuf>)> = Vec::new();
    for line in agent_lines {
        match sessions.last_mut() {
            Some((_, end, paths)) if line.id.saturating_sub(*end) <= AGENT_SESSION_GAP_MS => {
                *end = line.id;
                paths.insert(line.path.clone());
            }
            _ => sessions.push((line.id, line.id, BTreeSet::from([line.path.clone()]))),
        }
    }
    sessions
        .into_iter()
        .rev()
        .map(|(start, end, paths)| AgentSession {
            start,
            end,
            paths: paths.into_iter().collect(),
        })
        .collect()
}

/// The file's content just before `before_id`, or `None` if nothing older was recorded
/// (for example, the agent created the file).
fn content_before(root: &Path, abs_path: &Path, before_id: u64) -> Result<Option<String>> {
    let entries = load_meta(&file_dir(root, abs_path))?.entries;
    entries
        .iter()
        .rev()
        .find(|entry| entry.id < before_id)
        .map(|entry| load_entry(root, abs_path, entry.id))
        .transpose()
}

// ---------------------------------------------------------------------------------------------
// UI: per-file history picker. Enter opens a diff against the file; secondary confirm restores.

fn register_editor(editor: &mut Editor, _window: Option<&mut Window>, cx: &mut Context<Editor>) {
    if !editor.mode().is_full() {
        return;
    }
    let editor_handle = cx.weak_entity();
    editor
        .register_action(move |_: &ShowFileHistory, window, cx| {
            show_file_history(&editor_handle, window, cx);
        })
        .detach();
}

fn show_file_history(editor: &WeakEntity<Editor>, window: &mut Window, cx: &mut App) {
    let Some((workspace, buffer)) = editor
        .update(cx, |editor, cx| {
            Some((editor.workspace()?, editor.active_buffer(cx)?))
        })
        .ok()
        .flatten()
    else {
        return;
    };
    let Some(abs_path) = buffer
        .read(cx)
        .file()
        .and_then(|file| Some(file.as_local()?.abs_path(cx)))
    else {
        return;
    };
    let root = history_root();
    let entries_task = cx.background_spawn({
        let abs_path = abs_path.clone();
        async move { list_entries(&root, &abs_path) }
    });
    let workspace_handle = workspace.downgrade();
    window
        .spawn(cx, async move |cx| {
            let entries = entries_task.await?;
            workspace_handle.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.weak_entity();
                workspace.toggle_modal(window, cx, move |window, cx| {
                    FileHistoryModal::new(workspace_handle, buffer, abs_path, entries, window, cx)
                });
            })
        })
        .detach_and_log_err(cx);
}

pub struct FileHistoryModal {
    picker: Entity<Picker<FileHistoryDelegate>>,
}

impl FileHistoryModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        buffer: Entity<Buffer>,
        abs_path: PathBuf,
        entries: Vec<Entry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = FileHistoryDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            buffer,
            abs_path,
            entries,
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::nonsearchable_uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for FileHistoryModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(30.)).child(self.picker.clone())
    }
}

impl Focusable for FileHistoryModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for FileHistoryModal {}
impl ModalView for FileHistoryModal {}

struct FileHistoryDelegate {
    modal: WeakEntity<FileHistoryModal>,
    workspace: WeakEntity<Workspace>,
    buffer: Entity<Buffer>,
    abs_path: PathBuf,
    entries: Vec<Entry>,
    selected_index: usize,
}

impl FileHistoryDelegate {
    fn open_diff(&self, entry: Entry, window: &mut Window, cx: &mut App) {
        let root = history_root();
        let abs_path = self.abs_path.clone();
        let buffer = self.buffer.clone();
        let workspace = self.workspace.clone();
        let content = cx.background_spawn(async move { load_entry(&root, &abs_path, entry.id) });
        window
            .spawn(cx, async move |cx| {
                let content = content.await?;
                let old_buffer = cx.new(|cx| {
                    let mut old_buffer = Buffer::local(content, cx);
                    let language = buffer.read(cx).language().cloned();
                    if let Some(registry) = buffer.read(cx).language_registry() {
                        old_buffer.set_language_registry(registry);
                    }
                    old_buffer.set_language(language, cx);
                    old_buffer
                });
                let diff = build_buffer_diff(&old_buffer, &buffer, cx).await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    let project = workspace.project().clone();
                    let workspace_entity = cx.entity();
                    let diff_view = cx.new(|cx| {
                        FileDiffView::new(
                            old_buffer,
                            buffer,
                            diff,
                            project,
                            workspace_entity,
                            window,
                            cx,
                        )
                    });
                    workspace.active_pane().update(cx, |pane, cx| {
                        pane.add_item(Box::new(diff_view), true, true, None, window, cx);
                    });
                })
            })
            .detach_and_log_err(cx);
    }

    fn restore(&self, entry: Entry, window: &mut Window, cx: &mut App) {
        let root = history_root();
        let abs_path = self.abs_path.clone();
        let buffer = self.buffer.clone();
        let workspace = self.workspace.clone();
        let content = cx.background_spawn(async move { load_entry(&root, &abs_path, entry.id) });
        window
            .spawn(cx, async move |cx| {
                let content = content.await?;
                // One undoable edit; saving it records a new snapshot, so restores can be undone too.
                buffer.update(cx, |buffer, cx| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content)], None, cx);
                });
                let project = workspace.update(cx, |workspace, _| workspace.project().clone())?;
                project
                    .update(cx, |project, cx| project.save_buffer(buffer, cx))
                    .await
            })
            .detach_and_log_err(cx);
    }
}

fn entry_label(entry: &Entry) -> &'static str {
    match (entry.kind, entry.author) {
        (EntryKind::Baseline, _) => "Before edits",
        (EntryKind::Save, Author::You) => "Saved by you",
        (EntryKind::Save, Author::Agent) => "Saved by agent",
        (EntryKind::Save, Author::Outside) => "Changed outside Bloo",
    }
}

impl PickerDelegate for FileHistoryDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "file history"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Enter shows the diff, ⌘↵ restores the file to that version".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No saved versions yet. Bloo records one every time this file is saved.".into())
    }

    fn match_count(&self) -> usize {
        self.entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        _query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(entry) = self.entries.get(self.selected_index).cloned() {
            if secondary {
                self.restore(entry, window, cx);
            } else {
                self.open_diff(entry, window, cx);
            }
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let entry = self.entries.get(index)?;
        let author_color = match entry.author {
            Author::Agent if entry.kind == EntryKind::Save => Color::Accent,
            _ => Color::Muted,
        };
        let size = if entry.bytes < 1024 {
            format!("{} B", entry.bytes)
        } else {
            format!("{:.1} KB", entry.bytes as f64 / 1024.0)
        };
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(Label::new(format_age(entry.id, now_ms())))
                        .child(
                            Label::new(entry_label(entry))
                                .color(author_color)
                                .size(LabelSize::Small),
                        )
                        .child(div().flex_1())
                        .child(Label::new(size).color(Color::Muted).size(LabelSize::Small)),
                ),
        )
    }
}

// ---------------------------------------------------------------------------------------------
// UI: agent sessions picker. Enter puts every file the session changed back the way it was.

fn register_workspace(
    workspace: &mut Workspace,
    _window: Option<&mut Window>,
    _cx: &mut Context<Workspace>,
) {
    workspace.register_action(|workspace, _: &RewindAgentChanges, window, cx| {
        let root = history_root();
        let sessions = cx.background_spawn(async move { read_journal(&root) });
        let workspace_handle = cx.weak_entity();
        window
            .spawn(cx, async move |cx| {
                let sessions = agent_sessions(&sessions.await?);
                workspace_handle.update_in(cx, |workspace, window, cx| {
                    let workspace_handle = cx.weak_entity();
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        AgentSessionsModal::new(workspace_handle, sessions, window, cx)
                    });
                })
            })
            .detach_and_log_err(cx);
        let _ = workspace;
    });
}

pub struct AgentSessionsModal {
    picker: Entity<Picker<AgentSessionsDelegate>>,
}

impl AgentSessionsModal {
    fn new(
        workspace: WeakEntity<Workspace>,
        sessions: Vec<AgentSession>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = AgentSessionsDelegate {
            modal: cx.entity().downgrade(),
            workspace,
            sessions,
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::nonsearchable_uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for AgentSessionsModal {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(36.)).child(self.picker.clone())
    }
}

impl Focusable for AgentSessionsModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for AgentSessionsModal {}
impl ModalView for AgentSessionsModal {}

struct AgentSessionsDelegate {
    modal: WeakEntity<AgentSessionsModal>,
    workspace: WeakEntity<Workspace>,
    sessions: Vec<AgentSession>,
    selected_index: usize,
}

fn rewind_session(
    workspace: WeakEntity<Workspace>,
    session: AgentSession,
    window: &mut Window,
    cx: &mut App,
) {
    let root = history_root();
    let contents = cx.background_spawn(async move {
        session
            .paths
            .iter()
            .map(|path| Ok((path.clone(), content_before(&root, path, session.start)?)))
            .collect::<Result<Vec<_>>>()
    });
    window
        .spawn(cx, async move |cx| {
            let contents = contents.await?;
            let project = workspace.update(cx, |workspace, _| workspace.project().clone())?;
            let mut restored = 0;
            let mut left_in_place = 0;
            for (path, content) in contents {
                let Some(content) = content else {
                    left_in_place += 1;
                    continue;
                };
                let buffer = project
                    .update(cx, |project, cx| project.open_local_buffer(&path, cx))
                    .await?;
                buffer.update(cx, |buffer, cx| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content)], None, cx);
                });
                project
                    .update(cx, |project, cx| project.save_buffer(buffer, cx))
                    .await?;
                restored += 1;
            }
            let mut message = format!(
                "Rewound {restored} {} to before the agent's changes. Each file's history can undo this.",
                if restored == 1 { "file" } else { "files" }
            );
            if left_in_place > 0 {
                message.push_str(&format!(
                    " {left_in_place} new {} left in place.",
                    if left_in_place == 1 { "file was" } else { "files were" }
                ));
            }
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<RewindAgentChanges>(), message),
                    cx,
                );
            })
        })
        .detach_and_log_err(cx);
}

impl PickerDelegate for AgentSessionsDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "agent sessions"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Enter puts every file this agent session changed back the way it was".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No agent changes recorded yet. Bloo records them whenever an agent saves a file.".into())
    }

    fn match_count(&self) -> usize {
        self.sessions.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, index: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = index;
    }

    fn update_matches(
        &mut self,
        _query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(session) = self.sessions.get(self.selected_index).cloned() {
            rewind_session(self.workspace.clone(), session, window, cx);
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.modal.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        index: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let session = self.sessions.get(index)?;
        let count = session.paths.len();
        let names = session
            .paths
            .iter()
            .take(3)
            .filter_map(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(", ");
        let names = if count > 3 {
            format!("{names} +{}", count - 3)
        } else {
            names
        };
        Some(
            ListItem::new(index)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .w_full()
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Label::new(format!(
                                    "Agent changed {count} {}",
                                    if count == 1 { "file" } else { "files" }
                                )))
                                .child(div().flex_1())
                                .child(
                                    Label::new(format_age(session.end, now_ms()))
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                        )
                        .child(Label::new(names).color(Color::Muted).size(LabelSize::Small)),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(root: &Path, path: &Path) -> Vec<(EntryKind, Author, String)> {
        list_entries(root, path)
            .unwrap()
            .into_iter()
            .rev()
            .map(|entry| {
                (
                    entry.kind,
                    entry.author,
                    load_entry(root, path, entry.id).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn records_baseline_saves_and_coalesces() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let path = Path::new("/project/src/main.rs");

        // First save: the pre-edit content becomes a baseline.
        record(root, path, Some("v0"), "v1", 1_000_000, Author::You).unwrap();
        assert_eq!(
            kinds(root, path),
            vec![
                (EntryKind::Baseline, Author::You, "v0".to_string()),
                (EntryKind::Save, Author::You, "v1".to_string())
            ]
        );

        // A save within the coalescing window replaces the previous save by the same author.
        record(root, path, Some("v1"), "v2", 1_005_000, Author::You).unwrap();
        assert_eq!(
            kinds(root, path).last().unwrap(),
            &(EntryKind::Save, Author::You, "v2".to_string())
        );
        assert_eq!(list_entries(root, path).unwrap().len(), 2);

        // An agent save right after never replaces the user's save.
        record(root, path, Some("v2"), "agent", 1_006_000, Author::Agent).unwrap();
        assert_eq!(list_entries(root, path).unwrap().len(), 3);

        // Saving identical content adds nothing.
        record(root, path, Some("agent"), "agent", 1_100_000, Author::You).unwrap();
        assert_eq!(list_entries(root, path).unwrap().len(), 3);

        // The file changed outside Bloo: the external content is kept as a baseline.
        record(root, path, Some("external"), "v3", 1_200_000, Author::You).unwrap();
        let recorded = kinds(root, path);
        assert_eq!(recorded.len(), 5);
        assert_eq!(
            recorded[3],
            (EntryKind::Baseline, Author::You, "external".to_string())
        );
        assert_eq!(recorded[4], (EntryKind::Save, Author::You, "v3".to_string()));
    }

    #[test]
    fn prunes_old_and_excess_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let path = Path::new("/project/notes.md");

        for index in 0..(MAX_ENTRIES_PER_FILE as u64 + 5) {
            let now = 1_000_000 + index * (COALESCE_MS + 1);
            record(root, path, None, &format!("version {index}"), now, Author::You).unwrap();
        }
        let entries = list_entries(root, path).unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES_PER_FILE);
        assert_eq!(
            load_entry(root, path, entries[0].id).unwrap(),
            format!("version {}", MAX_ENTRIES_PER_FILE + 4)
        );

        let far_future = entries[0].id + MAX_AGE_MS + 1;
        record(root, path, None, "fresh", far_future, Author::You).unwrap();
        let entries = list_entries(root, path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(load_entry(root, path, entries[0].id).unwrap(), "fresh");
    }

    #[test]
    fn rewinds_an_agent_session_across_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let app = Path::new("/project/app.tsx");
        let styles = Path::new("/project/styles.css");
        let created = Path::new("/project/new.ts");

        // The user's own work, then an agent session touching three files.
        record(root, app, None, "app v1", 1_000_000, Author::You).unwrap();
        record(root, styles, None, "styles v1", 1_000_000, Author::You).unwrap();
        let agent_start = 2_000_000;
        record(root, app, Some("app v1"), "app agent", agent_start, Author::Agent).unwrap();
        record(root, styles, Some("styles v1"), "styles agent", agent_start + 60_000, Author::Agent)
            .unwrap();
        record(root, created, None, "brand new", agent_start + 120_000, Author::Agent).unwrap();
        // A later, separate agent session.
        record(root, app, Some("app agent"), "app agent 2", agent_start + 3_600_000, Author::Agent)
            .unwrap();

        let sessions = agent_sessions(&read_journal(root).unwrap());
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].paths, vec![app.to_path_buf()]);
        let first = &sessions[1];
        assert_eq!(first.start, agent_start);
        assert_eq!(first.paths.len(), 3);

        assert_eq!(
            content_before(root, app, first.start).unwrap().as_deref(),
            Some("app v1")
        );
        assert_eq!(
            content_before(root, styles, first.start).unwrap().as_deref(),
            Some("styles v1")
        );
        assert_eq!(content_before(root, created, first.start).unwrap(), None);
    }
}
