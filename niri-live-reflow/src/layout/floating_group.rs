//! Floating-over-floating groups (item 5).
//!
//! Drop zones appear only when a **floating** drag passes over **another floating**
//! window. Dropping onto a zone joins the windows into a group of up to
//! [`MAX_GROUP_MEMBERS`] cells that still lives on the floating strip (not retiled).
//!
//! Model: every member stores its own cell (strip coordinates). The group box is the
//! union of the cells. Cells never overlap and neighbors keep the layout gap between
//! them.
//!
//! - Grow: a lone target splits into halves (left/right, or top/bottom bands). On a
//!   group, the hovered cell splits (left/right half, or bottom band → cell on top, new
//!   one below), or the centered bottom zone of the group adds a full-width bottom strip
//!   (existing cells shrink vertically, keeping proportions). A full group offers no
//!   zones.
//! - Resize: the group box is the limit of its members. An edge shared with neighbors
//!   moves that boundary only (down to a minimum member size); an edge on the group
//!   boundary resizes the whole group, scaling every member, clamped to [`GroupLimits`].
//! - Leave: picking up / closing a member lets the sibling cells grow into its space;
//!   two members left are laid side by side in the old box; one left dissolves the group.
//! - Move: the group moves as a whole for in-layout moves.
//! - FullViewport: scales every cell from the group box to the tile band.

use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::utils::{id::IdCounter, ResizeEdge};

/// Maximum number of windows in a floating group.
pub const MAX_GROUP_MEMBERS: usize = 4;

/// Top/bottom band of a target cell, analogous to tiled InColumn.
const STACK_BAND_FRAC: f64 = 0.2;
/// Below this width, a cell offers no left/right split.
const MIN_HORIZONTAL_SPLIT: f64 = 96.;
/// Below this height, a cell offers no top/bottom split.
const MIN_VERTICAL_SPLIT: f64 = 96.;
/// Smallest size a member can be resized down to inside its group.
const MIN_MEMBER: f64 = 96.;
/// Overlap tolerance for float comparisons.
const EPS: f64 = 0.5;
/// How far off an edge may be and still count as touching across a gap.
const ABUT_TOL: f64 = 2.;

type Rect = Rectangle<f64, Logical>;

static FLOATING_GROUP_ID_COUNTER: IdCounter = IdCounter::new();

/// Identity of a floating group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FloatingGroupId(u64);

impl FloatingGroupId {
    pub fn next() -> Self {
        Self(FLOATING_GROUP_ID_COUNTER.next())
    }
}

/// Where the incoming window sits inside the split cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatingSplitSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// How a drop grows the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatingJoinOp {
    /// Split the target cell; the incoming window takes `side`.
    Split(FloatingSplitSide),
    /// New full-width strip along the bottom of the target's group.
    BottomStrip,
}

/// Drop preview in workspace render coordinates (`x = logical_x - view_pos`).
#[derive(Debug, Clone, PartialEq)]
pub struct FloatingJoinHint {
    pub op: FloatingJoinOp,
    /// Cell the incoming window will occupy.
    pub hint: Rect,
    /// Zone outlines on the target (at most 4).
    pub guides: Vec<Rect>,
    /// Both windows fit with their size limits; a drop is refused otherwise.
    pub fits: bool,
}

/// Size limits a member's app asks for, as tile sizes. A zero min means none; an infinite
/// max means none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellLimits {
    pub min: Size<f64, Logical>,
    pub max: Size<f64, Logical>,
}

impl CellLimits {
    pub fn none() -> Self {
        Self {
            min: Size::from((0., 0.)),
            max: Size::from((f64::INFINITY, f64::INFINITY)),
        }
    }

    fn min_on(self, axis: Axis) -> f64 {
        limit(self.min, axis)
    }

    fn max_on(self, axis: Axis) -> f64 {
        limit(self.max, axis)
    }

    /// Floor for interactive resizes: the app min, and never below [`MIN_MEMBER`].
    fn floor_on(self, axis: Axis) -> f64 {
        self.min_on(axis).max(MIN_MEMBER)
    }
}

impl Default for CellLimits {
    fn default() -> Self {
        Self::none()
    }
}

/// Build [`CellLimits`] from a tile's non-fullscreen min/max sizes. A non-positive max
/// means unlimited.
pub fn cell_limits_of(min: Size<f64, Logical>, max: Size<f64, Logical>) -> CellLimits {
    CellLimits {
        min: Size::from((min.w.max(0.), min.h.max(0.))),
        max: Size::from((
            if max.w <= 0. { f64::INFINITY } else { max.w },
            if max.h <= 0. { f64::INFINITY } else { max.h },
        )),
    }
}

fn lim(limits: &[CellLimits], i: usize) -> CellLimits {
    limits.get(i).copied().unwrap_or_default()
}

/// Size limits of a group box (and the max for any floating tile).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroupLimits {
    pub min: Size<f64, Logical>,
    pub max: Size<f64, Logical>,
}

impl GroupLimits {
    /// `band` is the scrolling working area (struts included).
    ///
    /// Max: the tile band (`band - 2 * gaps`), same as FullViewport.
    /// Min: a quarter of a tiled window at 50% of the viewport.
    pub fn from_band(band: Rect, gaps: f64) -> Self {
        let max_w = (band.size.w - gaps * 2.).max(1.);
        let max_h = (band.size.h - gaps * 2.).max(1.);
        let ref_w = ((band.size.w - gaps) * 0.5 - gaps).max(1.);
        let ref_h = max_h;
        Self {
            min: Size::from(((ref_w / 4.).min(max_w), (ref_h / 4.).min(max_h))),
            max: Size::from((max_w, max_h)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    X,
    Y,
}

fn lo(r: Rect, axis: Axis) -> f64 {
    match axis {
        Axis::X => r.loc.x,
        Axis::Y => r.loc.y,
    }
}

fn len(r: Rect, axis: Axis) -> f64 {
    match axis {
        Axis::X => r.size.w,
        Axis::Y => r.size.h,
    }
}

fn hi(r: Rect, axis: Axis) -> f64 {
    lo(r, axis) + len(r, axis)
}

fn other(axis: Axis) -> Axis {
    match axis {
        Axis::X => Axis::Y,
        Axis::Y => Axis::X,
    }
}

fn with_span(mut r: Rect, axis: Axis, start: f64, length: f64) -> Rect {
    match axis {
        Axis::X => {
            r.loc.x = start;
            r.size.w = length;
        }
        Axis::Y => {
            r.loc.y = start;
            r.size.h = length;
        }
    }
    r
}

fn limit(size: Size<f64, Logical>, axis: Axis) -> f64 {
    match axis {
        Axis::X => size.w,
        Axis::Y => size.h,
    }
}

/// Spans of `a` and `b` along `axis` overlap by more than `EPS`.
fn overlaps_on(a: Rect, b: Rect, axis: Axis) -> bool {
    lo(a, axis) < hi(b, axis) - EPS && lo(b, axis) < hi(a, axis) - EPS
}

pub fn rects_overlap(a: Rect, b: Rect) -> bool {
    overlaps_on(a, b, Axis::X) && overlaps_on(a, b, Axis::Y)
}

fn contains_excl(rect: Rect, pos: Point<f64, Logical>) -> bool {
    pos.x >= rect.loc.x
        && pos.y >= rect.loc.y
        && pos.x < rect.loc.x + rect.size.w
        && pos.y < rect.loc.y + rect.size.h
}

pub fn union_rect(a: Rect, b: Rect) -> Rect {
    let x0 = a.loc.x.min(b.loc.x);
    let y0 = a.loc.y.min(b.loc.y);
    let x1 = (a.loc.x + a.size.w).max(b.loc.x + b.size.w);
    let y1 = (a.loc.y + a.size.h).max(b.loc.y + b.size.h);
    Rectangle::new(Point::from((x0, y0)), Size::from((x1 - x0, y1 - y0)))
}

/// Bounding box of `cells` (must not be empty).
pub fn union_all(cells: &[Rect]) -> Rect {
    cells[1..]
        .iter()
        .fold(cells[0], |acc, r| union_rect(acc, *r))
}

/// Split `combined` into `(incoming, target)` rects separated by `gap`.
pub fn split_combined(combined: Rect, incoming_side: FloatingSplitSide, gap: f64) -> (Rect, Rect) {
    let gap = gap.clamp(0., combined.size.w.max(combined.size.h) / 2.);
    let x0 = combined.loc.x;
    let y0 = combined.loc.y;
    let w = combined.size.w.max(2.);
    let h = combined.size.h.max(2.);

    match incoming_side {
        FloatingSplitSide::Left | FloatingSplitSide::Right => {
            let inner = (w - gap).max(2.);
            let lead = (inner / 2.).max(1.);
            let trail = (inner - lead).max(1.);
            let left = Rectangle::new(Point::from((x0, y0)), Size::from((lead, h)));
            let right = Rectangle::new(Point::from((x0 + lead + gap, y0)), Size::from((trail, h)));
            if incoming_side == FloatingSplitSide::Left {
                (left, right)
            } else {
                (right, left)
            }
        }
        FloatingSplitSide::Top | FloatingSplitSide::Bottom => {
            let inner = (h - gap).max(2.);
            let lead = (inner / 2.).max(1.);
            let trail = (inner - lead).max(1.);
            let top = Rectangle::new(Point::from((x0, y0)), Size::from((w, lead)));
            let bottom = Rectangle::new(Point::from((x0, y0 + lead + gap)), Size::from((w, trail)));
            if incoming_side == FloatingSplitSide::Top {
                (top, bottom)
            } else {
                (bottom, top)
            }
        }
    }
}

/// Like [`split_combined`], but honoring the apps' minimum sizes: halves when both fit, else
/// the limited one gets its minimum and the other the rest. `None` when both minimums (plus
/// the gap) don't fit, or the incoming window doesn't fit across the split.
pub fn split_fit(
    combined: Rect,
    incoming_side: FloatingSplitSide,
    gap: f64,
    incoming: CellLimits,
    target: CellLimits,
) -> Option<(Rect, Rect)> {
    let axis = match incoming_side {
        FloatingSplitSide::Left | FloatingSplitSide::Right => Axis::X,
        FloatingSplitSide::Top | FloatingSplitSide::Bottom => Axis::Y,
    };
    let cross = other(axis);
    if incoming.min_on(cross) > len(combined, cross) + EPS {
        return None;
    }
    let inner = len(combined, axis) - gap;
    let (inc_min, tgt_min) = (incoming.min_on(axis), target.min_on(axis));
    if inc_min + tgt_min > inner + EPS {
        return None;
    }
    let (inc, tgt) = split_combined(combined, incoming_side, gap);
    if len(inc, axis) >= inc_min - EPS && len(tgt, axis) >= tgt_min - EPS {
        return Some((inc, tgt));
    }
    let inc_len = (inner / 2.).clamp(inc_min, (inner - tgt_min).max(inc_min));
    let tgt_len = inner - inc_len;
    let start = lo(combined, axis);
    let incoming_first = matches!(
        incoming_side,
        FloatingSplitSide::Left | FloatingSplitSide::Top
    );
    Some(if incoming_first {
        (
            with_span(combined, axis, start, inc_len),
            with_span(combined, axis, start + inc_len + gap, tgt_len),
        )
    } else {
        (
            with_span(combined, axis, start + tgt_len + gap, inc_len),
            with_span(combined, axis, start, tgt_len),
        )
    })
}

/// Zone outline of `side` on a cell (bands for top/bottom, halves for left/right).
fn zone_rect(cell: Rect, side: FloatingSplitSide) -> Rect {
    let Rectangle { loc, size } = cell;
    let band = size.h * STACK_BAND_FRAC;
    match side {
        FloatingSplitSide::Left => Rectangle::new(loc, Size::from((size.w / 2., size.h))),
        FloatingSplitSide::Right => Rectangle::new(
            Point::from((loc.x + size.w / 2., loc.y)),
            Size::from((size.w - size.w / 2., size.h)),
        ),
        FloatingSplitSide::Top => Rectangle::new(loc, Size::from((size.w, band))),
        FloatingSplitSide::Bottom => Rectangle::new(
            Point::from((loc.x, loc.y + size.h - band)),
            Size::from((size.w, band)),
        ),
    }
}

/// Zone of a lone floating window under `pointer`: top/bottom bands stack, the rest
/// splits left/right. Narrow targets only stack (by halves).
pub fn lone_side_at(target: Rect, pointer: Point<f64, Logical>) -> Option<FloatingSplitSide> {
    let Rectangle { loc, size } = target;
    let can_h = size.w >= MIN_HORIZONTAL_SPLIT;
    let can_v = size.h >= MIN_VERTICAL_SPLIT;
    let band = size.h * STACK_BAND_FRAC;
    if can_v && (!can_h || pointer.y < loc.y + band || pointer.y > loc.y + size.h - band) {
        return Some(if pointer.y < loc.y + size.h / 2. {
            FloatingSplitSide::Top
        } else {
            FloatingSplitSide::Bottom
        });
    }
    if can_h {
        return Some(if pointer.x < loc.x + size.w / 2. {
            FloatingSplitSide::Left
        } else {
            FloatingSplitSide::Right
        });
    }
    None
}

fn lone_guides(target: Rect) -> Vec<Rect> {
    let mut guides = Vec::new();
    if target.size.h >= MIN_VERTICAL_SPLIT {
        if target.size.w >= MIN_HORIZONTAL_SPLIT {
            guides.push(zone_rect(target, FloatingSplitSide::Top));
            guides.push(zone_rect(target, FloatingSplitSide::Bottom));
        } else {
            let (top, bottom) = split_combined(target, FloatingSplitSide::Top, 0.);
            guides.push(top);
            guides.push(bottom);
        }
    }
    if target.size.w >= MIN_HORIZONTAL_SPLIT {
        guides.push(zone_rect(target, FloatingSplitSide::Left));
        guides.push(zone_rect(target, FloatingSplitSide::Right));
    }
    guides
}

/// Drop onto a lone floating window (render rect).
pub fn lone_join_at(
    target: Rect,
    pointer: Point<f64, Logical>,
    gap: f64,
    incoming: CellLimits,
    target_limits: CellLimits,
) -> Option<FloatingJoinHint> {
    if !contains_excl(target, pointer) {
        return None;
    }
    let side = lone_side_at(target, pointer)?;
    let (hint, fits) = match split_fit(target, side, gap, incoming, target_limits) {
        Some((hint, _)) => (hint, true),
        None => (split_combined(target, side, gap).0, false),
    };
    Some(FloatingJoinHint {
        op: FloatingJoinOp::Split(side),
        hint,
        guides: lone_guides(target),
        fits,
    })
}

/// Zone of a grouped cell under `pointer`: bottom band splits it vertically, the rest
/// splits it left/right.
pub fn cell_side_at(cell: Rect, pointer: Point<f64, Logical>) -> Option<FloatingSplitSide> {
    let Rectangle { loc, size } = cell;
    let can_h = size.w >= MIN_HORIZONTAL_SPLIT;
    let can_v = size.h >= MIN_VERTICAL_SPLIT;
    let band = size.h * STACK_BAND_FRAC;
    if can_v && (!can_h || pointer.y > loc.y + size.h - band) {
        return Some(FloatingSplitSide::Bottom);
    }
    if can_h {
        return Some(if pointer.x < loc.x + size.w / 2. {
            FloatingSplitSide::Left
        } else {
            FloatingSplitSide::Right
        });
    }
    None
}

/// Centered bottom zone of a group box that adds a full-width bottom strip.
pub fn bottom_center_zone(bbox: Rect) -> Rect {
    let band = bbox.size.h * STACK_BAND_FRAC;
    Rectangle::new(
        Point::from((
            bbox.loc.x + bbox.size.w / 3.,
            bbox.loc.y + bbox.size.h - band,
        )),
        Size::from((bbox.size.w / 3., band)),
    )
}

/// Max number of cells stacked vertically at any cell's horizontal center.
pub fn stack_depth(cells: &[Rect]) -> usize {
    cells
        .iter()
        .map(|c| {
            let cx = c.loc.x + c.size.w / 2.;
            cells
                .iter()
                .filter(|d| d.loc.x <= cx && cx < d.loc.x + d.size.w)
                .count()
        })
        .max()
        .unwrap_or(0)
        .max(1)
}

/// Map `cells` from box `from` to box `to`, keeping proportions and exact gaps.
pub fn scale_cells(cells: &[Rect], from: Rect, to: Rect, gap: f64) -> Vec<Rect> {
    let sx = (to.size.w + gap) / (from.size.w + gap).max(1.);
    let sy = (to.size.h + gap) / (from.size.h + gap).max(1.);
    let mx = |x: f64| to.loc.x + (x - from.loc.x) * sx;
    let my = |y: f64| to.loc.y + (y - from.loc.y) * sy;
    cells
        .iter()
        .map(|c| {
            let x0 = mx(c.loc.x);
            let x1 = mx(c.loc.x + c.size.w + gap) - gap;
            let y0 = my(c.loc.y);
            let y1 = my(c.loc.y + c.size.h + gap) - gap;
            Rectangle::new(
                Point::from((x0, y0)),
                Size::from(((x1 - x0).max(1.), (y1 - y0).max(1.))),
            )
        })
        .collect()
}

/// Add a full-width strip along the bottom of the group box. Existing cells shrink
/// vertically (proportionally) so the strip gets an equal share of the rows.
///
/// Returns `(existing cells, strip)`.
pub fn add_bottom_strip(cells: &[Rect], gap: f64) -> (Vec<Rect>, Rect) {
    let bbox = union_all(cells);
    let depth = stack_depth(cells) as f64;
    let row = ((bbox.size.h - depth * gap) / (depth + 1.)).max(1.);
    bottom_strip_with_row(cells, row, gap)
}

fn bottom_strip_with_row(cells: &[Rect], row: f64, gap: f64) -> (Vec<Rect>, Rect) {
    let bbox = union_all(cells);
    let top_h = (bbox.size.h - gap - row).max(1.);
    let top_box = Rectangle::new(bbox.loc, Size::from((bbox.size.w, top_h)));
    let scaled = scale_cells(cells, bbox, top_box, gap);
    let strip = Rectangle::new(
        Point::from((bbox.loc.x, bbox.loc.y + top_h + gap)),
        Size::from((bbox.size.w, row)),
    );
    (scaled, strip)
}

/// [`add_bottom_strip`] honoring the apps' minimum sizes: the strip gets at least the incoming
/// window's minimum height and the existing cells keep theirs. `None` when that can't fit.
pub fn add_bottom_strip_fit(
    cells: &[Rect],
    limits: &[CellLimits],
    gap: f64,
    incoming: CellLimits,
) -> Option<(Vec<Rect>, Rect)> {
    let bbox = union_all(cells);
    if incoming.min_on(Axis::X) > bbox.size.w + EPS {
        return None;
    }
    let depth = stack_depth(cells) as f64;
    let default_row = ((bbox.size.h - depth * gap) / (depth + 1.)).max(1.);
    let inc_min = incoming.min_on(Axis::Y);
    let others_fit = |row: f64| {
        let (scaled, _) = bottom_strip_with_row(cells, row, gap);
        scaled
            .iter()
            .enumerate()
            .all(|(i, c)| c.size.h >= lim(limits, i).min_on(Axis::Y) - EPS)
    };

    let row = default_row.max(inc_min);
    if row > bbox.size.h - gap - 1. {
        return None;
    }
    if others_fit(row) {
        return Some(bottom_strip_with_row(cells, row, gap));
    }
    if !others_fit(inc_min) || inc_min >= row {
        return None;
    }
    // The tallest strip (down to the incoming minimum) that leaves the others theirs.
    let (mut ok, mut bad) = (inc_min, row);
    for _ in 0..32 {
        let mid = (ok + bad) / 2.;
        if others_fit(mid) {
            ok = mid;
        } else {
            bad = mid;
        }
    }
    Some(bottom_strip_with_row(cells, ok, gap))
}

fn group_guides(cells: &[Rect], cell: Option<Rect>) -> Vec<Rect> {
    let mut guides = Vec::new();
    if let Some(cell) = cell {
        if cell.size.w >= MIN_HORIZONTAL_SPLIT {
            guides.push(zone_rect(cell, FloatingSplitSide::Left));
            guides.push(zone_rect(cell, FloatingSplitSide::Right));
        }
        if cell.size.h >= MIN_VERTICAL_SPLIT {
            guides.push(zone_rect(cell, FloatingSplitSide::Bottom));
        }
    }
    guides.push(bottom_center_zone(union_all(cells)));
    guides
}

/// Drop onto a group whose cells (render coords) are `cells`.
///
/// Returns the targeted cell index and the hint. A full group has no zones.
pub fn group_join_at(
    cells: &[Rect],
    cell_limits: &[CellLimits],
    pointer: Point<f64, Logical>,
    gap: f64,
    incoming: CellLimits,
) -> Option<(usize, FloatingJoinHint)> {
    if cells.is_empty() || cells.len() >= MAX_GROUP_MEMBERS {
        return None;
    }
    let bbox = union_all(cells);
    if !contains_excl(bbox, pointer) {
        return None;
    }
    let hit = cells.iter().position(|c| contains_excl(*c, pointer));

    if contains_excl(bottom_center_zone(bbox), pointer) {
        let (hint, fits) = match add_bottom_strip_fit(cells, cell_limits, gap, incoming) {
            Some((_, strip)) => (strip, true),
            None => (add_bottom_strip(cells, gap).1, false),
        };
        let idx = hit.unwrap_or_else(|| {
            // In the gap: the bottom-most cell is as good a target as any.
            (0..cells.len())
                .max_by(|a, b| f64::total_cmp(&hi(cells[*a], Axis::Y), &hi(cells[*b], Axis::Y)))
                .unwrap()
        });
        return Some((
            idx,
            FloatingJoinHint {
                op: FloatingJoinOp::BottomStrip,
                hint,
                guides: group_guides(cells, hit.map(|i| cells[i])),
                fits,
            },
        ));
    }

    let idx = hit?;
    let cell = cells[idx];
    let side = cell_side_at(cell, pointer)?;
    let (hint, fits) = match split_fit(cell, side, gap, incoming, lim(cell_limits, idx)) {
        Some((hint, _)) => (hint, true),
        None => (split_combined(cell, side, gap).0, false),
    };
    Some((
        idx,
        FloatingJoinHint {
            op: FloatingJoinOp::Split(side),
            hint,
            guides: group_guides(cells, Some(cell)),
            fits,
        },
    ))
}

/// Cells after joining: `cells[target]` is the targeted cell. Returns the new cells of
/// the existing members (same order) and the incoming window's cell, or `None` when the
/// apps' minimum sizes don't fit.
pub fn join_cells(
    cells: &[Rect],
    cell_limits: &[CellLimits],
    target: usize,
    op: FloatingJoinOp,
    gap: f64,
    incoming: CellLimits,
) -> Option<(Vec<Rect>, Rect)> {
    match op {
        FloatingJoinOp::Split(side) => {
            let (inc, rest) =
                split_fit(cells[target], side, gap, incoming, lim(cell_limits, target))?;
            let mut out = cells.to_vec();
            out[target] = rest;
            Some((out, inc))
        }
        FloatingJoinOp::BottomStrip => add_bottom_strip_fit(cells, cell_limits, gap, incoming),
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= ABUT_TOL
}

/// Move the boundary next to member `idx` along `axis` by `d`: its far edge when
/// `member_before`, else its near edge. Cells ending at the boundary grow by `d`, cells
/// starting past it (after `gap`) shrink by `d`. Only cells chained to the member across
/// the boundary move, so the other rows/columns keep their split. `d` is clamped so nobody
/// goes below its app floor (or [`MIN_MEMBER`]) or above its app max. Returns `None` when
/// the member has no neighbor across that edge.
fn move_boundary(
    cells: &[Rect],
    limits: &[CellLimits],
    idx: usize,
    axis: Axis,
    member_before: bool,
    d: f64,
    gap: f64,
) -> Option<Vec<Rect>> {
    let n = cells.len();
    let m = cells[idx];
    let line = if member_before {
        hi(m, axis)
    } else {
        lo(m, axis) - gap
    };
    let cross = other(axis);
    let mut before = vec![false; n];
    let mut after = vec![false; n];
    if member_before {
        before[idx] = true;
    } else {
        after[idx] = true;
    }
    loop {
        let mut changed = false;
        for i in 0..n {
            let c = cells[i];
            if !before[i]
                && near(hi(c, axis), line)
                && (0..n).any(|j| after[j] && overlaps_on(c, cells[j], cross))
            {
                before[i] = true;
                changed = true;
            }
            if !after[i]
                && near(lo(c, axis), line + gap)
                && (0..n).any(|j| before[j] && overlaps_on(c, cells[j], cross))
            {
                after[i] = true;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if !before.contains(&true) || !after.contains(&true) {
        return None;
    }

    let shrink_room = |side: &[bool]| {
        (0..n)
            .filter(|&i| side[i])
            .map(|i| {
                let floor = lim(limits, i).floor_on(axis).min(len(cells[i], axis));
                (len(cells[i], axis) - floor).max(0.)
            })
            .fold(f64::MAX, f64::min)
    };
    let grow_room = |side: &[bool]| {
        (0..n)
            .filter(|&i| side[i])
            .map(|i| {
                let ceil = lim(limits, i).max_on(axis);
                if ceil.is_finite() {
                    (ceil - len(cells[i], axis)).max(0.)
                } else {
                    f64::MAX
                }
            })
            .fold(f64::MAX, f64::min)
    };
    // Positive d grows the before side and shrinks the after side.
    let d = d.clamp(
        (-shrink_room(&before)).max(-grow_room(&after)),
        shrink_room(&after).min(grow_room(&before)),
    );
    let mut out = cells.to_vec();
    for i in 0..n {
        let c = cells[i];
        if before[i] {
            out[i] = with_span(c, axis, lo(c, axis), len(c, axis) + d);
        } else if after[i] {
            out[i] = with_span(c, axis, lo(c, axis) + d, len(c, axis) - d);
        }
    }
    Some(out)
}

/// The `edges` of member `idx` that lie on the group box boundary.
pub fn outer_edges(cells: &[Rect], idx: usize, edges: ResizeEdge) -> ResizeEdge {
    let bbox = union_all(cells);
    let m = cells[idx];
    let mut out = ResizeEdge::empty();
    for (edge, on) in [
        (ResizeEdge::LEFT, near(lo(m, Axis::X), lo(bbox, Axis::X))),
        (ResizeEdge::RIGHT, near(hi(m, Axis::X), hi(bbox, Axis::X))),
        (ResizeEdge::TOP, near(lo(m, Axis::Y), lo(bbox, Axis::Y))),
        (ResizeEdge::BOTTOM, near(hi(m, Axis::Y), hi(bbox, Axis::Y))),
    ] {
        if on && edges.contains(edge) {
            out |= edge;
        }
    }
    out
}

/// A member whose `edges` all lie on the group box boundary (the corner cell for two
/// edges); falls back to one touching any of them.
pub fn member_on_edges(cells: &[Rect], edges: ResizeEdge) -> Option<usize> {
    if cells.is_empty() {
        return None;
    }
    (0..cells.len())
        .find(|&i| outer_edges(cells, i, edges) == edges)
        .or_else(|| (0..cells.len()).find(|&i| !outer_edges(cells, i, edges).is_empty()))
}

/// Group box after dragging its outer `edges` by `delta`, clamped to `limits`. The opposite
/// sides stay put.
pub fn resize_box(
    bbox: Rect,
    edges: ResizeEdge,
    delta: Point<f64, Logical>,
    limits: GroupLimits,
) -> Rect {
    let mut out = bbox;
    for (axis, d, fwd_edge, back_edge) in [
        (Axis::X, delta.x, ResizeEdge::RIGHT, ResizeEdge::LEFT),
        (Axis::Y, delta.y, ResizeEdge::BOTTOM, ResizeEdge::TOP),
    ] {
        let forward = if edges.contains(fwd_edge) {
            true
        } else if edges.contains(back_edge) {
            false
        } else {
            continue;
        };
        let cur = len(bbox, axis);
        let max = limit(limits.max, axis).max(cur);
        let min = limit(limits.min, axis).min(cur);
        let new_len = (cur + if forward { d } else { -d }).clamp(min, max);
        out = if forward {
            with_span(out, axis, lo(bbox, axis), new_len)
        } else {
            with_span(out, axis, hi(bbox, axis) - new_len, new_len)
        };
    }
    out
}

/// Smallest group-box length along `axis` that keeps every cell at its app floor after a
/// proportional scale (cells already smaller stay as they are).
fn cells_min_box(cells: &[Rect], limits: &[CellLimits], axis: Axis, gap: f64) -> f64 {
    let box_len = len(union_all(cells), axis);
    cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let floor = lim(limits, i).floor_on(axis).min(len(*c, axis));
            let den = (len(*c, axis) + gap).max(1.);
            (floor + gap) * (box_len + gap) / den - gap
        })
        .fold(1., f64::max)
}

/// Interactive resize of member `idx` from the cells at grab start. `delta` is the
/// pointer delta since the grab started.
///
/// An edge shared with neighbors only moves that boundary: the group box never changes and
/// the move stops at the members' size limits. An edge on the group boundary resizes the
/// whole group instead (clamped to `limits` and the members' floors), scaling every member.
pub fn resize_member(
    start: &[Rect],
    member_limits: &[CellLimits],
    idx: usize,
    edges: ResizeEdge,
    delta: Point<f64, Logical>,
    gap: f64,
    limits: GroupLimits,
) -> Vec<Rect> {
    let outer = outer_edges(start, idx, edges);
    let inner = edges.difference(outer);
    let mut cells = start.to_vec();
    for (axis, d, fwd_edge, back_edge) in [
        (Axis::X, delta.x, ResizeEdge::RIGHT, ResizeEdge::LEFT),
        (Axis::Y, delta.y, ResizeEdge::BOTTOM, ResizeEdge::TOP),
    ] {
        let forward = if inner.contains(fwd_edge) {
            true
        } else if inner.contains(back_edge) {
            false
        } else {
            continue;
        };
        if let Some(out) = move_boundary(&cells, member_limits, idx, axis, forward, d, gap) {
            cells = out;
        }
    }
    if outer.is_empty() {
        return cells;
    }
    let from = union_all(&cells);
    let mut group = limits;
    group.min.w = group
        .min
        .w
        .max(cells_min_box(&cells, member_limits, Axis::X, gap))
        .min(from.size.w);
    group.min.h = group
        .min
        .h
        .max(cells_min_box(&cells, member_limits, Axis::Y, gap))
        .min(from.size.h);
    let to = resize_box(from, outer, delta, group);
    if from == to {
        return cells;
    }
    scale_cells(&cells, from, to, gap)
}

/// Set the boundary next to member `idx` along `axis` so the member gets `target` length.
/// Uses the far edge when it has neighbors there, else the near edge.
fn set_member_len(
    cells: &[Rect],
    limits: &[CellLimits],
    idx: usize,
    axis: Axis,
    target: f64,
    gap: f64,
) -> Vec<Rect> {
    let cur = len(cells[idx], axis);
    let target = target.clamp(
        lim(limits, idx)
            .floor_on(axis)
            .min(cur.max(lim(limits, idx).floor_on(axis))),
        lim(limits, idx).max_on(axis),
    );
    move_boundary(cells, limits, idx, axis, true, target - cur, gap)
        .or_else(|| move_boundary(cells, limits, idx, axis, false, cur - target, gap))
        .unwrap_or_else(|| cells.to_vec())
}

/// A full-width member is cut to the left half of the box; the neighbor row it borders
/// (above when it sits at the bottom, else below) fills the freed right part.
fn cut_full_width(cells: &[Rect], limits: &[CellLimits], idx: usize, gap: f64) -> Vec<Rect> {
    let bbox = union_all(cells);
    let m = cells[idx];
    let half_w = ((bbox.size.w - gap) / 2.).max(1.);
    let above = near(hi(m, Axis::Y), hi(bbox, Axis::Y));
    let neighbors: Vec<usize> = (0..cells.len())
        .filter(|&i| i != idx)
        .filter(|&i| {
            if above {
                near(hi(cells[i], Axis::Y), lo(m, Axis::Y) - gap)
            } else {
                near(lo(cells[i], Axis::Y), hi(m, Axis::Y) + gap)
            }
        })
        .collect();
    let Some(&filler) = neighbors
        .iter()
        .max_by(|a, b| f64::total_cmp(&hi(cells[**a], Axis::X), &hi(cells[**b], Axis::X)))
    else {
        return cells.to_vec();
    };

    let mut out = cells.to_vec();
    let f = cells[filler];
    let y0 = lo(f, Axis::Y).min(lo(m, Axis::Y));
    let y1 = hi(f, Axis::Y).max(hi(m, Axis::Y));
    if neighbors.len() == 1 {
        // The neighbor spans the full width too: both become side by side halves.
        let left = half_w
            .max(lim(limits, idx).floor_on(Axis::X))
            .min((bbox.size.w - gap - lim(limits, filler).floor_on(Axis::X)).max(1.));
        let right = (bbox.size.w - gap - left).max(1.);
        out[idx] = Rectangle::new(Point::from((bbox.loc.x, y0)), Size::from((left, y1 - y0)));
        out[filler] = Rectangle::new(
            Point::from((hi(bbox, Axis::X) - right, y0)),
            Size::from((right, y1 - y0)),
        );
        return out;
    }
    out[filler] = with_span(f, Axis::Y, y0, y1 - y0);
    let w = (lo(f, Axis::X) - gap - bbox.loc.x).max(1.);
    out[idx] = with_span(m, Axis::X, bbox.loc.x, w);
    set_member_len(&out, limits, idx, Axis::X, half_w, gap)
}

/// Lime Half on member `idx`: inside the same group box, the member gets 50% of the box
/// width (its row neighbor takes the rest) and 50% of its height (pushing the boundary with
/// the cell below, or above when it is on the bottom row). A full-height member keeps its
/// height; a full-width one is cut to half width and the bordering row fills the hole.
/// Stops at each app's size limits.
pub fn half_member(
    cells: &[Rect],
    member_limits: &[CellLimits],
    idx: usize,
    gap: f64,
) -> Vec<Rect> {
    if cells.len() < 2 || idx >= cells.len() {
        return cells.to_vec();
    }
    let bbox = union_all(cells);
    let half_w = ((bbox.size.w - gap) / 2.).max(1.);
    let half_h = ((bbox.size.h - gap) / 2.).max(1.);

    let m = cells[idx];
    let full_w = near(lo(m, Axis::X), lo(bbox, Axis::X)) && near(hi(m, Axis::X), hi(bbox, Axis::X));
    let mut out = if full_w {
        cut_full_width(cells, member_limits, idx, gap)
    } else {
        set_member_len(cells, member_limits, idx, Axis::X, half_w, gap)
    };

    let m = out[idx];
    let full_h = near(lo(m, Axis::Y), lo(bbox, Axis::Y)) && near(hi(m, Axis::Y), hi(bbox, Axis::Y));
    if !full_h {
        out = set_member_len(&out, member_limits, idx, Axis::Y, half_h, gap);
    }
    out
}

/// Clamp a tile size to the floating max.
pub fn clamp_to_max(size: Size<f64, Logical>, limits: GroupLimits) -> Size<f64, Logical> {
    Size::from((size.w.min(limits.max.w), size.h.min(limits.max.h)))
}

/// Remaining cells after `removed` left the group.
///
/// The cells that exactly border `removed` on one side and together span it grow into its
/// space (above first, then below, left, right). Two cells left end up side by side in
/// the old box.
pub fn redistribute(remaining: &[Rect], removed: Rect, gap: f64) -> Vec<Rect> {
    let mut cells = remaining.to_vec();
    if cells.len() < 2 {
        return cells;
    }
    let old_box = union_rect(union_all(&cells), removed);

    // (axis of growth, candidates sit before `removed` along that axis)
    for (axis, before) in [
        (Axis::Y, true),
        (Axis::Y, false),
        (Axis::X, true),
        (Axis::X, false),
    ] {
        let cross = other(axis);
        let cands: Vec<usize> = (0..cells.len())
            .filter(|&i| {
                let c = cells[i];
                let touches = if before {
                    (lo(removed, axis) - (hi(c, axis) + gap)).abs() <= ABUT_TOL
                } else {
                    (lo(c, axis) - (hi(removed, axis) + gap)).abs() <= ABUT_TOL
                };
                touches
                    && lo(c, cross) >= lo(removed, cross) - ABUT_TOL
                    && hi(c, cross) <= hi(removed, cross) + ABUT_TOL
            })
            .collect();
        if cands.is_empty() {
            continue;
        }

        let mut spans: Vec<(f64, f64)> = cands
            .iter()
            .map(|&i| (lo(cells[i], cross), hi(cells[i], cross)))
            .collect();
        spans.sort_by(|a, b| f64::total_cmp(&a.0, &b.0));
        let mut covered = spans[0].0 <= lo(removed, cross) + ABUT_TOL;
        let mut reach = spans[0].1;
        for s in &spans[1..] {
            covered &= s.0 <= reach + gap + ABUT_TOL;
            reach = reach.max(s.1);
        }
        covered &= reach >= hi(removed, cross) - ABUT_TOL;
        if !covered {
            continue;
        }

        for i in cands {
            let c = cells[i];
            cells[i] = if before {
                with_span(c, axis, lo(c, axis), hi(removed, axis) - lo(c, axis))
            } else {
                with_span(c, axis, lo(removed, axis), hi(c, axis) - lo(removed, axis))
            };
        }
        break;
    }

    if cells.len() == 2 && overlaps_on(cells[0], cells[1], Axis::X) {
        let first_left = cells[0].loc.y <= cells[1].loc.y;
        let (l, r) = split_combined(old_box, FloatingSplitSide::Left, gap);
        cells = if first_left { vec![l, r] } else { vec![r, l] };
    }
    cells
}

/// Cells of the members left after `removed` was vacated, keeping the group: a single member
/// takes the whole old box, more members [`redistribute`].
pub fn collapse_cells(remaining: &[Rect], removed: Rect, gap: f64) -> Vec<Rect> {
    match remaining.len() {
        0 => Vec::new(),
        1 => vec![union_rect(remaining[0], removed)],
        _ => redistribute(remaining, removed, gap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GAP: f64 = 16.;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    fn pt(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }

    fn close(a: Rect, b: Rect) -> bool {
        (a.loc.x - b.loc.x).abs() < 0.01
            && (a.loc.y - b.loc.y).abs() < 0.01
            && (a.size.w - b.size.w).abs() < 0.01
            && (a.size.h - b.size.h).abs() < 0.01
    }

    fn assert_disjoint(cells: &[Rect]) {
        for (i, a) in cells.iter().enumerate() {
            for b in &cells[i + 1..] {
                assert!(!rects_overlap(*a, *b), "{a:?} overlaps {b:?}");
            }
        }
    }

    fn limits() -> GroupLimits {
        GroupLimits::from_band(rect(0., 0., 1280., 720.), GAP)
    }

    fn none_n(n: usize) -> Vec<CellLimits> {
        vec![CellLimits::none(); n]
    }

    fn rsz(cells: &[Rect], idx: usize, edges: ResizeEdge, d: Point<f64, Logical>) -> Vec<Rect> {
        resize_member(cells, &none_n(cells.len()), idx, edges, d, GAP, limits())
    }

    fn half(cells: &[Rect], idx: usize) -> Vec<Rect> {
        half_member(cells, &none_n(cells.len()), idx, GAP)
    }

    /// Pair side by side in a 416x316 box at (0, 0).
    fn pair() -> Vec<Rect> {
        let (l, r) = split_combined(rect(0., 0., 416., 316.), FloatingSplitSide::Left, GAP);
        vec![l, r]
    }

    #[test]
    fn lone_zones() {
        let t = rect(0., 0., 400., 200.);
        assert_eq!(lone_side_at(t, pt(200., 10.)), Some(FloatingSplitSide::Top));
        assert_eq!(
            lone_side_at(t, pt(200., 190.)),
            Some(FloatingSplitSide::Bottom)
        );
        assert_eq!(
            lone_side_at(t, pt(50., 100.)),
            Some(FloatingSplitSide::Left)
        );
        assert_eq!(
            lone_side_at(t, pt(300., 100.)),
            Some(FloatingSplitSide::Right)
        );
        let hint = lone_join_at(
            t,
            pt(300., 100.),
            GAP,
            CellLimits::none(),
            CellLimits::none(),
        )
        .unwrap();
        assert!(close(hint.hint, rect(208., 0., 192., 200.)), "{hint:?}");
        assert!(lone_join_at(
            t,
            pt(500., 100.),
            GAP,
            CellLimits::none(),
            CellLimits::none()
        )
        .is_none());
    }

    #[test]
    fn split_combined_occupies_box() {
        let (inc, tgt) = split_combined(rect(10., 20., 200., 100.), FloatingSplitSide::Left, 10.);
        assert!(close(inc, rect(10., 20., 95., 100.)));
        assert!(close(tgt, rect(115., 20., 95., 100.)));
        let (inc, tgt) = split_combined(rect(0., 0., 80., 200.), FloatingSplitSide::Top, 10.);
        assert!(close(inc, rect(0., 0., 80., 95.)));
        assert!(close(tgt, rect(0., 105., 80., 95.)));
    }

    #[test]
    fn third_via_bottom_of_one_side_splits_only_that_side() {
        let cells = pair();
        let left = cells[0];
        // Bottom band of the left cell, away from the center zone.
        let p = pt(left.loc.x + 20., left.loc.y + left.size.h - 10.);
        let (idx, hint) =
            group_join_at(&cells, &none_n(cells.len()), p, GAP, CellLimits::none()).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(hint.op, FloatingJoinOp::Split(FloatingSplitSide::Bottom));
        let (out, inc) = join_cells(
            &cells,
            &none_n(cells.len()),
            idx,
            hint.op,
            GAP,
            CellLimits::none(),
        )
        .unwrap();
        assert!(close(inc, hint.hint));
        assert!(close(out[0], rect(0., 0., 200., 150.)));
        assert!(close(inc, rect(0., 166., 200., 150.)));
        assert!(close(out[1], cells[1]), "other side keeps full height");
    }

    #[test]
    fn third_via_bottom_center_adds_full_width_strip() {
        let cells = pair();
        let bbox = union_all(&cells);
        let p = pt(bbox.loc.x + bbox.size.w / 2., bbox.loc.y + bbox.size.h - 5.);
        let (_, hint) =
            group_join_at(&cells, &none_n(cells.len()), p, GAP, CellLimits::none()).unwrap();
        assert_eq!(hint.op, FloatingJoinOp::BottomStrip);
        let (out, inc) = join_cells(
            &cells,
            &none_n(cells.len()),
            0,
            hint.op,
            GAP,
            CellLimits::none(),
        )
        .unwrap();
        assert!(close(inc, hint.hint));
        assert!(close(inc, rect(0., 166., 416., 150.)), "{inc:?}");
        assert!(close(out[0], rect(0., 0., 200., 150.)), "{:?}", out[0]);
        assert!(close(out[1], rect(216., 0., 200., 150.)), "{:?}", out[1]);
        assert_disjoint(&[out[0], out[1], inc]);
    }

    #[test]
    fn bottom_strip_on_split_side_gives_equal_rows() {
        let mut cells = pair();
        let (out, inc) = join_cells(
            &cells,
            &none_n(cells.len()),
            0,
            FloatingJoinOp::Split(FloatingSplitSide::Bottom),
            GAP,
            CellLimits::none(),
        )
        .unwrap();
        cells = out;
        cells.push(inc);
        let (out, strip) = add_bottom_strip(&cells, GAP);
        // 316 = 3 rows + 2 gaps → 94.67 each.
        let row = (316. - 2. * GAP) / 3.;
        assert!((strip.size.h - row).abs() < 0.01, "{strip:?}");
        assert!((out[0].size.h - row).abs() < 0.01, "{:?}", out[0]);
        assert!(
            (out[1].size.h - (2. * row + GAP)).abs() < 0.01,
            "{:?}",
            out[1]
        );
        let mut all = out;
        all.push(strip);
        assert_disjoint(&all);
    }

    #[test]
    fn fourth_makes_two_by_two_and_full_group_has_no_zones() {
        let cells = pair();
        let (mut cells, inc) = join_cells(
            &cells,
            &none_n(cells.len()),
            0,
            FloatingJoinOp::Split(FloatingSplitSide::Bottom),
            GAP,
            CellLimits::none(),
        )
        .unwrap();
        cells.push(inc);
        let right = cells[1];
        let p = pt(
            right.loc.x + right.size.w - 20.,
            right.loc.y + right.size.h - 10.,
        );
        let (idx, hint) =
            group_join_at(&cells, &none_n(cells.len()), p, GAP, CellLimits::none()).unwrap();
        assert_eq!(idx, 1);
        assert_eq!(hint.op, FloatingJoinOp::Split(FloatingSplitSide::Bottom));
        let (mut cells, inc) = join_cells(
            &cells,
            &none_n(cells.len()),
            idx,
            hint.op,
            GAP,
            CellLimits::none(),
        )
        .unwrap();
        cells.push(inc);
        assert!(close(cells[0], rect(0., 0., 200., 150.)));
        assert!(close(cells[1], rect(216., 0., 200., 150.)));
        assert!(close(cells[2], rect(0., 166., 200., 150.)));
        assert!(close(cells[3], rect(216., 166., 200., 150.)));
        assert_disjoint(&cells);

        for p in [pt(100., 75.), pt(208., 310.), pt(300., 300.)] {
            assert!(
                group_join_at(&cells, &none_n(cells.len()), p, GAP, CellLimits::none()).is_none()
            );
        }
    }

    #[test]
    fn inner_resize_moves_the_shared_boundary_only() {
        let cells = pair();
        let out = rsz(&cells, 0, ResizeEdge::RIGHT, pt(50., 0.));
        assert!(close(out[0], rect(0., 0., 250., 316.)));
        assert!(close(out[1], rect(266., 0., 150., 316.)), "{:?}", out[1]);
        assert!(close(union_all(&out), union_all(&cells)), "box stays");

        let out = rsz(&cells, 1, ResizeEdge::LEFT, pt(-30., 0.));
        assert!(close(out[1], rect(186., 0., 230., 316.)));
        assert!(close(out[0], rect(0., 0., 170., 316.)));

        // Stops at the minimum member size.
        let out = rsz(&cells, 0, ResizeEdge::RIGHT, pt(5000., 0.));
        assert!((out[1].size.w - MIN_MEMBER).abs() < 0.01, "{:?}", out[1]);
        assert!(close(union_all(&out), union_all(&cells)));
    }

    #[test]
    fn inner_resize_keeps_the_other_row() {
        let (top, strip) = add_bottom_strip(&pair(), GAP);
        let cells = vec![top[0], top[1], strip];
        let out = rsz(&cells, 0, ResizeEdge::BOTTOM, pt(0., 40.));
        assert!(close(out[0], rect(0., 0., 200., 190.)));
        assert!(close(out[1], rect(216., 0., 200., 190.)), "{:?}", out[1]);
        assert!(close(out[2], rect(0., 206., 416., 110.)), "{:?}", out[2]);
        assert_disjoint(&out);

        // Top row split: the strip below is not touched.
        let out = rsz(&cells, 0, ResizeEdge::RIGHT, pt(30., 0.));
        assert!(close(out[0], rect(0., 0., 230., 150.)));
        assert!(close(out[1], rect(246., 0., 170., 150.)));
        assert!(close(out[2], cells[2]));
    }

    #[test]
    fn outer_resize_scales_the_group_within_limits() {
        let lim = limits();
        assert!((lim.max.w - 1248.).abs() < 0.01);
        assert!((lim.max.h - 688.).abs() < 0.01);
        assert!((lim.min.w - 154.).abs() < 0.01);
        assert!((lim.min.h - 172.).abs() < 0.01);

        let cells = pair();
        let out = resize_member(
            &cells,
            &none_n(cells.len()),
            1,
            ResizeEdge::RIGHT,
            pt(100., 0.),
            GAP,
            lim,
        );
        assert!(close(union_all(&out), rect(0., 0., 516., 316.)));
        assert!((out[0].size.w - out[1].size.w).abs() < 0.01, "{out:?}");

        let out = resize_member(
            &cells,
            &none_n(cells.len()),
            0,
            ResizeEdge::LEFT,
            pt(-100., 0.),
            GAP,
            lim,
        );
        assert!(close(union_all(&out), rect(-100., 0., 516., 316.)));

        let out = resize_member(
            &cells,
            &none_n(cells.len()),
            1,
            ResizeEdge::RIGHT,
            pt(5000., 0.),
            GAP,
            lim,
        );
        assert!((union_all(&out).size.w - lim.max.w).abs() < 0.01);
        assert_disjoint(&out);

        let out = resize_member(
            &cells,
            &none_n(cells.len()),
            0,
            ResizeEdge::BOTTOM,
            pt(0., 5000.),
            GAP,
            lim,
        );
        assert!((union_all(&out).size.h - lim.max.h).abs() < 0.01);
        let out = resize_member(
            &cells,
            &none_n(cells.len()),
            0,
            ResizeEdge::BOTTOM,
            pt(0., -5000.),
            GAP,
            lim,
        );
        assert!((union_all(&out).size.h - lim.min.h).abs() < 0.01);

        // Corner: the inner edge moves the boundary, the outer one grows the box.
        let out = resize_member(
            &cells,
            &none_n(cells.len()),
            0,
            ResizeEdge::BOTTOM_RIGHT,
            pt(20., 40.),
            GAP,
            lim,
        );
        assert!(close(union_all(&out), rect(0., 0., 416., 356.)));
        assert!((out[0].size.w - 220.).abs() < 0.01, "{:?}", out[0]);
    }

    #[test]
    fn member_on_edges_picks_corner_cell() {
        let (top, strip) = add_bottom_strip(&pair(), GAP);
        let cells = vec![top[0], top[1], strip];
        assert_eq!(member_on_edges(&cells, ResizeEdge::BOTTOM), Some(2));
        assert_eq!(member_on_edges(&cells, ResizeEdge::LEFT), Some(0));
        assert_eq!(
            member_on_edges(&cells, ResizeEdge::LEFT | ResizeEdge::BOTTOM),
            Some(2)
        );
    }

    #[test]
    fn half_member_rules() {
        let half_w = (416. - GAP) / 2.;
        let half_h = (316. - GAP) / 2.;

        // Side by side: 50% width, full height kept.
        let cells = vec![rect(0., 0., 300., 316.), rect(316., 0., 100., 316.)];
        let out = half(&cells, 0);
        assert!(close(out[0], rect(0., 0., half_w, 316.)), "{out:?}");
        assert!(close(out[1], rect(half_w + GAP, 0., half_w, 316.)));

        // Taller cell above a short one: grows/shrinks to 50% height.
        let quad = vec![
            rect(0., 0., 300., 250.),
            rect(316., 0., 100., 250.),
            rect(0., 266., 300., 50.),
            rect(316., 266., 100., 50.),
        ];
        let out = half(&quad, 0);
        assert!(close(out[0], rect(0., 0., half_w, half_h)), "{:?}", out[0]);
        assert!((out[2].loc.y - (half_h + GAP)).abs() < 0.01, "{:?}", out[2]);
        assert!(close(union_all(&out), union_all(&quad)));
        assert_disjoint(&out);

        // Bottom row: grows upwards.
        let out = half(&quad, 3);
        assert!(
            close(out[3], rect(half_w + GAP, half_h + GAP, half_w, half_h)),
            "{:?}",
            out[3]
        );
        assert_disjoint(&out);

        // Full-width bottom strip: cut to 50% width, the right top cell fills the hole.
        let (top, strip) = add_bottom_strip(&pair(), GAP);
        let cells = vec![top[0], top[1], strip];
        let out = half(&cells, 2);
        assert!(
            close(out[2], rect(0., half_h + GAP, half_w, half_h)),
            "{out:?}"
        );
        assert!(
            close(out[1], rect(half_w + GAP, 0., half_w, 316.)),
            "{out:?}"
        );
        assert!(close(out[0], rect(0., 0., half_w, half_h)), "{out:?}");
        assert!(close(union_all(&out), union_all(&cells)));

        // Two stacked full-width cells become side by side halves.
        let (t, b) = split_combined(rect(0., 0., 416., 316.), FloatingSplitSide::Top, GAP);
        let out = half(&[t, b], 0);
        assert!(close(out[0], rect(0., 0., half_w, 316.)), "{out:?}");
        assert!(
            close(out[1], rect(half_w + GAP, 0., half_w, 316.)),
            "{out:?}"
        );
    }

    #[test]
    fn redistribute_after_leaving() {
        // 2x2 → remove top-left: bottom-left grows up.
        let quad = vec![
            rect(0., 0., 200., 150.),
            rect(216., 0., 200., 150.),
            rect(0., 166., 200., 150.),
            rect(216., 166., 200., 150.),
        ];
        let out = redistribute(&quad[1..], quad[0], GAP);
        assert!(close(out[0], quad[1]));
        assert!(close(out[1], rect(0., 0., 200., 316.)));
        assert!(close(out[2], quad[3]));

        // 3 → remove the full-width strip: top cells grow down.
        let three = vec![
            rect(0., 0., 200., 150.),
            rect(216., 0., 200., 150.),
            rect(0., 166., 416., 150.),
        ];
        let out = redistribute(&three[..2], three[2], GAP);
        assert!(close(out[0], rect(0., 0., 200., 316.)));
        assert!(close(out[1], rect(216., 0., 200., 316.)));

        // 3 → remove a top cell: two left are laid side by side in the old box.
        let out = redistribute(&three[1..], three[0], GAP);
        assert!(close(out[0], rect(0., 0., 200., 316.)), "{out:?}");
        assert!(close(out[1], rect(216., 0., 200., 316.)), "{out:?}");

        // Split side → remove its bottom: the sibling grows down.
        let split = vec![
            rect(0., 0., 200., 150.),
            rect(216., 0., 200., 316.),
            rect(0., 166., 200., 150.),
        ];
        let out = redistribute(&split[..2], split[2], GAP);
        assert!(close(out[0], rect(0., 0., 200., 316.)));
        assert!(close(out[1], split[1]));

        // One left: nothing to do (caller dissolves).
        let out = redistribute(&split[..1], split[1], GAP);
        assert_eq!(out.len(), 1);

        // Collapse keeps a lone member in the group: it takes the whole box.
        let p = pair();
        let out = collapse_cells(&p[..1], p[1], GAP);
        assert!(close(out[0], union_all(&p)));
        let out = collapse_cells(&three[1..], three[0], GAP);
        assert!(close(out[0], rect(0., 0., 200., 316.)));
    }

    #[test]
    fn scale_cells_keeps_proportions_and_gaps() {
        let cells = vec![
            rect(100., 50., 200., 150.),
            rect(316., 50., 200., 150.),
            rect(100., 216., 416., 150.),
        ];
        let from = union_all(&cells);
        let to = rect(100., 16., 1248., 688.);
        let out = scale_cells(&cells, from, to, GAP);
        assert!(close(union_all(&out), to));
        assert!((out[0].size.w - out[1].size.w).abs() < 0.01);
        assert!((out[1].loc.x - (out[0].loc.x + out[0].size.w) - GAP).abs() < 0.01);
        assert!((out[2].loc.y - (out[0].loc.y + out[0].size.h) - GAP).abs() < 0.01);
        assert!((out[0].size.h - out[2].size.h).abs() < 0.01);
        assert_disjoint(&out);
    }

    #[test]
    fn split_fit_honors_mins_and_refuses_when_tight() {
        let box_ = rect(0., 0., 300., 200.);
        let lim = |w, h| CellLimits {
            min: Size::from((w, h)),
            max: Size::from((f64::INFINITY, f64::INFINITY)),
        };
        // Incoming needs 200: gets 200, target gets the rest.
        let (inc, tgt) = split_fit(
            box_,
            FloatingSplitSide::Left,
            GAP,
            lim(200., 0.),
            CellLimits::none(),
        )
        .unwrap();
        assert!((inc.size.w - 200.).abs() < 0.01, "{inc:?}");
        assert!((tgt.size.w - (300. - GAP - 200.)).abs() < 0.01, "{tgt:?}");

        // Both need 200 in a 300 box: refuse.
        assert!(split_fit(
            box_,
            FloatingSplitSide::Left,
            GAP,
            lim(200., 0.),
            lim(200., 0.),
        )
        .is_none());

        let hint = lone_join_at(box_, pt(50., 100.), GAP, lim(200., 0.), lim(200., 0.)).unwrap();
        assert!(!hint.fits);
    }
}
