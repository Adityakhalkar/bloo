use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
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
use language::{Buffer, BufferEvent};
use picker::{Picker, PickerDelegate};
use project::{Project, buffer_store::BufferStoreEvent};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::vercel_status::{format_age, now_ms};

actions!(
    file_history,
    [
        /// Lists earlier saved versions of the current file, including saves made by agents.
        ShowFileHistory
    ]
);

/// Larger files are skipped; they are rarely hand-edited and would dominate the history's disk use.
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_ENTRIES_PER_FILE: usize = 100;
const MAX_AGE_MS: u64 = 30 * 24 * 60 * 60 * 1000;
/// Saves closer together than this replace each other (autosave and format-on-save bursts).
const COALESCE_MS: u64 = 10_000;

/// Snapshot writes for one file must not interleave, or two saves could both rewrite `meta.json`.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(track_project).detach();
    cx.observe_new(register_editor).detach();
}

fn history_root() -> PathBuf {
    paths::data_dir().join("file_history")
}

// ---------------------------------------------------------------------------------------------
// Recording

type SavedVersions = Rc<RefCell<HashMap<EntityId, clock::Global>>>;

fn track_project(project: &mut Project, _window: Option<&mut Window>, cx: &mut Context<Project>) {
    if !project.is_local() {
        return;
    }
    let saved_versions = SavedVersions::default();
    let buffer_store = project.buffer_store().clone();
    let existing: Vec<_> = buffer_store.read(cx).buffers().collect();
    for buffer in &existing {
        track_buffer(buffer, &saved_versions, cx);
    }
    cx.subscribe(&buffer_store, move |_, _, event, cx| {
        if let BufferStoreEvent::BufferAdded(buffer) = event {
            track_buffer(buffer, &saved_versions, cx);
        }
    })
    .detach();
}

fn track_buffer(buffer: &Entity<Buffer>, saved_versions: &SavedVersions, cx: &mut Context<Project>) {
    saved_versions
        .borrow_mut()
        .insert(buffer.entity_id(), buffer.read(cx).saved_version().clone());
    let saved_versions = saved_versions.clone();
    cx.subscribe(buffer, move |_, buffer, event, cx| match event {
        BufferEvent::Saved => record_save(&buffer, &saved_versions, cx),
        BufferEvent::Reloaded => {
            saved_versions
                .borrow_mut()
                .insert(buffer.entity_id(), buffer.read(cx).saved_version().clone());
        }
        _ => {}
    })
    .detach();
}

fn record_save(buffer: &Entity<Buffer>, saved_versions: &SavedVersions, cx: &mut Context<Project>) {
    let buffer_ref = buffer.read(cx);
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
    let saved_version = buffer_ref.saved_version().clone();
    // The content as written, even if the user kept typing while the save was in flight.
    let current = snapshot.rope_for_version(&saved_version);
    if current.len() > MAX_FILE_BYTES {
        return;
    }
    let previous = saved_versions
        .borrow_mut()
        .insert(buffer.entity_id(), saved_version)
        .map(|version| snapshot.rope_for_version(&version).to_string());
    let current = current.to_string();
    let root = history_root();
    cx.background_spawn(async move {
        record(&root, &abs_path, previous.as_deref(), &current, now_ms())
            .with_context(|| format!("recording file history for {}", abs_path.display()))
    })
    .detach_and_log_err(cx);
}

// ---------------------------------------------------------------------------------------------
// Storage: <root>/<sha256(path)[..16]>/meta.json plus one zstd blob per entry.

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
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Meta {
    path: PathBuf,
    entries: Vec<Entry>,
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

fn record(
    root: &Path,
    abs_path: &Path,
    previous: Option<&str>,
    current: &str,
    now_ms: u64,
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

    if let Some(last) = meta.entries.last()
        && last.kind == EntryKind::Save
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

    write_meta(&dir, &meta)
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

// ---------------------------------------------------------------------------------------------
// UI: a picker of snapshots. Enter opens a diff against the file; secondary confirm restores.

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
        let kind = match entry.kind {
            EntryKind::Save => "Saved",
            EntryKind::Baseline => "Before edits",
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
                        .child(Label::new(kind).color(Color::Muted).size(LabelSize::Small))
                        .child(div().flex_1())
                        .child(Label::new(size).color(Color::Muted).size(LabelSize::Small)),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(root: &Path, path: &Path) -> Vec<(EntryKind, String)> {
        list_entries(root, path)
            .unwrap()
            .into_iter()
            .rev()
            .map(|entry| (entry.kind, load_entry(root, path, entry.id).unwrap()))
            .collect()
    }

    #[test]
    fn records_baseline_saves_and_coalesces() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let path = Path::new("/project/src/main.rs");

        // First save: the pre-edit content becomes a baseline.
        record(root, path, Some("v0"), "v1", 1_000_000).unwrap();
        assert_eq!(
            kinds(root, path),
            vec![
                (EntryKind::Baseline, "v0".to_string()),
                (EntryKind::Save, "v1".to_string())
            ]
        );

        // A save within the coalescing window replaces the previous save.
        record(root, path, Some("v1"), "v2", 1_005_000).unwrap();
        assert_eq!(
            kinds(root, path).last().unwrap(),
            &(EntryKind::Save, "v2".to_string())
        );
        assert_eq!(list_entries(root, path).unwrap().len(), 2);

        // Saving identical content adds nothing.
        record(root, path, Some("v2"), "v2", 1_100_000).unwrap();
        assert_eq!(list_entries(root, path).unwrap().len(), 2);

        // The file changed outside Bloo: the external content is kept as a baseline.
        record(root, path, Some("external"), "v3", 1_200_000).unwrap();
        let recorded = kinds(root, path);
        assert_eq!(recorded.len(), 4);
        assert_eq!(recorded[2], (EntryKind::Baseline, "external".to_string()));
        assert_eq!(recorded[3], (EntryKind::Save, "v3".to_string()));
    }

    #[test]
    fn prunes_old_and_excess_entries() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let path = Path::new("/project/notes.md");

        for index in 0..(MAX_ENTRIES_PER_FILE as u64 + 5) {
            let now = 1_000_000 + index * (COALESCE_MS + 1);
            record(root, path, None, &format!("version {index}"), now).unwrap();
        }
        let entries = list_entries(root, path).unwrap();
        assert_eq!(entries.len(), MAX_ENTRIES_PER_FILE);
        assert_eq!(
            load_entry(root, path, entries[0].id).unwrap(),
            format!("version {}", MAX_ENTRIES_PER_FILE + 4)
        );

        let far_future = entries[0].id + MAX_AGE_MS + 1;
        record(root, path, None, "fresh", far_future).unwrap();
        let entries = list_entries(root, path).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(load_entry(root, path, entries[0].id).unwrap(), "fresh");
    }
}
