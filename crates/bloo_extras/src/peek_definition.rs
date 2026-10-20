use std::{cell::RefCell, rc::Rc, sync::Arc};

use collections::HashSet;
use editor::{
    Editor, GotoDefinitionKind, MultiBuffer, RowHighlightOptions, SelectionEffects,
    actions::Cancel,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
    hover_links::HoverLink,
    scroll::Autoscroll,
};
use gpui::{
    AnimationExt as _, App, Context, ElementId, Entity, Focusable as _, Subscription, WeakEntity,
    Window, actions,
};
use language::{Capability, Location, ToPoint as _};
use project::{LocationLink, Project};
use ui::prelude::*;

actions!(
    peek_definition,
    [
        /// Shows the definition of the symbol under the cursor inline, without leaving the file.
        PeekDefinition,
        /// Opens the peeked definition in the editor.
        OpenPeekedDefinition,
        /// Shows the next definition when the symbol has several.
        NextPeekResult,
        /// Shows the previous definition when the symbol has several.
        PreviousPeekResult,
    ]
);

const PEEK_EDITOR_LINES: u32 = 12;
/// The editor plus the header row, rounded up to whole editor lines.
const PEEK_BLOCK_LINES: u32 = PEEK_EDITOR_LINES + 2;
const CONTEXT_LINES: u32 = 40;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(register).detach();
}

type PeekSlot = Rc<RefCell<Option<ActivePeek>>>;

struct ActivePeek {
    block_id: CustomBlockId,
}

fn register(editor: &mut Editor, _window: Option<&mut Window>, cx: &mut Context<Editor>) {
    if !editor.mode().is_full() {
        return;
    }
    let slot = PeekSlot::default();
    let handle = cx.entity().downgrade();
    editor
        .register_action({
            let slot = slot.clone();
            let handle = handle.clone();
            move |_: &PeekDefinition, window, cx| open_peek(&handle, &slot, window, cx)
        })
        .detach();
    editor
        .register_action(move |_: &Cancel, window, cx| {
            if !close_peek(&handle, &slot, window, cx) {
                cx.propagate();
            }
        })
        .detach();
}

fn open_peek(handle: &WeakEntity<Editor>, slot: &PeekSlot, window: &mut Window, cx: &mut App) {
    let Some(editor) = handle.upgrade() else {
        return;
    };
    let Some(definitions) = editor.update(cx, |editor, cx| {
        editor.definition_locations_of_kind(GotoDefinitionKind::Symbol, cx)
    }) else {
        return;
    };
    let handle = handle.clone();
    let slot = slot.clone();
    window
        .spawn(cx, async move |cx| {
            let locations = definitions.await?;
            if locations.is_empty() {
                return anyhow::Ok(());
            }
            cx.update(|window, cx| show_peek(handle, slot, locations, window, cx))?;
            Ok(())
        })
        .detach_and_log_err(cx);
}

fn show_peek(
    handle: WeakEntity<Editor>,
    slot: PeekSlot,
    locations: Vec<Location>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(editor) = handle.upgrade() else {
        return;
    };
    close_peek(&handle, &slot, window, cx);

    let project = editor.read(cx).project().cloned();
    let view = cx.new(|cx| PeekView::new(handle.clone(), slot.clone(), locations, project, window, cx));
    let block_id = editor.update(cx, |editor, cx| {
        let cursor = editor.selections.newest_anchor().head();
        let view = view.clone();
        editor
            .insert_blocks(
                [BlockProperties {
                    placement: BlockPlacement::Below(cursor),
                    height: Some(PEEK_BLOCK_LINES),
                    style: BlockStyle::Sticky,
                    render: Arc::new(move |cx| {
                        div()
                            .occlude()
                            .w_full()
                            .h(PEEK_BLOCK_LINES as f32 * cx.window.line_height())
                            .pl(cx.margins.gutter.full_width())
                            .child(view.clone())
                            .into_any_element()
                    }),
                    priority: 0,
                }],
                Some(Autoscroll::fit()),
                cx,
            )
            .into_iter()
            .next()
    });
    let Some(block_id) = block_id else {
        return;
    };
    *slot.borrow_mut() = Some(ActivePeek { block_id });
    let inner_focus = view.read(cx).editor.focus_handle(cx);
    inner_focus.focus(window, cx);
}

/// Removes the open peek, if any, and returns focus to the outer editor.
fn close_peek(
    handle: &WeakEntity<Editor>,
    slot: &PeekSlot,
    window: &mut Window,
    cx: &mut App,
) -> bool {
    let Some(peek) = slot.borrow_mut().take() else {
        return false;
    };
    if let Some(editor) = handle.upgrade() {
        editor.update(cx, |editor, cx| {
            editor.remove_blocks(HashSet::from_iter([peek.block_id]), None, cx);
        });
        let outer_focus = editor.focus_handle(cx);
        outer_focus.focus(window, cx);
    }
    true
}

struct PeekTargetHighlight;

struct PeekView {
    outer: WeakEntity<Editor>,
    slot: PeekSlot,
    locations: Vec<Location>,
    selected: usize,
    editor: Entity<Editor>,
    _actions: Vec<Subscription>,
}

impl PeekView {
    fn new(
        outer: WeakEntity<Editor>,
        slot: PeekSlot,
        locations: Vec<Location>,
        project: Option<Entity<Project>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            // Editable on purpose: edits go straight to the definition's buffer.
            let multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
            let mut editor = Editor::for_multibuffer(multi_buffer, project, window, cx);
            editor.disable_scrollbars_and_minimap(window, cx);
            editor.disable_expand_excerpt_buttons(cx);
            editor.disable_mouse_wheel_zoom();
            editor.set_show_git_diff_gutter(false, cx);
            editor.set_show_code_actions(false, cx);
            editor.set_show_runnables(false, cx);
            editor.set_show_breakpoints(false, cx);
            editor
        });

        let this = cx.entity().downgrade();
        // Listeners registered on an editor run before its own handlers, so these win over
        // the inner editor's default Escape/Enter behavior while focus is inside the peek.
        let actions = editor.update(cx, |inner, _| {
            vec![
                inner.register_action({
                    let outer = outer.clone();
                    let slot = slot.clone();
                    move |_: &Cancel, window, cx| {
                        close_peek(&outer, &slot, window, cx);
                    }
                }),
                inner.register_action({
                    let this = this.clone();
                    move |_: &OpenPeekedDefinition, window, cx| {
                        this.update(cx, |view, cx| view.open_selected(window, cx)).ok();
                    }
                }),
                inner.register_action({
                    let this = this.clone();
                    move |_: &NextPeekResult, window, cx| {
                        this.update(cx, |view, cx| view.cycle(1, window, cx)).ok();
                    }
                }),
                inner.register_action({
                    let this = this.clone();
                    move |_: &PreviousPeekResult, window, cx| {
                        this.update(cx, |view, cx| view.cycle(-1, window, cx)).ok();
                    }
                }),
            ]
        });

        let mut view = Self {
            outer,
            slot,
            locations,
            selected: 0,
            editor,
            _actions: actions,
        };
        view.show_location(0, window, cx);
        view
    }

    fn cycle(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let count = self.locations.len();
        if count < 2 {
            return;
        }
        let index = (self.selected as isize + step).rem_euclid(count as isize) as usize;
        self.show_location(index, window, cx);
    }

    fn show_location(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(location) = self.locations.get(index).cloned() else {
            return;
        };
        self.selected = index;
        let buffer_snapshot = location.buffer.read(cx).snapshot();
        let target = location.range.start.to_point(&buffer_snapshot)
            ..location.range.end.to_point(&buffer_snapshot);

        self.editor.update(cx, |editor, cx| {
            let multi_buffer = editor.buffer().clone();
            multi_buffer.update(cx, |multi_buffer, cx| {
                multi_buffer.clear(cx);
                multi_buffer.set_excerpts_for_buffer(
                    location.buffer.clone(),
                    [target],
                    CONTEXT_LINES,
                    cx,
                );
            });
            let snapshot = multi_buffer.read(cx).snapshot(cx);
            let Some((start, end)) = snapshot
                .anchor_in_excerpt(location.range.start)
                .zip(snapshot.anchor_in_excerpt(location.range.end))
            else {
                return;
            };
            editor.clear_row_highlights::<PeekTargetHighlight>();
            editor.highlight_rows::<PeekTargetHighlight>(
                start..end,
                |cx| cx.theme().colors().editor_active_line_background,
                RowHighlightOptions::default(),
                cx,
            );
            editor.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |selections| selections.select_anchor_ranges([start..start]),
            );
        });
        cx.notify();
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.locations.get(self.selected).cloned() else {
            return;
        };
        close_peek(&self.outer, &self.slot, window, cx);
        self.outer
            .update(cx, |editor, cx| {
                editor
                    .navigate_to_hover_links(
                        Some(GotoDefinitionKind::Symbol),
                        vec![HoverLink::Text(LocationLink {
                            origin: None,
                            target,
                        })],
                        None,
                        false,
                        window,
                        cx,
                    )
                    .detach_and_log_err(cx);
            })
            .ok();
    }

    fn header_text(&self, cx: &App) -> SharedString {
        let Some(location) = self.locations.get(self.selected) else {
            return SharedString::default();
        };
        let buffer = location.buffer.read(cx);
        let line = location.range.start.to_point(&buffer.snapshot()).row + 1;
        let path = buffer
            .file()
            .map(|file| file.full_path(cx).to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".to_string());
        format!("{path}:{line}").into()
    }
}

impl Render for PeekView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let count = self.locations.len();
        let hint = if count > 1 {
            "F4 next · ⌘↵ open · esc close"
        } else {
            "⌘↵ open · esc close"
        };

        let entrance_id = ElementId::NamedInteger(
            "bloo-peek-entrance".into(),
            cx.entity_id().as_u64(),
        );
        v_flex()
            .key_context("PeekDefinition")
            .relative()
            .size_full()
            .border_y_1()
            .border_color(colors.border)
            .bg(colors.editor_background)
            .child(
                h_flex()
                    .flex_none()
                    .px_2()
                    .py_0p5()
                    .gap_2()
                    .bg(colors.editor_subheader_background)
                    .border_b_1()
                    .border_color(colors.border_variant)
                    .child(Label::new(self.header_text(cx)).size(LabelSize::Small))
                    .when(count > 1, |row| {
                        row.child(
                            Label::new(format!("{} of {count}", self.selected + 1))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Label::new(hint)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.editor.clone()))
            // Unfolds downward from the line it was opened on.
            .with_animation(
                entrance_id,
                ui::motion::enter_animation(),
                |this, delta| {
                    this.opacity(delta)
                        .top(px(-ui::motion::enter_offset(delta)))
                },
            )
    }
}
