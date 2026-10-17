use std::{collections::VecDeque, fmt, mem, sync::Arc};

use collections::HashMap;
use language::{Bias, Point, Selection, SelectionGoal};
use multi_buffer::ToPoint as _;
use parking_lot::Mutex;
use smallvec::{SmallVec, smallvec};

use crate::{
    Anchor, DisplayPoint, DisplayRow,
    display_map::{DisplaySnapshot, ToDisplayPoint as _},
    movement::TextLayoutDetails,
    scroll::ScrollPixelOffset,
};

#[derive(Clone, Default)]
pub(crate) struct HorizontalOrigins(Arc<Mutex<RecordedOrigins>>);

#[derive(Clone, Default)]
pub(super) struct HorizontalGoals {
    origins: HorizontalOrigins,
    details: Option<Arc<TextLayoutDetails>>,
    epoch: usize,
    edit_count: usize,
    provisional: Arc<HashMap<usize, Provisional>>,
}

impl HorizontalOrigins {
    pub(crate) fn record(&self, x: ScrollPixelOffset, origin: Anchor, details: &TextLayoutDetails) {
        let mut recorded = self.0.lock();
        if recorded.origins.len() == MAX_RECORDED_ORIGINS {
            recorded.origins.pop_front();
        }
        recorded.origins.push_back((x.to_bits(), origin));
        if recorded.details.is_none() {
            recorded.details = Some(details.detached());
        }
    }

    fn take(&self) -> RecordedOrigins {
        mem::take(&mut *self.0.lock())
    }

    fn is_empty(&self) -> bool {
        self.0.lock().origins.is_empty()
    }
}

impl fmt::Debug for HorizontalOrigins {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HorizontalOrigins").finish_non_exhaustive()
    }
}

impl HorizontalGoals {
    pub(super) fn origins(&self) -> HorizontalOrigins {
        self.origins.clone()
    }

    pub(super) fn has_provisional(&self) -> bool {
        !self.provisional.is_empty()
    }

    pub(super) fn is_unsettled(&self, snapshot: &DisplaySnapshot) -> bool {
        self.has_provisional() && self.epoch != snapshot.long_row_epoch()
    }

    pub(super) fn has_recorded_origins(&self) -> bool {
        !self.origins.is_empty()
    }

    pub(super) fn clear_provisional(&mut self) {
        self.provisional = Arc::default();
    }

    pub(super) fn set_details(&mut self, details: &TextLayoutDetails) {
        self.details = Some(Arc::new(details.detached()));
    }

    pub(super) fn capture<'a>(
        &mut self,
        selections: impl IntoIterator<Item = &'a Selection<Anchor>>,
        snapshot: &DisplaySnapshot,
    ) {
        let recorded = self.origins.take();
        self.capture_with(recorded, selections, snapshot, true);
    }

    pub(super) fn settle(
        &mut self,
        selections: &[Selection<Anchor>],
        snapshot: &DisplaySnapshot,
    ) -> Option<Vec<Selection<Point>>> {
        if !self.is_unsettled(snapshot) {
            return None;
        }
        self.epoch = snapshot.long_row_epoch();
        let buffer = snapshot.buffer_snapshot();
        let edited = buffer.edit_count() != self.edit_count;
        let mut remaining = HashMap::default();
        let mut changed = false;
        let settled = selections
            .iter()
            .map(|selection| {
                let resolved = selection.map(|anchor| anchor.to_point(buffer));
                let Some(provisional) = self
                    .provisional
                    .get(&selection.id)
                    .filter(|provisional| provisional.selection == *selection)
                else {
                    return resolved;
                };
                let positions = provisional
                    .positions
                    .iter()
                    .map(|position| self.settled_position(*position, snapshot))
                    .collect::<SmallVec<[GoalPosition; 2]>>();
                let mut moved = resolved;
                moved.goal = map_goal(selection.goal, &provisional.positions, &positions);
                let mut unsettled = positions.iter().any(|position| position.origin.is_some());
                let derived = if edited {
                    SmallVec::new()
                } else {
                    provisional.derived.clone()
                };
                for derived in &derived {
                    let anchor = match derived.endpoint {
                        Endpoint::Start => selection.start,
                        Endpoint::End => selection.end,
                    };
                    let row = anchor.to_display_point(snapshot).row();
                    let Some(point) = positions
                        .get(derived.position)
                        .and_then(|position| self.resolve(snapshot, row, position.x, derived.bias))
                    else {
                        continue;
                    };
                    unsettled |= snapshot
                        .long_row(point.row())
                        .is_some_and(|long_row| !long_row.is_settled_through(point.column()));
                    let point = point.to_point(snapshot);
                    match derived.endpoint {
                        Endpoint::Start => moved.start = point,
                        Endpoint::End => moved.end = point,
                    }
                }
                changed |= moved != resolved;
                if unsettled {
                    remaining.insert(
                        selection.id,
                        Provisional {
                            selection: *selection,
                            positions,
                            derived,
                        },
                    );
                }
                moved
            })
            .collect::<Vec<_>>();
        self.provisional = Arc::new(remaining);
        changed.then_some(settled)
    }

    pub(super) fn capture_settled<'a>(
        &mut self,
        selections: impl IntoIterator<Item = &'a Selection<Anchor>>,
        snapshot: &DisplaySnapshot,
    ) {
        let derive = snapshot.buffer_snapshot().edit_count() == self.edit_count;
        self.capture_with(RecordedOrigins::default(), selections, snapshot, derive);
    }

    fn capture_with<'a>(
        &mut self,
        recorded: RecordedOrigins,
        selections: impl IntoIterator<Item = &'a Selection<Anchor>>,
        snapshot: &DisplaySnapshot,
        derive: bool,
    ) {
        if let Some(details) = recorded.details {
            self.details = Some(Arc::new(details));
        }
        self.epoch = snapshot.long_row_epoch();
        self.edit_count = snapshot.buffer_snapshot().edit_count();
        if recorded.origins.is_empty()
            && self.provisional.is_empty()
            && !snapshot.may_have_long_rows()
        {
            return;
        }
        let previous = mem::take(&mut self.provisional);
        let recorded_origins = recorded
            .origins
            .into_iter()
            .collect::<HashMap<u64, Anchor>>();
        let mut previous_origins = HashMap::<u64, Anchor>::default();
        for position in previous
            .values()
            .flat_map(|provisional| &provisional.positions)
        {
            if let Some(origin) = position.origin {
                previous_origins
                    .entry(position.x.to_bits())
                    .or_insert(origin);
            }
        }
        let buffer = snapshot.buffer_snapshot();
        let mut provisional = HashMap::default();
        for selection in selections {
            let xs = goal_positions(selection.goal);
            if xs.is_empty() {
                continue;
            }
            let own = previous.get(&selection.id);
            let positions = xs
                .into_iter()
                .map(|x| {
                    let bits = x.to_bits();
                    let origin = own
                        .and_then(|own| {
                            own.positions
                                .iter()
                                .find(|position| position.x.to_bits() == bits)
                                .and_then(|position| position.origin)
                        })
                        .or_else(|| recorded_origins.get(&bits).copied())
                        .or_else(|| previous_origins.get(&bits).copied())
                        .filter(|origin| buffer.can_resolve(origin));
                    GoalPosition { x, origin }
                })
                .collect::<SmallVec<[GoalPosition; 2]>>();
            let derived = if derive {
                self.derived_endpoints(selection, &positions, snapshot)
            } else {
                SmallVec::new()
            };
            if positions.iter().any(|position| position.origin.is_some()) || !derived.is_empty() {
                provisional.insert(
                    selection.id,
                    Provisional {
                        selection: *selection,
                        positions,
                        derived,
                    },
                );
            }
        }
        self.provisional = Arc::new(provisional);
    }

    fn derived_endpoints(
        &self,
        selection: &Selection<Anchor>,
        positions: &[GoalPosition],
        snapshot: &DisplaySnapshot,
    ) -> SmallVec<[Derived; 2]> {
        let mut derived = SmallVec::new();
        for (endpoint, anchor) in [
            (Endpoint::Start, selection.start),
            (Endpoint::End, selection.end),
        ] {
            let point = anchor.to_display_point(snapshot);
            let unsettled_row = snapshot
                .long_row(point.row())
                .is_some_and(|long_row| !long_row.is_settled_through(point.column()));
            let found = positions.iter().enumerate().find_map(|(index, position)| {
                if position.origin.is_none() && !unsettled_row {
                    return None;
                }
                [Bias::Right, Bias::Left].into_iter().find_map(|bias| {
                    (self.resolve(snapshot, point.row(), position.x, bias)? == point).then_some(
                        Derived {
                            endpoint,
                            position: index,
                            bias,
                        },
                    )
                })
            });
            derived.extend(found);
        }
        derived
    }

    fn settled_position(&self, position: GoalPosition, snapshot: &DisplaySnapshot) -> GoalPosition {
        let Some(origin) = position
            .origin
            .filter(|origin| snapshot.buffer_snapshot().can_resolve(origin))
        else {
            return GoalPosition {
                origin: None,
                ..position
            };
        };
        let point = origin.to_display_point(snapshot);
        match snapshot.long_row(point.row()) {
            Some(long_row) => GoalPosition {
                x: long_row.measured_x_for_column(point.column()),
                origin: (!long_row.is_settled_through(point.column())).then_some(origin),
            },
            None => GoalPosition {
                x: self.details.as_deref().map_or(position.x, |details| {
                    snapshot
                        .layout_row(point.row(), details)
                        .x_for_index(point.column() as usize)
                }),
                origin: None,
            },
        }
    }

    fn resolve(
        &self,
        snapshot: &DisplaySnapshot,
        row: DisplayRow,
        x: ScrollPixelOffset,
        bias: Bias,
    ) -> Option<DisplayPoint> {
        let column = match snapshot.long_row(row) {
            Some(long_row) => long_row.measured_closest_column_for_x(x),
            None => snapshot
                .layout_row(row, self.details.as_deref()?)
                .closest_index_for_x(x) as u32,
        };
        Some(snapshot.clip_point(DisplayPoint::new(row, column), bias))
    }
}

impl fmt::Debug for HorizontalGoals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HorizontalGoals")
            .field("epoch", &self.epoch)
            .field("provisional", &self.provisional)
            .finish_non_exhaustive()
    }
}

const MAX_RECORDED_ORIGINS: usize = 256;

#[derive(Clone, Default)]
struct RecordedOrigins {
    origins: VecDeque<(u64, Anchor)>,
    details: Option<TextLayoutDetails>,
}

#[derive(Clone, Debug)]
struct Provisional {
    selection: Selection<Anchor>,
    positions: SmallVec<[GoalPosition; 2]>,
    derived: SmallVec<[Derived; 2]>,
}

#[derive(Clone, Copy, Debug)]
struct GoalPosition {
    x: ScrollPixelOffset,
    origin: Option<Anchor>,
}

#[derive(Clone, Copy, Debug)]
struct Derived {
    endpoint: Endpoint,
    position: usize,
    bias: Bias,
}

#[derive(Clone, Copy, Debug)]
enum Endpoint {
    Start,
    End,
}

fn goal_positions(goal: SelectionGoal) -> SmallVec<[ScrollPixelOffset; 2]> {
    let mut positions = match goal {
        SelectionGoal::None => SmallVec::<[ScrollPixelOffset; 2]>::new(),
        SelectionGoal::HorizontalPosition(x) | SelectionGoal::WrappedHorizontalPosition((_, x)) => {
            smallvec![x]
        }
        SelectionGoal::HorizontalRange { start, end } => smallvec![start, end],
    };
    positions.retain(|x| x.is_finite());
    positions
}

fn map_goal(goal: SelectionGoal, from: &[GoalPosition], to: &[GoalPosition]) -> SelectionGoal {
    let map = |x: ScrollPixelOffset| {
        from.iter()
            .zip(to)
            .find(|(from, _)| from.x.to_bits() == x.to_bits())
            .map_or(x, |(_, to)| to.x)
    };
    match goal {
        SelectionGoal::None => SelectionGoal::None,
        SelectionGoal::HorizontalPosition(x) => SelectionGoal::HorizontalPosition(map(x)),
        SelectionGoal::WrappedHorizontalPosition((row, x)) => {
            SelectionGoal::WrappedHorizontalPosition((row, map(x)))
        }
        SelectionGoal::HorizontalRange { start, end } => SelectionGoal::HorizontalRange {
            start: map(start),
            end: map(end),
        },
    }
}
