use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use editor::{
    Editor, EditorMode, MultiBuffer,
    display_map::{DisplayRow, DisplaySnapshot},
    scroll::ScrollOffset,
};
use gpui::{
    AnyWindowHandle, AppContext as _, BenchAppContext, Context, Entity, Focusable as _,
    IntoElement, ParentElement as _, Point, Render, Styled as _, Window, div, point,
};
use language::{Buffer, LineEnding, Rope};

#[gpui::bench(
    inputs = pending_long_row_characters(),
    group = "Scroll a long row while it is measured",
    input_name = "character",
    sample_size = 10
)]
fn editor_scroll_pending_long_row(character: &char, cx: &mut BenchAppContext) {
    init_context(cx);

    let text = Rope::from(
        character
            .to_string()
            .repeat(PENDING_LONG_ROW_CHARACTERS)
            .as_str(),
    );
    let mut session_window = None::<AnyWindowHandle>;
    cx.bench_renderer_session(
        PENDING_LONG_ROW_SESSION_TIMEOUT,
        move |cx| {
            let window =
                *session_window.get_or_insert_with(|| cx.add_empty_window().window_handle());
            let text = text.clone();
            let editor_slot = Rc::new(RefCell::new(None));
            let drawn_frames = Rc::new(Cell::new(0));
            cx.update_window(window, |_, window, cx| {
                let editor = cx.new(|cx| {
                    let buffer = cx.new(|cx| Buffer::local_normalized(text, LineEnding::Unix, cx));
                    let buffer = cx.new(|cx| MultiBuffer::singleton(buffer, cx));
                    let mut editor = Editor::new(EditorMode::full(), buffer, None, window, cx);
                    editor.set_style(editor::EditorStyle::default(), window, cx);
                    editor
                });
                editor_slot.replace(Some(editor.clone()));
                window.replace_root(cx, |_, _| PendingLongRowRoot {
                    editor: editor_slot.clone(),
                    drawn_frames: drawn_frames.clone(),
                });
                window.focus(&editor.focus_handle(cx), cx);
            })
            .expect("the benchmark window must remain open");
            let stopped = Arc::new(AtomicBool::new(false));
            let session = PendingLongRowSession {
                editor: editor_slot,
                drawn_frames,
                stopped: stopped.clone(),
                bytes_per_character: character.len_utf8() as u32,
                scroll_x: PENDING_LONG_ROW_SCROLL_START,
                requested_scroll: None,
                visible_start_character: 0,
            };
            (session, window, stopped)
        },
        |session, frame, window, cx| {
            let Some(editor) = session.editor.borrow().clone() else {
                return;
            };
            editor.update(cx, |editor, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                let is_pending = display_snapshot
                    .long_row(DisplayRow(0))
                    .is_some_and(|long_row| !long_row.is_measured());
                assert!(
                    is_pending,
                    "the long row finished measuring before scroll frame {frame}, \
                     increase PENDING_LONG_ROW_CHARACTERS"
                );
                if let Some(requested_scroll) = session.requested_scroll.take() {
                    session.validate_rendered_scroll(
                        requested_scroll,
                        frame,
                        editor,
                        &display_snapshot,
                        cx,
                    );
                }
                if frame == PENDING_LONG_ROW_FRAMES {
                    session.stopped.store(true, Ordering::Release);
                    return;
                }
                session.scroll_x += PENDING_LONG_ROW_SCROLL_STEP;
                let position = point(session.scroll_x, 0.);
                editor.set_scroll_position(position, window, cx);
                assert_eq!(
                    editor.scroll_position(cx),
                    position,
                    "scroll turn {frame} did not accept the scroll request"
                );
                session.requested_scroll = Some(RequestedScroll {
                    position,
                    drawn_frames: session.drawn_frames.get(),
                });
            });
        },
    );
}

struct PendingLongRowSession {
    editor: Rc<RefCell<Option<Entity<Editor>>>>,
    drawn_frames: Rc<Cell<u64>>,
    stopped: Arc<AtomicBool>,
    bytes_per_character: u32,
    scroll_x: f64,
    requested_scroll: Option<RequestedScroll>,
    visible_start_character: u32,
}

impl PendingLongRowSession {
    fn validate_rendered_scroll(
        &mut self,
        requested_scroll: RequestedScroll,
        frame: u64,
        editor: &Editor,
        display_snapshot: &DisplaySnapshot,
        cx: &mut Context<Editor>,
    ) {
        assert!(
            self.drawn_frames.get() > requested_scroll.drawn_frames,
            "no frame was drawn after the scroll request preceding turn {frame}"
        );
        let requested = requested_scroll.position;
        let scroll_position = editor.scroll_position(cx);
        assert!(
            scroll_position.y == requested.y
                && (scroll_position.x - requested.x).abs() <= PENDING_LONG_ROW_SCROLL_TOLERANCE,
            "the frame drawn before turn {frame} is at {scroll_position:?} instead of {requested:?}"
        );
        let visible_ranges = editor.visible_text_ranges(display_snapshot, cx);
        let [visible_range] = visible_ranges.as_slice() else {
            panic!("expected one visible range before turn {frame}, got {visible_ranges:?}");
        };
        assert!(
            visible_range.start.row == 0
                && visible_range.end.row == 0
                && visible_range.start.column < visible_range.end.column
                && visible_range.start.column % self.bytes_per_character == 0,
            "invalid visible range {visible_range:?} before turn {frame}"
        );
        let visible_start_character = visible_range.start.column / self.bytes_per_character;
        let expected_start_characters = (requested.x - PENDING_LONG_ROW_SCROLL_TOLERANCE)
            / PENDING_LONG_ROW_MAX_COLUMNS_PER_CHARACTER
            ..=requested.x + PENDING_LONG_ROW_SCROLL_TOLERANCE;
        assert!(
            visible_start_character > self.visible_start_character
                && expected_start_characters.contains(&f64::from(visible_start_character)),
            "visible text before turn {frame} starts at character {visible_start_character}, \
             expected after character {} and within {expected_start_characters:?}",
            self.visible_start_character
        );
        self.visible_start_character = visible_start_character;
    }
}

struct RequestedScroll {
    position: Point<ScrollOffset>,
    drawn_frames: u64,
}

impl Drop for PendingLongRowSession {
    fn drop(&mut self) {
        self.editor.borrow_mut().take();
    }
}

struct PendingLongRowRoot {
    editor: Rc<RefCell<Option<Entity<Editor>>>>,
    drawn_frames: Rc<Cell<u64>>,
}

impl Render for PendingLongRowRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.drawn_frames.set(self.drawn_frames.get() + 1);
        div().size_full().children(self.editor.borrow().clone())
    }
}

const PENDING_LONG_ROW_CHARACTERS: usize = 20_000_000;
const PENDING_LONG_ROW_FRAMES: u64 = 8;
const PENDING_LONG_ROW_SCROLL_START: f64 = 1_000_000.;
const PENDING_LONG_ROW_SCROLL_STEP: f64 = 200.;
const PENDING_LONG_ROW_SCROLL_TOLERANCE: f64 = 1.;
const PENDING_LONG_ROW_MAX_COLUMNS_PER_CHARACTER: f64 = 2.;
const PENDING_LONG_ROW_SESSION_TIMEOUT: Duration = Duration::from_secs(30);

fn pending_long_row_characters() -> Vec<char> {
    vec!['x', 'é', '漢']
}

fn init_context(cx: &mut BenchAppContext) {
    cx.update(|cx| {
        settings::init(cx);
        assets::Assets.load_test_fonts(cx);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
    });
}

gpui::bench_group!(benches, editor_scroll_pending_long_row);
gpui::bench_main!(benches);
