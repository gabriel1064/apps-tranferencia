use std::{
    cmp::{max, min},
    iter::{self, zip},
    rc::Rc,
    time::Duration,
};

use niri_config::{utils::MergeWith as _, CenterFocusedColumn, PresetSize, Struts};
use niri_ipc::{ColumnDisplay, SizeChange, WindowLayout};
use ordered_float::NotNan;
use smithay::{
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Point, Rectangle, Scale, Serial, Size},
};

use super::{
    closing_window::{ClosingWindow, ClosingWindowRenderElement},
    floating_group::{
        cell_limits_of, collapse_cells, group_join_at, half_member, join_cells, member_on_edges,
        redistribute, resize_member, scale_cells, union_all, CellLimits, FloatingJoinHint,
        FloatingJoinOp, GroupLimits, MAX_GROUP_MEMBERS,
    },
    group_chrome::{
        bottom_bar_w, group_extra, hit_at as group_bar_hit_at, side_bar_rect, GroupBarHit,
        GroupChrome, GroupChromeLayer,
    },
    monitor::InsertPosition,
    tab_indicator::{TabIndicator, TabIndicatorRenderElement, TabInfo},
    tile::{Tile, TileRenderElement, TileRenderSnapshot},
    workspace::{InteractiveResize, ResolvedSize},
    ConfigureIntent, HitType, InteractiveResizeData, LayoutElement, Options, RemovedTile,
};
use crate::{
    animation::{Animation, Clock},
    input::swipe_tracker::SwipeTracker,
    layout::SizingMode,
    niri_render_elements,
    render_helpers::{renderer::NiriRenderer, xray::XrayPos, RenderCtx},
    utils::{
        transaction::{Transaction, TransactionBlocker},
        ResizeEdge,
    },
    window::ResolvedWindowRules,
};

/// Amount of touchpad movement to scroll the view for the width of one working area.
const VIEW_GESTURE_WORKING_AREA_MOVEMENT: f64 = 1200.;

/// The scrolling strip may grow to at most this many working-area widths.
const CAP_VIEWPORTS: f64 = 4.;
/// A split column may not shrink below this fraction of a viewport.
const MIN_COL_VIEWPORTS: f64 = 0.25;
/// Horizontal split is allowed down to this fraction so a 50% column can still
/// be halved (~0.24 viewport each after gaps).
const MIN_SPLIT_HALF_VIEWPORTS: f64 = 0.15;
/// Top/bottom band of the working area that prefers stacking over a side split.
const STACK_BAND_FRAC: f64 = 0.2;
/// Lateral edge of a column treated as a gutter for a new column when leftover exists.
const NEW_COL_EDGE_FRAC: f64 = 0.15;
const NEW_COL_EDGE_MIN: f64 = 24.;
const NEW_COL_EDGE_MAX: f64 = 80.;

/// Wheel notches closer than this (same direction) ramp the pan gain up.
const WHEEL_ACCEL_WINDOW: Duration = Duration::from_millis(90);
const WHEEL_ACCEL_STEP: f64 = 1.22;
const WHEEL_ACCEL_MAX: f64 = 3.;
/// Top pan speed in logical px/s (a flick at this speed glides ~3.7k px).
const WHEEL_MAX_VELOCITY: f64 = 11000.;
/// Deceleration when braking (leaving the strip upward). The normal glide uses
/// 0.997; lower bleeds speed faster. At top speed this stops in ~0.8 s / ~1.1k px.
const WHEEL_BRAKE_RATE: f64 = 0.990;
const WHEEL_COAST_RATE: f64 = 0.997;
/// Edge-scroll gain starts here and ramps toward EDGE_GAIN_MAX (left and right).
const EDGE_GAIN_START: f64 = 0.4;
const EDGE_GAIN_PER_SEC: f64 = 1.2;
const EDGE_GAIN_MAX: f64 = 2.0;
/// How fast edge-scroll velocity eases toward its target (1/s).
const EDGE_APPROACH_PER_SEC: f64 = 9.;
/// Extra inter-column gap while a tiled window is being dragged.
/// Slightly larger than the idle gap so drop targets are easier to hit; not a
/// window-width slot.
pub(super) const DRAG_COL_GAP_EXTRA: f64 = 40.;

/// A scrollable-tiling space for windows.
#[derive(Debug)]
pub struct ScrollingSpace<W: LayoutElement> {
    /// Columns of windows on this space.
    columns: Vec<Column<W>>,

    /// Extra per-column data.
    data: Vec<ColumnData>,

    /// Index of the currently active column, if any.
    active_column_idx: usize,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Group cells at the start of an interactive resize of a tiled group member.
    group_resize_start: Option<(W::Id, Vec<Rectangle<f64, Logical>>)>,

    /// Offset of the view computed from the active column.
    ///
    /// Any gaps, including left padding from work area left exclusive zone, is handled
    /// with this view offset (rather than added as a constant elsewhere in the code). This allows
    /// for natural handling of fullscreen windows, which must ignore work area padding.
    view_offset: ViewOffset,

    /// Whether to activate the previous, rather than the next, column upon column removal.
    ///
    /// When a new column is created and removed with no focus changes in-between, it is more
    /// natural to activate the previously-focused column. This variable tracks that.
    ///
    /// Since we only create-and-activate columns immediately to the right of the active column (in
    /// contrast to tabs in Firefox, for example), we can track this as a bool, rather than an
    /// index of the previous column to activate.
    ///
    /// The value is the view offset that the previous column had before, to restore it.
    activate_prev_column_on_removal: Option<f64>,

    /// View offset to restore after unfullscreening or unmaximizing.
    view_offset_to_restore: Option<f64>,

    /// The user panned the camera (wheel or touchpad). Stop fitting the
    /// focused column back into view; that animation fights the pan.
    user_panned_view: bool,

    /// Last wheel notch: (time, direction sign, current gain).
    wheel_accel: Option<(Duration, f64, f64)>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    ///
    /// Takes into account layer-shell exclusive zones and niri struts.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this space excluding struts.
    ///
    /// Used for popup unconstraining. Popups can go over struts, but they shouldn't go over
    /// the layer-shell top layer (which renders on top of popups).
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,

    /// Extra gap reserved for a live-reflow interactive move (origin hole or leftover column).
    live_reflow_gap: Option<LiveReflowGap>,
    /// Drop dest while dragging. Overlay-only dests (split/stack) live here so existing
    /// windows keep their size until drop; leftover empty-column dests also move the layout.
    drop_dest: Option<LiveReflowGap>,

    /// Slightly larger column gaps while a tiled window is being dragged.
    drag_spread: bool,

    /// Virtual empty strip width added before the first column while a tiled
    /// window is edge-dragged left (toward `cap_width`). Cleared on drag end.
    drag_edge_left: f64,
    /// Virtual empty strip width after the last column while edge-dragging right.
    drag_edge_right: f64,
    /// Per-side cap on edge-grow: just enough room to drop the dragged window
    /// (its width plus a gap) past the first or last column.
    drag_edge_limit: f64,

    /// Next id to assign when splitting a column horizontally.
    next_split_id: u64,
}

niri_render_elements! {
    ScrollingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        ClosingWindow = ClosingWindowRenderElement,
        TabIndicator = TabIndicatorRenderElement,
        GroupLayer = GroupChromeLayer,
    }
}

/// Extra per-column data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnData {
    /// Cached actual column width.
    width: f64,
}

#[derive(Debug)]
pub(super) enum ViewOffset {
    /// The view offset is static.
    Static(f64),
    /// The view offset is animating.
    Animation(Animation),
    /// The view offset is controlled by the ongoing gesture.
    Gesture(ViewGesture),
}

#[derive(Debug)]
pub(super) struct ViewGesture {
    current_view_offset: f64,
    /// Animation for the extra offset to the current position.
    ///
    /// For example, when we need to activate a specific window during a DnD scroll.
    animation: Option<Animation>,
    tracker: SwipeTracker,
    delta_from_tracker: f64,
    // The view offset we'll use if needed for activate_prev_column_on_removal.
    stationary_view_offset: f64,
    /// Whether the gesture is controlled by the touchpad.
    is_touchpad: bool,

    // If this gesture is for drag-and-drop scrolling, this is the last event's unadjusted
    // timestamp.
    dnd_last_event_time: Option<Duration>,
    // Time when the drag-and-drop scroll delta became non-zero, used for debouncing.
    //
    // If `None` then the scroll delta is currently zero.
    dnd_nonzero_start_time: Option<Duration>,
    /// Horizontal view velocity (px/s) while dragging: edge scroll + wheel coast.
    dnd_velocity: f64,
    /// Speed multiplier ramping up while the pointer stays in a screen-edge zone.
    dnd_edge_gain: f64,
    /// After leaving the screen-edge zone, bleed speed with WHEEL_BRAKE_RATE.
    dnd_braking: bool,
}

#[derive(Debug)]
pub struct Column<W: LayoutElement> {
    /// Tiles in this column.
    ///
    /// Must be non-empty.
    tiles: Vec<Tile<W>>,

    /// Extra per-tile data.
    ///
    /// Must have the same number of elements as `tiles`.
    data: Vec<TileData>,

    /// Index of the currently active tile.
    active_tile_idx: usize,

    /// Desired width of this column.
    ///
    /// If the column is full-width or full-screened, this is the width that should be restored
    /// upon unfullscreening and untoggling full-width.
    width: ColumnWidth,

    /// Currently selected preset width index.
    preset_width_idx: Option<usize>,

    /// Whether this column is full-width.
    is_full_width: bool,

    /// Whether this column is going to be fullscreen.
    ///
    /// This is the compositor-side fullscreen state, so it changes immediately upon
    /// set_fullscreen(). The actual tiles will take some time to respond to the fullscreen request
    /// and become fullscreen.
    ///
    /// Similarly, unsetting fullscreen will change this value to false immediately, and tiles will
    /// take some time to catch up and actually unfullscreen.
    is_pending_fullscreen: bool,

    /// Whether this column is going to be maximized.
    ///
    /// Can be `true` together with `is_pending_fullscreen`, which means that the column is
    /// effectively pending fullscreen, but unfullscreening should go back to maximized state,
    /// rather than normal.
    is_pending_maximized: bool,

    /// How this column displays and arranges windows.
    display_mode: ColumnDisplay,

    /// Tab indicator for the tabbed display mode.
    tab_indicator: TabIndicator,

    /// Animation of the render offset during window swapping.
    move_animation: Option<MoveAnimation>,

    /// Latest known view size for this column's workspace.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this column's workspace.
    working_area: Rectangle<f64, Logical>,

    /// Working area for this column's workspace excluding struts.
    ///
    /// Used for maximize-to-edges.
    parent_area: Rectangle<f64, Logical>,

    /// Scale of the output the column is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,

    /// Extra vertical gap reserved while live-reflowing an in-column insert.
    ///
    /// `(tile_index, height)` inserts space before `tile_index` (or after the last tile when
    /// `tile_index == tiles.len()`).
    live_reflow_tile_gap: Option<(usize, f64)>,

    /// Target layout heights for existing tiles during in-column live-reflow (split preview).
    live_reflow_tile_heights: Option<Vec<f64>>,

    /// Layout width while live-reflow is squeezing this column to make room.
    live_reflow_width: Option<f64>,
    /// Unsqueezed layout width, restored when the squeeze ends.
    live_reflow_orig_width: Option<f64>,

    /// Pixel width this column should grow back to when space is freed.
    natural_width: f64,
    /// Pair id shared with the column created by the last horizontal split, if any.
    split_id: Option<u64>,
    /// Combined width of this column and its split partner before the split.
    split_pair_width: f64,
    /// Recency of the last shrink, used when restoring leftover space.
    split_seq: u64,

    /// Set when this column is a window group: a side bar plus up to four cells.
    group: Option<TiledGroup>,
}

/// Window group laid out in one scrolling column.
#[derive(Debug)]
struct TiledGroup {
    /// Member cells relative to the content origin (right of the bar, at the tiles origin),
    /// parallel to `Column::tiles`. Their union starts at (0, 0).
    cells: Vec<Rectangle<f64, Logical>>,
    chrome: GroupChrome,
    /// Column width before a group Fullscreen, restored by the next one.
    restore_width: Option<(ColumnWidth, bool)>,
    /// Empty cell of a member picked up with a deferred ungroup; the group keeps its box until
    /// the pickup resolves.
    reserved: Option<Rectangle<f64, Logical>>,
}

impl TiledGroup {
    fn new(cells: Vec<Rectangle<f64, Logical>>) -> Self {
        Self {
            cells,
            chrome: GroupChrome::new(),
            restore_width: None,
            reserved: None,
        }
    }

    /// Member cells plus the reserved cell.
    fn all_cells(&self) -> Vec<Rectangle<f64, Logical>> {
        let mut cells = self.cells.clone();
        cells.extend(self.reserved);
        cells
    }

    fn bbox(&self) -> Rectangle<f64, Logical> {
        union_all(&self.all_cells())
    }

    /// Replace all cells (members then the reserved one), shifted so their union starts at
    /// (0, 0).
    fn set_all_cells(&mut self, cells: Vec<Rectangle<f64, Logical>>) {
        let mut cells = normalize_cells(cells);
        self.reserved = self.reserved.is_some().then(|| cells.pop()).flatten();
        self.cells = cells;
    }

    /// Cells a join starts from: the members grown into the reserved cell.
    fn join_base_cells(&self, gap: f64) -> Vec<Rectangle<f64, Logical>> {
        match self.reserved {
            Some(reserved) => collapse_cells(&self.cells, reserved, gap),
            None => self.cells.clone(),
        }
    }
}

/// Shift `cells` so their union starts at (0, 0).
fn normalize_cells(cells: Vec<Rectangle<f64, Logical>>) -> Vec<Rectangle<f64, Logical>> {
    if cells.is_empty() {
        return cells;
    }
    let origin = union_all(&cells).loc;
    cells
        .into_iter()
        .map(|mut c| {
            c.loc -= origin;
            c
        })
        .collect()
}

/// Extra per-tile data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TileData {
    /// Requested height of the window.
    ///
    /// This is window height, not tile height, so it excludes tile decorations.
    height: WindowHeight,

    /// Cached actual size of the tile.
    size: Size<f64, Logical>,

    /// Cached whether the tile is being interactively resized by its left edge.
    interactively_resizing_by_left_edge: bool,
}

/// Width of a column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    /// Proportion of the current view width.
    Proportion(f64),
    /// Fixed width in logical pixels.
    Fixed(f64),
}

enum CappedPlace {
    NewColumn {
        idx: usize,
        width: ColumnWidth,
    },
    Split {
        col_idx: usize,
        insert_left: bool,
        half: f64,
    },
    Stack {
        col_idx: usize,
        tile_idx: usize,
        above: bool,
    },
}

struct SplitTarget {
    col_idx: usize,
    tile_idx: usize,
    left: bool,
    above: bool,
}

/// Height of a window in a column.
///
/// Every window but one in a column must be `Auto`-sized so that the total height can add up to
/// the workspace height. Resizing a window converts all other windows to `Auto`, weighted to
/// preserve their visual heights at the moment of the conversion.
///
/// In contrast to column widths, proportional height changes are converted to, and stored as,
/// fixed height right away. With column widths you frequently want e.g. two columns side-by-side
/// with 50% width each, and you want them to remain this way when moving to a differently sized
/// monitor. Windows in a column, however, already auto-size to fill the available height, giving
/// you this behavior. The main reason to set a different window height, then, is when you want
/// something in the window to fit exactly, e.g. to fit 30 lines in a terminal, which corresponds
/// to the `Fixed` variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WindowHeight {
    /// Automatically computed *tile* height, distributed across the column according to weights.
    ///
    /// This controls the tile height rather than the window height because it's easier in the auto
    /// height distribution algorithm.
    Auto { weight: f64 },
    /// Fixed *window* height in logical pixels.
    Fixed(f64),
    /// One of the preset heights (tile or window).
    Preset(usize),
}

/// Horizontal direction for an operation.
///
/// As operations often have a symmetrical counterpart, e.g. focus-right/focus-left, methods
/// on `Scrolling` can sometimes be factored using the direction of the operation as a parameter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScrollDirection {
    Left,
    Right,
}

#[derive(Debug)]
struct MoveAnimation {
    anim: Animation,
    from: f64,
}

/// Placeholder gap opened in the remaining layout during an interactive tiled move.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct LiveReflowGap {
    pub position: InsertPosition,
    pub width: f64,
    pub height: f64,
    /// Column squeezed to share space with the moving window: (index, display width).
    pub squeeze: Option<(usize, f64)>,
    /// Reserved origin slot. Drop restores the window instead of inserting.
    pub origin: bool,
}

impl LiveReflowGap {
    /// Column whose real drop zones should be outlined while hovering it.
    pub(super) fn overlay_column_idx(self) -> Option<usize> {
        if self.origin {
            return None;
        }
        if let Some((col, _)) = self.squeeze {
            return Some(col);
        }
        match self.position {
            InsertPosition::InColumn(col, _) => Some(col),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum InsertColumnSlot {
    Real(usize),
    Virtual(usize),
}

impl<W: LayoutElement> ScrollingSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        Self {
            columns: Vec::new(),
            data: Vec::new(),
            active_column_idx: 0,
            interactive_resize: None,
            group_resize_start: None,
            view_offset: ViewOffset::Static(0.),
            activate_prev_column_on_removal: None,
            view_offset_to_restore: None,
            user_panned_view: false,
            wheel_accel: None,
            closing_windows: Vec::new(),
            view_size,
            working_area,
            parent_area,
            scale,
            clock,
            options,
            live_reflow_gap: None,
            drop_dest: None,
            drag_spread: false,
            drag_edge_left: 0.,
            drag_edge_right: 0.,
            drag_edge_limit: f64::INFINITY,
            next_split_id: 1,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let working_area = compute_working_area(parent_area, scale, options.layout.struts);

        for (column, data) in zip(&mut self.columns, &mut self.data) {
            column.update_config(view_size, working_area, parent_area, scale, options.clone());
            data.update(column);
        }

        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        // Apply always-center and such right away.
        if !self.columns.is_empty() && !self.view_offset.is_gesture() && !self.user_panned_view {
            self.animate_view_offset_to_column(None, self.active_column_idx, None);
        }
    }

    pub fn update_shaders(&mut self) {
        for col in &mut self.columns {
            col.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        if let ViewOffset::Animation(anim) = &self.view_offset {
            if anim.is_done() {
                self.view_offset = ViewOffset::Static(anim.to());
            }
        }

        // A wheel glide runs at full speed into the edge and stops dead there.
        if self.user_panned_view {
            if let ViewOffset::Animation(anim) = &self.view_offset {
                let vel = anim.velocity();
                if vel != 0. {
                    if let Some((lo, hi)) = self.free_pan_offset_bounds() {
                        let cur = self.view_offset.current();
                        if (vel > 0. && cur >= hi) || (vel < 0. && cur <= lo) {
                            self.view_offset = ViewOffset::Static(cur.clamp(lo, hi));
                            self.wheel_accel = None;
                        }
                    }
                }
            }
        }

        // Content shrank under a free-panned view: glide back inside the limits.
        if self.user_panned_view && matches!(self.view_offset, ViewOffset::Static(_)) {
            if let Some((lo, hi)) = self.free_pan_offset_bounds() {
                let cur = self.view_offset.current();
                let to = cur.clamp(lo, hi);
                if (to - cur).abs() > 0.5 {
                    self.view_offset = ViewOffset::Animation(Animation::new(
                        self.clock.clone(),
                        cur,
                        to,
                        0.,
                        self.options.animations.horizontal_view_movement.0,
                    ));
                }
            }
        }

        if let ViewOffset::Gesture(gesture) = &mut self.view_offset {
            // Make sure the last event time doesn't go too much out of date (for
            // workspaces not under cursor), causing sudden jumps.
            //
            // This happens after any dnd_scroll_gesture_scroll() calls (in
            // Layout::advance_animations()), so it doesn't mess up the time delta there.
            if let Some(last_time) = &mut gesture.dnd_last_event_time {
                let now = self.clock.now_unadjusted();
                if *last_time != now {
                    *last_time = now;

                    // If last_time was already == now, then dnd_scroll_gesture_scroll() must've
                    // updated the gesture already. Therefore, when this code runs, the pointer
                    // must be outside the DnD scrolling zone.
                    gesture.dnd_nonzero_start_time = None;
                }
            }

            if let Some(anim) = &mut gesture.animation {
                if anim.is_done() {
                    gesture.animation = None;
                }
            }
        }

        for col in &mut self.columns {
            col.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.view_offset.is_animation_ongoing()
            || self.columns.iter().any(Column::are_animations_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        !self.view_offset.is_static()
            || self.columns.iter().any(Column::are_transitions_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        let view_pos = Point::from((self.view_pos(), 0.));
        let view_size = self.view_size;
        let active_idx = self.active_column_idx;
        for (col_idx, (col, col_x)) in self.columns_mut().enumerate() {
            let is_active = is_active && col_idx == active_idx;
            let col_off = Point::from((col_x, 0.));
            let col_pos = view_pos - col_off - col.render_offset();
            let view_rect = Rectangle::new(col_pos, view_size);
            for tile in col.tiles.iter_mut() {
                tile.set_floating(false);
            }
            col.update_render_elements(is_active, view_rect);
        }
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.columns.iter().flat_map(|col| col.tiles.iter())
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.columns.iter_mut().flat_map(|col| col.tiles.iter_mut())
    }

    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.columns.is_empty() {
            return None;
        }

        let col = &self.columns[self.active_column_idx];
        Some(col.tiles[col.active_tile_idx].window())
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.columns.is_empty() {
            return None;
        }

        let col = &mut self.columns[self.active_column_idx];
        Some(col.tiles[col.active_tile_idx].window_mut())
    }

    pub fn active_tile_mut(&mut self) -> Option<&mut Tile<W>> {
        if self.columns.is_empty() {
            return None;
        }

        let col = &mut self.columns[self.active_column_idx];
        Some(&mut col.tiles[col.active_tile_idx])
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        let col = &self.columns[self.active_column_idx];
        col.pending_sizing_mode().is_fullscreen()
    }

    pub fn new_window_toplevel_bounds(&self, rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let border_config = self.options.layout.border.merged_with(&rules.border);

        let display_mode = rules
            .default_column_display
            .unwrap_or(self.options.layout.default_column_display);
        let will_tab = display_mode == ColumnDisplay::Tabbed;
        let extra_size = if will_tab {
            TabIndicator::new(self.options.layout.tab_indicator).extra_size(1, self.scale)
        } else {
            Size::from((0., 0.))
        };

        compute_toplevel_bounds(
            border_config,
            self.working_area.size,
            extra_size,
            self.options.layout.gaps,
        )
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = self.options.layout.border.merged_with(&rules.border);

        let display_mode = rules
            .default_column_display
            .unwrap_or(self.options.layout.default_column_display);
        let will_tab = display_mode == ColumnDisplay::Tabbed;
        let extra = if will_tab {
            TabIndicator::new(self.options.layout.tab_indicator).extra_size(1, self.scale)
        } else {
            Size::from((0., 0.))
        };

        let working_size = self.working_area.size;

        let width = if let Some(size) = width {
            let size = match resolve_preset_size(size, &self.options, working_size.w, extra.w) {
                ResolvedSize::Tile(mut size) => {
                    if !border.off {
                        size -= border.width * 2.;
                    }
                    size
                }
                ResolvedSize::Window(size) => size,
            };

            max(1, size.floor() as i32)
        } else {
            0
        };

        let mut full_height = self.working_area.size.h - self.options.layout.gaps * 2.;
        if !border.off {
            full_height -= border.width * 2.;
        }

        let height = if let Some(height) = height {
            let height = match resolve_preset_size(height, &self.options, working_size.h, extra.h) {
                ResolvedSize::Tile(mut size) => {
                    if !border.off {
                        size -= border.width * 2.;
                    }
                    size
                }
                ResolvedSize::Window(size) => size,
            };
            f64::min(height, full_height)
        } else {
            full_height
        };

        Size::from((width, max(height.floor() as i32, 1)))
    }

    pub fn is_centering_focused_column(&self) -> bool {
        self.options.layout.center_focused_column == CenterFocusedColumn::Always
            || (self.options.layout.always_center_single_column && self.columns.len() <= 1)
    }

    fn compute_new_view_offset_fit(
        &self,
        target_x: Option<f64>,
        col_x: f64,
        width: f64,
        mode: SizingMode,
    ) -> f64 {
        if mode.is_fullscreen() {
            return 0.;
        }

        let (area, padding) = if mode.is_maximized() {
            (self.parent_area, 0.)
        } else {
            (self.working_area, self.options.layout.gaps)
        };

        let target_x = target_x.unwrap_or_else(|| self.target_view_pos());

        let new_offset =
            compute_new_view_offset(target_x + area.loc.x, area.size.w, col_x, width, padding);

        // Non-fullscreen windows are always offset at least by the working area position.
        new_offset - area.loc.x
    }

    fn compute_new_view_offset_centered(
        &self,
        target_x: Option<f64>,
        col_x: f64,
        width: f64,
        mode: SizingMode,
    ) -> f64 {
        if mode.is_fullscreen() {
            return self.compute_new_view_offset_fit(target_x, col_x, width, mode);
        }

        let area = if mode.is_maximized() {
            self.parent_area
        } else {
            self.working_area
        };

        // Columns wider than the view are left-aligned (the fit code can deal with that).
        if area.size.w <= width {
            return self.compute_new_view_offset_fit(target_x, col_x, width, mode);
        }

        -(area.size.w - width) / 2. - area.loc.x
    }

    fn compute_new_view_offset_for_column_fit(&self, target_x: Option<f64>, idx: usize) -> f64 {
        let col = &self.columns[idx];
        self.compute_new_view_offset_fit(
            target_x,
            self.column_x(idx),
            col.width(),
            col.sizing_mode(),
        )
    }

    fn compute_new_view_offset_for_column_centered(
        &self,
        target_x: Option<f64>,
        idx: usize,
    ) -> f64 {
        let col = &self.columns[idx];
        self.compute_new_view_offset_centered(
            target_x,
            self.column_x(idx),
            col.width(),
            col.sizing_mode(),
        )
    }

    fn compute_new_view_offset_for_column(
        &self,
        target_x: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
    ) -> f64 {
        if self.is_centering_focused_column() {
            return self.compute_new_view_offset_for_column_centered(target_x, idx);
        }

        match self.options.layout.center_focused_column {
            CenterFocusedColumn::Always => {
                self.compute_new_view_offset_for_column_centered(target_x, idx)
            }
            CenterFocusedColumn::OnOverflow => {
                let Some(prev_idx) = prev_idx else {
                    return self.compute_new_view_offset_for_column_fit(target_x, idx);
                };

                // Activating the same column.
                if prev_idx == idx {
                    return self.compute_new_view_offset_for_column_fit(target_x, idx);
                }

                // Always take the left or right neighbor of the target as the source.
                let source_idx = if prev_idx > idx {
                    min(idx + 1, self.columns.len() - 1)
                } else {
                    idx.saturating_sub(1)
                };

                let source_col_x = self.column_x(source_idx);
                let source_col_width = self.columns[source_idx].width();

                let target_col_x = self.column_x(idx);
                let target_col_width = self.columns[idx].width();

                // NOTE: This logic won't work entirely correctly with small fixed-size maximized
                // windows (they have a different area and padding).
                let total_width = if source_col_x < target_col_x {
                    // Source is left from target.
                    target_col_x - source_col_x + target_col_width
                } else {
                    // Source is right from target.
                    source_col_x - target_col_x + source_col_width
                } + self.options.layout.gaps * 2.;

                // If it fits together, do a normal animation, otherwise center the new column.
                if total_width <= self.working_area.size.w {
                    self.compute_new_view_offset_for_column_fit(target_x, idx)
                } else {
                    self.compute_new_view_offset_for_column_centered(target_x, idx)
                }
            }
            CenterFocusedColumn::Never => {
                self.compute_new_view_offset_for_column_fit(target_x, idx)
            }
        }
    }

    fn animate_view_offset(&mut self, idx: usize, new_view_offset: f64) {
        self.animate_view_offset_with_config(
            idx,
            new_view_offset,
            self.options.animations.horizontal_view_movement.0,
        );
    }

    fn animate_view_offset_with_config(
        &mut self,
        idx: usize,
        new_view_offset: f64,
        config: niri_config::Animation,
    ) {
        let new_col_x = self.column_x(idx);
        let old_col_x = self.column_x(self.active_column_idx);
        let offset_delta = old_col_x - new_col_x;
        self.view_offset.offset(offset_delta);

        // Never pull past the strip edge (e.g. centering the first/last column
        // at the 4-viewport cap). Keep that window flush with the screen.
        let new_view_offset = match self.free_pan_offset_bounds_for(idx) {
            Some((lo, hi)) => new_view_offset.clamp(lo, hi),
            None => new_view_offset,
        };

        let pixel = 1. / self.scale;

        // If our view offset is already this or animating towards this, we don't need to do
        // anything.
        let to_diff = new_view_offset - self.view_offset.target();
        if to_diff.abs() < pixel {
            // Correct for any inaccuracy.
            self.view_offset.offset(to_diff);
            return;
        }

        match &mut self.view_offset {
            ViewOffset::Gesture(gesture) if gesture.dnd_last_event_time.is_some() => {
                gesture.stationary_view_offset = new_view_offset;

                let current_pos = gesture.current_view_offset - gesture.delta_from_tracker;
                gesture.delta_from_tracker = new_view_offset - current_pos;
                let offset_delta = new_view_offset - gesture.current_view_offset;
                gesture.current_view_offset = new_view_offset;

                gesture.animate_from(-offset_delta, self.clock.clone(), config);
            }
            _ => {
                // FIXME: also compute and use current velocity.
                self.view_offset = ViewOffset::Animation(Animation::new(
                    self.clock.clone(),
                    self.view_offset.current(),
                    new_view_offset,
                    0.,
                    config,
                ));
            }
        }
    }

    fn animate_view_offset_to_column_centered(
        &mut self,
        target_x: Option<f64>,
        idx: usize,
        config: niri_config::Animation,
    ) {
        let new_view_offset = self.compute_new_view_offset_for_column_centered(target_x, idx);
        self.animate_view_offset_with_config(idx, new_view_offset, config);
    }

    fn animate_view_offset_to_column_with_config(
        &mut self,
        target_x: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
        config: niri_config::Animation,
    ) {
        let new_view_offset = self.compute_new_view_offset_for_column(target_x, idx, prev_idx);
        self.animate_view_offset_with_config(idx, new_view_offset, config);
    }

    fn animate_view_offset_to_column(
        &mut self,
        target_x: Option<f64>,
        idx: usize,
        prev_idx: Option<usize>,
    ) {
        self.animate_view_offset_to_column_with_config(
            target_x,
            idx,
            prev_idx,
            self.options.animations.horizontal_view_movement.0,
        )
    }

    fn activate_column(&mut self, idx: usize) {
        self.activate_column_with_anim_config(
            idx,
            self.options.animations.horizontal_view_movement.0,
        );
    }

    fn activate_column_with_anim_config(&mut self, idx: usize, _config: niri_config::Animation) {
        // Focusing a column must not move the camera. Offset is relative to the
        // active column, so rewrite it to keep view_pos() visually unchanged.
        if self.active_column_idx == idx {
            return;
        }

        let view_pos = self.view_pos();
        self.active_column_idx = idx;
        let new_off = view_pos - self.column_x(idx);
        self.view_offset = ViewOffset::Static(new_off);
        self.activate_prev_column_on_removal = None;
        self.view_offset_to_restore = None;
        self.interactive_resize = None;
    }

    pub(super) fn insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        if self.columns.is_empty() {
            return InsertPosition::NewColumn(0);
        }

        let x = pos.x + self.view_pos();

        // Aim for the center of the gap.
        let x = x + self.options.layout.gaps / 2.;
        let y = pos.y + self.options.layout.gaps / 2.;

        // Insert position is before the first column.
        if x < 0. {
            return InsertPosition::NewColumn(0);
        }

        let (starts, slots) = self.insert_column_slots();

        // Find the closest gap between columns (including a live-reflow virtual column).
        let (closest_idx, col_x) = starts
            .iter()
            .copied()
            .enumerate()
            .min_by_key(|(_, col_x)| NotNan::new((col_x - x).abs()).unwrap())
            .unwrap();

        let closest_new_column = match slots.get(closest_idx) {
            Some(InsertColumnSlot::Virtual(idx) | InsertColumnSlot::Real(idx)) => {
                InsertPosition::NewColumn(*idx)
            }
            None => InsertPosition::NewColumn(self.columns.len()),
        };

        // Find the column containing the position.
        let (contain_idx, _) = starts
            .iter()
            .copied()
            .enumerate()
            .take_while(|(_, col_x)| *col_x <= x)
            .last()
            .unwrap_or((0, 0.));

        // Past the last slot (real or virtual).
        if contain_idx >= slots.len() {
            return closest_new_column;
        }

        match slots[contain_idx] {
            InsertColumnSlot::Virtual(idx) => {
                // Pointer is over the live-reflow placeholder; keep that drop slot.
                InsertPosition::NewColumn(idx)
            }
            InsertColumnSlot::Real(col_idx) => {
                let col = &self.columns[col_idx];
                if col.group.is_some() {
                    return closest_new_column;
                }

                let (closest_tile_idx, tile_y) = if col.display_mode == ColumnDisplay::Tabbed {
                    // In tabbed mode, there's only one tile visible, and we want to check its top
                    // and bottom.
                    let top = col.tile_offsets().nth(col.active_tile_idx).unwrap().y;
                    let bottom = top + col.data[col.active_tile_idx].size.h;
                    if (top - y).abs() <= (bottom - y).abs() {
                        (col.active_tile_idx, top)
                    } else {
                        (col.active_tile_idx + 1, bottom)
                    }
                } else {
                    col.tile_insert_ys()
                        .iter()
                        .copied()
                        .enumerate()
                        .min_by_key(|(_, tile_y)| NotNan::new((tile_y - y).abs()).unwrap())
                        .unwrap()
                };

                let tile_pos = if let Some((gap_idx, _)) = col.live_reflow_tile_gap {
                    if closest_tile_idx == gap_idx {
                        InsertPosition::InColumn(col_idx, gap_idx)
                    } else if closest_tile_idx > gap_idx {
                        InsertPosition::InColumn(col_idx, closest_tile_idx - 1)
                    } else {
                        InsertPosition::InColumn(col_idx, closest_tile_idx)
                    }
                } else {
                    InsertPosition::InColumn(col_idx, closest_tile_idx)
                };

                // Prefer staying on the current live-reflow slot near column/tile edges.
                let vert_dist = (col_x - x).abs();
                let hor_dist = (tile_y - y).abs();
                const EDGE_HYSTERESIS: f64 = 16.;
                if let Some(gap) = self.live_reflow_gap {
                    if (vert_dist - hor_dist).abs() <= EDGE_HYSTERESIS
                        && (gap.position == closest_new_column || gap.position == tile_pos)
                    {
                        return gap.position;
                    }
                }

                if vert_dist <= hor_dist {
                    closest_new_column
                } else {
                    tile_pos
                }
            }
        }
    }

    fn insert_column_slots(&self) -> (Vec<f64>, Vec<InsertColumnSlot>) {
        let gaps = self.column_gap();
        let mut starts = Vec::with_capacity(self.columns.len() + 2);
        let mut slots = Vec::with_capacity(self.columns.len() + 1);
        let mut x = 0.;

        let virtual_col = match self.live_reflow_gap {
            Some(LiveReflowGap {
                position: InsertPosition::NewColumn(idx),
                width,
                ..
            }) => Some((idx, width)),
            _ => None,
        };

        for (idx, data) in self.data.iter().copied().enumerate() {
            if virtual_col.is_some_and(|(insert_idx, _)| insert_idx == idx) {
                let width = virtual_col.unwrap().1;
                starts.push(x);
                slots.push(InsertColumnSlot::Virtual(idx));
                x += width + gaps;
            }
            starts.push(x);
            slots.push(InsertColumnSlot::Real(idx));
            x += data.width + gaps;
        }

        if virtual_col.is_some_and(|(insert_idx, _)| insert_idx == self.columns.len()) {
            let width = virtual_col.unwrap().1;
            starts.push(x);
            slots.push(InsertColumnSlot::Virtual(self.columns.len()));
            x += width + gaps;
        }

        // One past all columns, matching column_xs().
        starts.push(x);
        (starts, slots)
    }

    pub(super) fn window_insert_position(&self, window: &W::Id) -> Option<InsertPosition> {
        let col_idx = self.columns.iter().position(|col| col.contains(window))?;
        let col = &self.columns[col_idx];
        if col.group.is_some() {
            Some(InsertPosition::NewColumn(col_idx + 1))
        } else if col.tiles.len() == 1 {
            Some(InsertPosition::NewColumn(col_idx))
        } else {
            Some(InsertPosition::InColumn(col_idx, col.position(window)?))
        }
    }

    pub(super) fn live_reflow_gap(&self) -> Option<LiveReflowGap> {
        self.drop_dest.or(self.live_reflow_gap)
    }

    pub(super) fn set_live_reflow_gap(&mut self, dest: Option<LiveReflowGap>) {
        self.set_live_reflow(dest, None, true);
    }

    pub(super) fn set_live_reflow(
        &mut self,
        dest: Option<LiveReflowGap>,
        _origin: Option<LiveReflowGap>,
        animate: bool,
    ) {
        self.drop_dest = dest;
        // Leftover NewColumn dests stay overlay-only: hovering a gutter is still
        // a drop target, but the slot never grows to the dragged window width
        // (`gutter-expand-ms` is ignored). Split/stack dests are overlay-only too.
        if self.live_reflow_gap.is_some() {
            self.set_live_reflow_gap_inner(None, animate);
        }
    }

    fn leftover_insert_hole(&self, gap: Option<LiveReflowGap>) -> f64 {
        match gap {
            Some(g)
                if !g.origin
                    && g.squeeze.is_none()
                    && matches!(g.position, InsertPosition::NewColumn(_)) =>
            {
                g.width + self.column_gap()
            }
            _ => 0.,
        }
    }

    fn set_live_reflow_gap_inner(&mut self, gap: Option<LiveReflowGap>, animate: bool) {
        let old_hole = self.leftover_insert_hole(self.live_reflow_gap);
        let gap = gap.filter(|gap| match gap.position {
            InsertPosition::NewColumn(idx) => {
                !self.columns.is_empty()
                    && idx <= self.columns.len()
                    && gap.width > 0.
                    && gap.squeeze.is_none_or(|(i, _)| i < self.columns.len())
            }
            InsertPosition::InColumn(col_idx, tile_idx) => {
                self.columns.get(col_idx).is_some_and(|col| {
                    tile_idx <= col.tiles.len() && col.display_mode != ColumnDisplay::Tabbed
                })
            }
            InsertPosition::Floating => false,
        });

        let same_slot = match (self.live_reflow_gap, gap) {
            (None, None) => true,
            (Some(a), Some(b)) if a.position == b.position && a.origin == b.origin => {
                let squeeze_eq = match (a.squeeze, b.squeeze) {
                    (None, None) => true,
                    (Some((i, w)), Some((j, v))) => i == j && (w - v).abs() < 1.,
                    _ => false,
                };
                match a.position {
                    InsertPosition::NewColumn(_) => (a.width - b.width).abs() < 1. && squeeze_eq,
                    InsertPosition::InColumn(..) | InsertPosition::Floating => squeeze_eq,
                }
            }
            _ => false,
        };
        if same_slot {
            return;
        }

        // Same dwindle target, only the insert side changed (left/right of the
        // same column). Move the virtual gap without unsqueezing — that restore
        // was what made a packed drag flicker.
        if let (Some(a), Some(b)) = (self.live_reflow_gap, gap) {
            let same_squeeze = match (a.squeeze, b.squeeze) {
                (Some((i, w)), Some((j, v))) => i == j && (w - v).abs() < 1.,
                _ => false,
            };
            let same_stack = match (a.position, b.position) {
                (InsertPosition::InColumn(c1, _), InsertPosition::InColumn(c2, _)) => c1 == c2,
                _ => false,
            };
            if same_squeeze || same_stack {
                if !self.columns.is_empty() {
                    let old_active_x = self.column_x(self.active_column_idx);
                    if same_stack {
                        if let InsertPosition::InColumn(col_idx, tile_idx) = b.position {
                            let col = &mut self.columns[col_idx];
                            let old_ys: Vec<f64> = col.tile_offsets().map(|p| p.y).collect();
                            col.live_reflow_tile_gap = Some((tile_idx, b.height.max(1.)));
                            col.update_tile_sizes(animate);
                            if animate {
                                let new_ys: Vec<f64> =
                                    col.tile_offsets().map(|p| p.y).collect();
                                for (j, tile) in col.tiles.iter_mut().enumerate() {
                                    let dy = old_ys[j] - new_ys[j];
                                    if dy.abs() > 0.001 {
                                        tile.animate_move_y_from(dy);
                                    }
                                }
                            }
                            self.data[col_idx].update(&self.columns[col_idx]);
                        }
                    }
                    self.live_reflow_gap = gap;
                    let new_active_x = self.column_x(self.active_column_idx);
                    self.view_offset.offset(old_active_x - new_active_x);
                } else {
                    self.live_reflow_gap = gap;
                }
                return;
            }
        }

        let n = self.columns.len();
        let old_xs: Vec<f64> = (0..n).map(|i| self.column_x(i)).collect();
        let old_active_x = if n == 0 {
            0.
        } else {
            self.column_x(self.active_column_idx)
        };
        let old_tile_ys: Vec<Vec<f64>> = self
            .columns
            .iter()
            .map(|col| col.tile_offsets().map(|p| p.y).collect())
            .collect();

        let mut cols_to_resize: Vec<usize> = Vec::new();
        let mut restore_width: Vec<Option<f64>> = vec![None; n];
        for (i, col) in self.columns.iter_mut().enumerate() {
            let mut changed = false;
            if col.live_reflow_width.is_some() {
                restore_width[i] = col.live_reflow_orig_width.take();
                col.live_reflow_width = None;
                changed = true;
            }
            if col.live_reflow_tile_gap.is_some() {
                col.live_reflow_tile_gap = None;
                col.live_reflow_tile_heights = None;
                changed = true;
            }
            if changed {
                cols_to_resize.push(i);
            }
        }
        self.live_reflow_gap = gap;
        if let Some(LiveReflowGap {
            position: InsertPosition::InColumn(col_idx, tile_idx),
            height,
            ..
        }) = gap
        {
            self.columns[col_idx].live_reflow_tile_gap = Some((tile_idx, height.max(1.)));
            if !cols_to_resize.contains(&col_idx) {
                cols_to_resize.push(col_idx);
            }
        }
        if let Some(LiveReflowGap {
            squeeze: Some((col_idx, width)),
            ..
        }) = gap
        {
            if col_idx < self.columns.len() {
                let orig = restore_width[col_idx]
                    .take()
                    .unwrap_or(self.data[col_idx].width);
                let col = &mut self.columns[col_idx];
                col.live_reflow_orig_width = Some(orig);
                col.live_reflow_width = Some(width.max(1.));
                if !cols_to_resize.contains(&col_idx) {
                    cols_to_resize.push(col_idx);
                }
            }
        }
        for i in cols_to_resize {
            self.columns[i].update_tile_sizes(animate);
            if let Some(orig) = restore_width[i] {
                self.data[i].width = orig;
            } else {
                self.data[i].update(&self.columns[i]);
            }
        }
        if let Some(LiveReflowGap {
            position: InsertPosition::InColumn(col_idx, _),
            height,
            ..
        }) = &mut self.live_reflow_gap
        {
            if let Some((_, h)) = self.columns[*col_idx].live_reflow_tile_gap {
                *height = h;
            }
        }

        if n == 0 {
            return;
        }

        let new_active_x = self.column_x(self.active_column_idx);
        let extra = (self.leftover_insert_hole(self.live_reflow_gap) - old_hole) / 2.;
        if animate {
            self.view_offset.offset(old_active_x - new_active_x + extra);
        }

        let new_xs: Vec<f64> = (0..n).map(|i| self.column_x(i)).collect();
        let new_tile_ys: Vec<Vec<f64>> = self
            .columns
            .iter()
            .map(|col| col.tile_offsets().map(|p| p.y).collect())
            .collect();

        if !animate {
            return;
        }

        for (i, col) in self.columns.iter_mut().enumerate() {
            let dx = old_xs[i] - new_xs[i] + extra;
            if dx.abs() > 0.001 {
                col.animate_move_from(dx);
            }
            for (j, tile) in col.tiles.iter_mut().enumerate() {
                let dy = old_tile_ys[i][j] - new_tile_ys[i][j];
                if dy.abs() > 0.001 {
                    tile.animate_move_y_from(dy);
                }
            }
        }
    }

    pub fn add_tile(
        &mut self,
        col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
        anim_config: Option<niri_config::Animation>,
    ) {
        let column = Column::new_with_tile(
            tile,
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            width,
            is_full_width,
        );

        self.add_column(col_idx, column, activate, anim_config);
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        self.set_live_reflow_gap(None);

        if self.columns[col_idx].group.is_some() {
            let width = ColumnWidth::Fixed(tile.tile_expected_or_current_size().w);
            self.add_tile(Some(col_idx + 1), tile, activate, width, false, None);
            return;
        }

        let prev_next_x = self.column_x(col_idx + 1);

        let target_column = &mut self.columns[col_idx];
        let tile_idx = tile_idx.unwrap_or(target_column.tiles.len());
        let mut prev_active_tile_idx = target_column.active_tile_idx;

        target_column.add_tile_at(tile_idx, tile);
        self.data[col_idx].update(target_column);

        if tile_idx <= prev_active_tile_idx {
            target_column.active_tile_idx += 1;
            prev_active_tile_idx += 1;
        }

        if activate {
            target_column.activate_idx(tile_idx);
            if self.active_column_idx != col_idx {
                self.activate_column(col_idx);
            }
        }

        let target_column = &mut self.columns[col_idx];
        if target_column.display_mode == ColumnDisplay::Tabbed {
            if target_column.active_tile_idx == tile_idx {
                // Fade out the previously active tile.
                let tile = &mut target_column.tiles[prev_active_tile_idx];
                tile.animate_alpha(1., 0., self.options.animations.window_movement.0);
            } else {
                // Fade out when adding into a tabbed column into the background.
                let tile = &mut target_column.tiles[tile_idx];
                tile.animate_alpha(1., 0., self.options.animations.window_movement.0);
            }
        }

        // Adding a wider window into a column increases its width now (even if the window will
        // shrink later). Move the columns to account for this.
        let offset = self.column_x(col_idx + 1) - prev_next_x;
        if self.active_column_idx <= col_idx {
            for col in &mut self.columns[col_idx + 1..] {
                col.animate_move_from(-offset);
            }
        } else {
            for col in &mut self.columns[..=col_idx] {
                col.animate_move_from(offset);
            }
        }
    }

    pub fn add_tile_right_of(
        &mut self,
        right_of: &W::Id,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
    ) {
        let right_of_idx = self
            .columns
            .iter()
            .position(|col| col.contains(right_of))
            .unwrap();
        let col_idx = right_of_idx + 1;

        self.add_tile(Some(col_idx), tile, activate, width, is_full_width, None);
    }

    /// Place a new tiled window, capping the strip at four viewports.
    ///
    /// Below the cap the window becomes a new column at `col_idx` (or after the
    /// active column when `col_idx` is `None`). At the cap it splits the column
    /// under the pointer (or `next_to`) like Hyprland dwindle, or stacks into
    /// that column when a horizontal split would go below a quarter viewport.
    pub fn add_tile_capped(
        &mut self,
        col_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
        width: ColumnWidth,
        is_full_width: bool,
        next_to: Option<&W::Id>,
        pointer: Option<Point<f64, Logical>>,
    ) {
        if !tile.window().pending_sizing_mode().is_normal() {
            self.add_tile(col_idx, tile, activate, width, is_full_width, None);
            return;
        }

        let requested = if is_full_width {
            ColumnWidth::Proportion(1.)
        } else {
            width
        };

        match self.capped_placement(col_idx, requested, next_to, pointer) {
            CappedPlace::NewColumn { idx, width } => {
                let full = is_full_width
                    && matches!(width, ColumnWidth::Proportion(p) if (p - 1.).abs() < 1e-3);
                self.add_tile(Some(idx), tile, activate, width, full, None);
            }
            CappedPlace::Split {
                col_idx,
                insert_left,
                half,
            } => {
                self.split_column(col_idx, insert_left, half, tile, activate);
            }
            CappedPlace::Stack {
                col_idx,
                tile_idx,
                above,
            } => {
                self.stack_into_column(col_idx, tile_idx, above, tile, activate);
            }
        }
    }

    fn cap_width(&self) -> f64 {
        self.working_area.size.w * CAP_VIEWPORTS
    }

    fn min_col_width(&self) -> f64 {
        self.working_area.size.w * MIN_COL_VIEWPORTS
    }

    fn set_view_offset_value(&mut self, offset: f64) {
        match &mut self.view_offset {
            ViewOffset::Static(v) => *v = offset,
            ViewOffset::Animation(_) => {
                self.view_offset = ViewOffset::Static(offset);
            }
            ViewOffset::Gesture(g) => {
                let delta = offset - g.current_view_offset;
                g.current_view_offset += delta;
                g.delta_from_tracker += delta;
                g.stationary_view_offset += delta;
            }
        }
    }

    pub(super) fn set_drag_edge_limit(&mut self, width: f64) {
        self.drag_edge_limit = width + self.options.layout.gaps;
        self.drag_edge_left = self.drag_edge_left.min(self.drag_edge_limit);
        self.drag_edge_right = self.drag_edge_right.min(self.drag_edge_limit);
    }

    pub(super) fn set_drag_spread(&mut self, spread: bool) {
        if self.drag_spread == spread {
            return;
        }
        let n = self.columns.len();
        let old_xs: Vec<f64> = (0..n).map(|i| self.column_x(i)).collect();
        let old_active_x = if n == 0 {
            0.
        } else {
            self.column_x(self.active_column_idx)
        };
        self.drag_spread = spread;
        if !spread {
            // Stop edge-grow; free-pan / live-reflow sizing rules resume.
            self.drag_edge_left = 0.;
            self.drag_edge_right = 0.;
        }
        if n > 0 {
            // Keep the absolute camera still. `view_pos` is tied to the active
            // column's X; when gaps open/close that X jumps and every column to
            // its left would snap on screen unless we compensate the offset.
            // Then animate columns from their old absolute positions.
            let new_active_x = self.column_x(self.active_column_idx);
            self.view_offset.offset(old_active_x - new_active_x);
            self.animate_xs_from(&old_xs);
            if let ViewOffset::Gesture(g) = &self.view_offset {
                let cur = g.current_view_offset;
                let clamped = self.clamp_dnd_view_offset(cur);
                if (clamped - cur).abs() > 0.5 {
                    self.set_view_offset_value(clamped);
                }
            }
        }
    }

    /// Leftover strip room under the 4-viewport cap, not yet claimed by
    /// edge-grow virtual padding.
    fn drag_edge_grow_room(&self) -> f64 {
        (self.cap_width() - self.occupied_width() - self.drag_edge_left - self.drag_edge_right)
            .max(0.)
    }

    /// While a tiled drag is held against a screen edge and the view cannot
    /// scroll further, grow virtual strip padding toward `cap_width` so the
    /// camera can keep moving into unused space under the cap.
    fn grow_drag_edge_toward_cap(&mut self, step: f64) -> f64 {
        if !self.drag_spread || step == 0. {
            return 0.;
        }
        let side = if step > 0. {
            self.drag_edge_right
        } else {
            self.drag_edge_left
        };
        let room = self
            .drag_edge_grow_room()
            .min((self.drag_edge_limit - side).max(0.));
        if room < 1. {
            return 0.;
        }
        let grow = step.abs().min(room);
        if step > 0. {
            self.drag_edge_right += grow;
        } else {
            self.drag_edge_left += grow;
        }
        grow
    }

    fn column_gap(&self) -> f64 {
        let extra = if self.drag_spread {
            DRAG_COL_GAP_EXTRA
        } else {
            0.
        };
        self.options.layout.gaps + extra
    }

    fn occupied_width(&self) -> f64 {
        if self.columns.is_empty() {
            0.
        } else {
            self.column_x(self.columns.len()) - self.column_gap()
        }
    }

    fn resolve_new_column_width(&self, width: ColumnWidth) -> f64 {
        let gaps = self.options.layout.gaps;
        let working_w = self.working_area.size.w;
        match width {
            ColumnWidth::Proportion(p) => (working_w - gaps) * p - gaps,
            ColumnWidth::Fixed(w) => w,
        }
    }

    fn capped_placement(
        &self,
        col_idx: Option<usize>,
        width: ColumnWidth,
        next_to: Option<&W::Id>,
        pointer: Option<Point<f64, Logical>>,
    ) -> CappedPlace {
        let idx = col_idx.unwrap_or_else(|| {
            if self.columns.is_empty() {
                0
            } else {
                self.active_column_idx + 1
            }
        });
        let idx = next_to
            .and_then(|id| {
                self.columns
                    .iter()
                    .position(|c| c.contains(id))
                    .map(|i| i + 1)
            })
            .unwrap_or(idx);

        if self.columns.is_empty() {
            return CappedPlace::NewColumn { idx: 0, width };
        }

        let gap = self.options.layout.gaps;
        let new_w = self.resolve_new_column_width(width);
        let occupied = self.occupied_width();
        let cap = self.cap_width();
        let projected = occupied + gap + new_w;

        if projected <= cap + 1. {
            return CappedPlace::NewColumn { idx, width };
        }

        let leftover = cap - occupied - gap;
        if leftover >= self.min_col_width() {
            return CappedPlace::NewColumn {
                idx,
                width: ColumnWidth::Fixed(leftover.max(1.)),
            };
        }

        let target = self.split_target(next_to, pointer);
        let col = &self.columns[target.col_idx];
        if !col.pending_sizing_mode().is_normal() || col.group.is_some() {
            return CappedPlace::NewColumn { idx, width };
        }

        let col_w = self.data[target.col_idx].width;
        let half = (col_w - gap) / 2.;
        if half >= self.min_col_width() && col.tiles.len() == 1 {
            CappedPlace::Split {
                col_idx: target.col_idx,
                insert_left: target.left,
                half,
            }
        } else {
            CappedPlace::Stack {
                col_idx: target.col_idx,
                tile_idx: target.tile_idx,
                above: target.above,
            }
        }
    }

    fn split_target(
        &self,
        next_to: Option<&W::Id>,
        pointer: Option<Point<f64, Logical>>,
    ) -> SplitTarget {
        let fallback = SplitTarget {
            col_idx: self.active_column_idx,
            tile_idx: self.columns[self.active_column_idx].active_tile_idx,
            left: false,
            above: false,
        };

        let hit_id = pointer.and_then(|pos| {
            self.tiles_with_render_positions()
                .find_map(|(tile, tile_pos, visible)| {
                    if !visible {
                        return None;
                    }
                    let size = tile.tile_size();
                    Rectangle::new(tile_pos, size)
                        .contains(pos)
                        .then(|| tile.window().id().clone())
                })
        });

        let (col_idx, tile_idx) = if let Some(id) = next_to {
            self.columns
                .iter()
                .enumerate()
                .find_map(|(c, col)| col.position(id).map(|t| (c, t)))
                .unwrap_or((fallback.col_idx, fallback.tile_idx))
        } else if let Some(id) = hit_id.as_ref() {
            self.columns
                .iter()
                .enumerate()
                .find_map(|(c, col)| col.position(id).map(|t| (c, t)))
                .unwrap_or((fallback.col_idx, fallback.tile_idx))
        } else {
            (fallback.col_idx, fallback.tile_idx)
        };

        let target_id = self.columns[col_idx].tiles[tile_idx].window().id().clone();
        let (left, above) = if let Some(pos) = pointer {
            self.tiles_with_render_positions()
                .find_map(|(tile, tile_pos, _)| {
                    if tile.window().id() != &target_id {
                        return None;
                    }
                    let size = tile.tile_size();
                    Some((
                        pos.x < tile_pos.x + size.w / 2.,
                        pos.y < tile_pos.y + size.h / 2.,
                    ))
                })
                .unwrap_or((false, false))
        } else {
            (false, false)
        };

        SplitTarget {
            col_idx,
            tile_idx,
            left,
            above,
        }
    }

    fn unsqueezed_col_width(&self, idx: usize) -> f64 {
        self.columns[idx]
            .live_reflow_orig_width
            .unwrap_or(self.data[idx].width)
    }

    fn leftover_for_new_column(&self) -> f64 {
        let gaps = self.options.layout.gaps;
        let occupied = if self.columns.is_empty() {
            0.
        } else {
            self.columns
                .iter()
                .enumerate()
                .map(|(i, col)| col.live_reflow_orig_width.unwrap_or(self.data[i].width))
                .sum::<f64>()
                + gaps * (self.columns.len() - 1) as f64
        };
        (self.cap_width() - occupied - gaps).max(0.)
    }

    fn new_column_insert_width(&self, dragged_w: f64) -> Option<f64> {
        if self.drag_spread {
            return Some(dragged_w.max(1.));
        }
        let leftover = self.leftover_for_new_column();
        let min_col = self.min_col_width();
        if leftover >= dragged_w {
            Some(dragged_w.max(1.))
        } else if leftover >= min_col {
            Some(leftover.max(1.))
        } else {
            None
        }
    }

    fn hovered_real_column(&self, pos: Point<f64, Logical>) -> Option<usize> {
        let x = pos.x + self.view_pos();
        for i in 0..self.columns.len() {
            let x0 = self.column_x(i);
            let x1 = x0 + self.data[i].width;
            if x >= x0 && x < x1 {
                return Some(i);
            }
        }
        None
    }

    fn squeeze_pair_range(&self, col_idx: usize) -> (f64, f64) {
        let gaps = self.options.layout.gaps;
        if let Some(gap) = self.live_reflow_gap {
            if let (Some((s, _)), InsertPosition::NewColumn(insert_idx)) =
                (gap.squeeze, gap.position)
            {
                if s == col_idx {
                    let body = self.column_x(col_idx);
                    let w = self.data[col_idx].width;
                    return if insert_idx <= col_idx {
                        (body - gap.width - gaps, body + w)
                    } else {
                        (body, body + w + gaps + gap.width)
                    };
                }
            }
        }
        let x = self.column_x(col_idx);
        (x, x + self.unsqueezed_col_width(col_idx))
    }

    fn pointer_in_squeeze_range(&self, pos: Point<f64, Logical>, gap: LiveReflowGap) -> bool {
        let Some((col_idx, _)) = gap.squeeze else {
            return false;
        };
        if col_idx >= self.columns.len() {
            return false;
        }
        let x = pos.x + self.view_pos();
        let (left, right) = self.squeeze_pair_range(col_idx);
        const PAD: f64 = 32.;
        x >= left - PAD && x <= right + PAD
    }

    fn origin_new_column_idx(origin: Option<LiveReflowGap>) -> Option<usize> {
        match origin?.position {
            InsertPosition::NewColumn(idx) => Some(idx),
            InsertPosition::InColumn(_, _) | InsertPosition::Floating => None,
        }
    }

    fn gutter_insert_idx(&self, pos: Point<f64, Logical>) -> Option<usize> {
        if self.hovered_real_column(pos).is_some() {
            return None;
        }
        let x = pos.x + self.view_pos();
        let first_x = self.column_x(0);
        if x < first_x {
            return Some(0);
        }
        let last = self.columns.len() - 1;
        let last_end = self.column_x(last) + self.data[last].width;
        if x >= last_end {
            return Some(self.columns.len());
        }
        for i in 0..last {
            let end = self.column_x(i) + self.data[i].width;
            let next = self.column_x(i + 1);
            if x >= end && x < next {
                return Some(i + 1);
            }
        }
        None
    }

    #[allow(dead_code)]
    fn edge_insert_idx(&self, col_idx: usize, pos: Point<f64, Logical>) -> Option<usize> {
        let x = pos.x + self.view_pos();
        let x0 = self.column_x(col_idx);
        let w = self.data[col_idx].width;
        let x1 = x0 + w;
        let edge = (w * NEW_COL_EDGE_FRAC).clamp(NEW_COL_EDGE_MIN, NEW_COL_EDGE_MAX);
        if x < x0 + edge {
            Some(col_idx)
        } else if x >= x1 - edge {
            Some(col_idx + 1)
        } else {
            None
        }
    }

    fn new_column_gap(&self, idx: usize, width: f64, height: f64) -> LiveReflowGap {
        LiveReflowGap {
            position: InsertPosition::NewColumn(idx),
            width: width.max(1.),
            height,
            squeeze: None,
            origin: false,
        }
    }

    fn in_stack_band(&self, pos: Point<f64, Logical>) -> bool {
        let area = self.working_area;
        let band = area.size.h * STACK_BAND_FRAC;
        pos.y < area.loc.y + band || pos.y > area.loc.y + area.size.h - band
    }

    fn split_too_narrow(&self, col_idx: usize) -> bool {
        let col = &self.columns[col_idx];
        let gaps = self.options.layout.gaps;
        let half = ((self.unsqueezed_col_width(col_idx) - gaps) / 2.).max(1.);
        let min_half = self.working_area.size.w * MIN_SPLIT_HALF_VIEWPORTS;
        half < min_half || col.tiles.len() != 1 || !col.pending_sizing_mode().is_normal()
    }

    /// Real drop sites on `col_idx`, matching `squeeze_reflow_column` (not a 2×2 grid).
    ///
    /// Stack bands are 20% of the working area; the leftover middle is split at the
    /// column midpoint. If a horizontal split is impossible, the only sites are
    /// above/below the working-area center.
    pub(super) fn drop_zone_guide_rects(&self, col_idx: usize) -> Vec<Rectangle<f64, Logical>> {
        if col_idx >= self.columns.len() {
            return Vec::new();
        }

        let col = &self.columns[col_idx];
        let col_w = self.unsqueezed_col_width(col_idx).max(1.);
        let x0 = self.column_x(col_idx) + col.tiles_origin().x;
        let gaps = self.options.layout.gaps;
        let area = self.working_area;
        let y0 = area.loc.y + gaps;
        let y1 = area.loc.y + area.size.h - gaps;
        let view = self.view_pos();

        let mut rects = Vec::new();
        let mut push_rect = |x: f64, y: f64, w: f64, h: f64| {
            if w >= 1. && h >= 1. {
                rects.push(Rectangle::new(
                    Point::from((x - view, y)),
                    Size::from((w, h)),
                ));
            }
        };

        if self.split_too_narrow(col_idx) {
            let cy = (area.loc.y + area.size.h / 2.).clamp(y0, y1);
            push_rect(x0, y0, col_w, cy - y0);
            push_rect(x0, cy, col_w, y1 - cy);
            return rects;
        }

        let band = area.size.h * STACK_BAND_FRAC;
        let stack_top_end = area.loc.y + band;
        let stack_bot_start = area.loc.y + area.size.h - band;
        let (pair_left, pair_right) = self.squeeze_pair_range(col_idx);
        let mid = ((pair_left + pair_right) / 2.).clamp(x0, x0 + col_w);

        let top_h = (stack_top_end - y0).min(y1 - y0);
        push_rect(x0, y0, col_w, top_h);

        let bot_y = stack_bot_start.max(y0);
        push_rect(x0, bot_y, col_w, y1 - bot_y);

        let mid_y0 = stack_top_end.max(y0);
        let mid_y1 = stack_bot_start.min(y1);
        let mid_h = mid_y1 - mid_y0;
        push_rect(x0, mid_y0, mid - x0, mid_h);
        push_rect(mid, mid_y0, x0 + col_w - mid, mid_h);

        rects
    }

    /// Hyprland-style dwindle preview of `col_idx`: share that column with the
    /// moving window (horizontal split, or vertical stack in the top/bottom band).
    fn squeeze_reflow_column(
        &self,
        col_idx: usize,
        pos: Point<f64, Logical>,
        dragged: Size<f64, Logical>,
    ) -> LiveReflowGap {
        let col_w = self.unsqueezed_col_width(col_idx);
        let gaps = self.options.layout.gaps;
        let half = ((col_w - gaps) / 2.).max(1.);
        let col = &self.columns[col_idx];
        let (pair_left, pair_right) = self.squeeze_pair_range(col_idx);
        let pair_w = (pair_right - pair_left).max(1.);
        let x = pos.x + self.view_pos();
        let insert_left = x < pair_left + pair_w / 2.;

        let area = self.working_area;
        let cy = area.loc.y + area.size.h / 2.;
        const BAND_HYST: f64 = 24.;

        let too_narrow = self.split_too_narrow(col_idx);

        let in_band = self.in_stack_band(pos);
        let prefer_vertical = if too_narrow {
            true
        } else {
            match self.live_reflow_gap {
                Some(g) if g.squeeze.is_some_and(|(i, _)| i == col_idx) => {
                    let band = area.size.h * STACK_BAND_FRAC;
                    pos.y < area.loc.y + band - BAND_HYST
                        || pos.y > area.loc.y + area.size.h - band + BAND_HYST
                }
                Some(g)
                    if matches!(
                        g.position,
                        InsertPosition::InColumn(c, _) if c == col_idx
                    ) =>
                {
                    let band = area.size.h * STACK_BAND_FRAC;
                    pos.y < area.loc.y + band + BAND_HYST
                        || pos.y > area.loc.y + area.size.h - band - BAND_HYST
                }
                _ => in_band,
            }
        };

        if prefer_vertical {
            let above = pos.y < cy;
            let tile_idx = if above { 0 } else { col.tiles.len() };
            LiveReflowGap {
                position: InsertPosition::InColumn(col_idx, tile_idx),
                width: col_w,
                height: dragged.h,
                squeeze: None,
                origin: false,
            }
        } else {
            let insert_idx = if insert_left { col_idx } else { col_idx + 1 };
            LiveReflowGap {
                position: InsertPosition::NewColumn(insert_idx),
                width: half,
                height: dragged.h,
                squeeze: Some((col_idx, half)),
                origin: false,
            }
        }
    }

    /// Over a window: stack in the top/bottom band, else split it in half.
    /// Leftover new columns come from gutters, not from the window's lateral edge.
    fn dest_for_column(
        &self,
        col_idx: usize,
        pos: Point<f64, Logical>,
        dragged: Size<f64, Logical>,
        _origin_new: Option<usize>,
    ) -> LiveReflowGap {
        self.squeeze_reflow_column(col_idx, pos, dragged)
    }

    /// Slot for a tiled window being dragged.
    ///
    /// Origin hole is not applied. Hovering a gutter is always a drop dest; the
    /// dest gutter stays at the (slightly enlarged) inter-column gap and does not
    /// grow to the dragged window width.
    pub(super) fn live_reflow_for_move(
        &self,
        pos: Point<f64, Logical>,
        dragged: Size<f64, Logical>,
        origin: Option<LiveReflowGap>,
    ) -> Option<LiveReflowGap> {
        if self.columns.is_empty() {
            return None;
        }

        let origin_new = Self::origin_new_column_idx(origin);

        // Stay on a committed dest until the pointer leaves that slot.
        if let Some(gap) = self.live_reflow_gap {
            if !gap.origin {
                if let InsertPosition::InColumn(c, _) = gap.position {
                    if self.hovered_real_column(pos) == Some(c) {
                        return Some(self.dest_for_column(c, pos, dragged, origin_new));
                    }
                }
                if let (Some((col_idx, _)), true) =
                    (gap.squeeze, self.pointer_in_squeeze_range(pos, gap))
                {
                    return Some(self.dest_for_column(col_idx, pos, dragged, origin_new));
                }
                if let InsertPosition::NewColumn(idx) = gap.position {
                    if gap.squeeze.is_none() {
                        let still_there = self.gutter_insert_idx(pos) == Some(idx);
                        if still_there {
                            let width =
                                self.new_column_insert_width(dragged.w).unwrap_or(gap.width);
                            return Some(self.new_column_gap(idx, width, dragged.h));
                        }
                    }
                }
            }
        }

        if let Some(col_idx) = self.hovered_real_column(pos) {
            // Group columns are gutter-only drop targets.
            if self.columns[col_idx].group.is_none() {
                return Some(self.dest_for_column(col_idx, pos, dragged, origin_new));
            }
        }

        if let Some(insert_idx) = self.gutter_insert_idx(pos) {
            if let Some(width) = self.new_column_insert_width(dragged.w) {
                return Some(self.new_column_gap(insert_idx, width, dragged.h));
            }
        }

        None
    }

    pub(super) fn split_column(
        &mut self,
        col_idx: usize,
        insert_left: bool,
        half: f64,
        tile: Tile<W>,
        activate: bool,
    ) {
        if self.columns[col_idx].group.is_some() {
            let idx = if insert_left { col_idx } else { col_idx + 1 };
            self.add_tile(
                Some(idx),
                tile,
                activate,
                ColumnWidth::Fixed(half.max(1.)),
                false,
                None,
            );
            return;
        }
        let pair_w = self.data[col_idx].width;
        let half = half.max(1.);

        // Shrinking the target column moves every column to its right. Capture
        // positions first so neighbors animate instead of snapping.
        let old_xs: Vec<f64> = (0..self.columns.len()).map(|i| self.column_x(i)).collect();
        {
            let col = &mut self.columns[col_idx];
            col.width = ColumnWidth::Fixed(half);
            col.preset_width_idx = None;
            col.is_full_width = false;
            col.update_tile_sizes(true);
            self.data[col_idx].update(col);
        }
        self.animate_xs_from(&old_xs);

        let new_idx = if insert_left { col_idx } else { col_idx + 1 };
        self.add_tile(
            Some(new_idx),
            tile,
            activate,
            ColumnWidth::Fixed(half),
            false,
            None,
        );

        let old_idx = if insert_left { col_idx + 1 } else { col_idx };
        let new_idx = if insert_left { col_idx } else { col_idx + 1 };
        let id = self.next_split_id;
        self.next_split_id = self.next_split_id.wrapping_add(1);

        for idx in [old_idx, new_idx] {
            let col = &mut self.columns[idx];
            col.split_id = Some(id);
            col.split_pair_width = pair_w;
            col.split_seq = id;
            col.natural_width = half;
        }
    }

    fn stack_into_column(
        &mut self,
        col_idx: usize,
        tile_idx: usize,
        above: bool,
        tile: Tile<W>,
        activate: bool,
    ) {
        if self.columns[col_idx].group.is_some() {
            self.add_tile_to_column(col_idx, None, tile, activate);
            return;
        }
        {
            let col = &mut self.columns[col_idx];
            col.convert_heights_to_auto();
        }
        let orig_w = match self.columns[col_idx].data[tile_idx].height {
            WindowHeight::Auto { weight } => weight,
            _ => 1.,
        };
        let insert_idx = if above { tile_idx } else { tile_idx + 1 };
        self.add_tile_to_column(col_idx, Some(insert_idx), tile, activate);

        let (orig_i, new_i) = if above {
            (insert_idx + 1, insert_idx)
        } else {
            (tile_idx, insert_idx)
        };
        let col = &mut self.columns[col_idx];
        col.data[orig_i].height = WindowHeight::Auto {
            weight: orig_w / 2.,
        };
        col.data[new_i].height = WindowHeight::Auto {
            weight: orig_w / 2.,
        };
        col.update_tile_sizes(true);
        self.data[col_idx].update(col);
    }

    fn restore_space_after_close(&mut self, split_id: Option<u64>, pair_w: f64) {
        if self.columns.is_empty() {
            return;
        }

        let old_xs: Vec<f64> = (0..=self.columns.len()).map(|i| self.column_x(i)).collect();

        if let Some(id) = split_id {
            if let Some(i) = self.columns.iter().position(|c| c.split_id == Some(id)) {
                let col = &mut self.columns[i];
                col.width = ColumnWidth::Fixed(pair_w.max(1.));
                col.natural_width = pair_w.max(1.);
                col.split_id = None;
                col.update_tile_sizes(true);
                self.data[i].update(col);
                self.animate_xs_from(&old_xs);
                return;
            }
        }

        let mut budget = (self.cap_width() - self.occupied_width()).max(0.);
        let mut order: Vec<usize> = (0..self.columns.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(self.columns[i].split_seq));
        for i in order {
            if budget < 1. {
                break;
            }
            let cur = self.data[i].width;
            let natural = self.columns[i].natural_width;
            if cur + 0.5 >= natural {
                continue;
            }
            let grow = (natural - cur).min(budget);
            let col = &mut self.columns[i];
            col.width = ColumnWidth::Fixed(cur + grow);
            col.update_tile_sizes(true);
            self.data[i].update(col);
            budget -= grow;
        }
        self.animate_xs_from(&old_xs);
    }

    fn animate_xs_from(&mut self, old_xs: &[f64]) {
        for i in 0..self.columns.len() {
            let dx = old_xs.get(i).copied().unwrap_or(0.) - self.column_x(i);
            if dx.abs() > 0.001 {
                self.columns[i].animate_move_from(dx);
            }
        }
    }

    fn enforce_cap(&mut self, col_idx: usize, prefer_right: bool) {
        let extra = self.occupied_width() - self.cap_width();
        if extra <= 1. {
            return;
        }

        let n = self.columns.len();
        let min_w = self.min_col_width();
        if n >= 2 {
            let neighbor = if prefer_right && col_idx + 1 < n {
                col_idx + 1
            } else if !prefer_right && col_idx > 0 {
                col_idx - 1
            } else if col_idx + 1 < n {
                col_idx + 1
            } else {
                col_idx - 1
            };
            let neigh_w = self.data[neighbor].width;
            let shrink = extra.min((neigh_w - min_w).max(0.));
            if shrink > 0.5 {
                let col = &mut self.columns[neighbor];
                col.width = ColumnWidth::Fixed((neigh_w - shrink).max(1.));
                col.preset_width_idx = None;
                col.is_full_width = false;
                col.update_tile_sizes(false);
                self.data[neighbor].update(col);
            }
        }

        let extra = self.occupied_width() - self.cap_width();
        if extra > 1. {
            let w = self.data[col_idx].width;
            let new_w = (w - extra).max(min_w).max(1.);
            let col = &mut self.columns[col_idx];
            col.width = ColumnWidth::Fixed(new_w);
            col.natural_width = col.width();
            col.preset_width_idx = None;
            col.is_full_width = false;
            col.update_tile_sizes(false);
            self.data[col_idx].update(col);
        }
    }

    pub fn add_column(
        &mut self,
        idx: Option<usize>,
        mut column: Column<W>,
        activate: bool,
        anim_config: Option<niri_config::Animation>,
    ) {
        self.set_live_reflow_gap(None);

        let was_empty = self.columns.is_empty();

        let idx = idx.unwrap_or_else(|| {
            if was_empty {
                0
            } else {
                self.active_column_idx + 1
            }
        });

        column.update_config(
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            self.options.clone(),
        );
        self.data.insert(idx, ColumnData::new(&column));
        self.columns.insert(idx, column);

        if !was_empty && idx <= self.active_column_idx {
            self.active_column_idx += 1;
        }

        // Columns pushed right by the insert always animate from their old X.
        // (Previously this only ran when active_column_idx <= idx, so inserting
        // left of the active column — especially at index 0 — snapped them.)
        let offset = self.column_x(idx + 1) - self.column_x(idx);
        let config = anim_config.unwrap_or(self.options.animations.window_movement.0);
        for col in &mut self.columns[idx + 1..] {
            col.animate_move_from_with_config(-offset, config);
        }
        // When the active column sits to the right of the insert, the camera is
        // tied to it; left-side columns need a compensating slide.
        if self.active_column_idx > idx {
            for col in &mut self.columns[..idx] {
                col.animate_move_from_with_config(offset, config);
            }
        }

        if activate {
            // If this is the first window on an empty workspace, remove the effect of whatever
            // view_offset was left over and skip the animation.
            if was_empty {
                self.view_offset = ViewOffset::Static(0.);
                self.view_offset =
                    ViewOffset::Static(self.compute_new_view_offset_for_column(None, idx, None));
            }

            let prev_offset = (!was_empty && idx == self.active_column_idx + 1)
                .then(|| self.view_offset.stationary());

            let anim_config =
                anim_config.unwrap_or(self.options.animations.horizontal_view_movement.0);
            self.activate_column_with_anim_config(idx, anim_config);
            self.activate_prev_column_on_removal = prev_offset;
        }
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        if self.columns.is_empty() {
            return None;
        }

        let column = &self.columns[self.active_column_idx];
        Some(self.remove_tile_by_idx(
            self.active_column_idx,
            column.active_tile_idx,
            transaction,
            None,
        ))
    }

    pub fn remove_tile(&mut self, window: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();
        let column = &self.columns[column_idx];

        let tile_idx = column.position(window).unwrap();
        self.remove_tile_by_idx(column_idx, tile_idx, transaction, None)
    }

    /// Pull a tiled window out for interactive move. Remaining tiles close the
    /// hole immediately; dest gutters stay at the enlarged column gap.
    pub(super) fn remove_tile_for_move(
        &mut self,
        window: &W::Id,
        transaction: Transaction,
    ) -> (RemovedTile<W>, Option<LiveReflowGap>) {
        self.set_live_reflow_gap(None);

        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();
        let tile_idx = self.columns[column_idx].position(window).unwrap();
        let tile_size = self.columns[column_idx].tiles[tile_idx].tile_size();
        let col_w = self.data[column_idx].width.max(1.);
        let from_group = self.columns[column_idx].group.is_some();
        let origin_gap = if from_group {
            // Cancel puts a group member back as its own column next to the group.
            LiveReflowGap {
                position: InsertPosition::NewColumn(column_idx + 1),
                width: tile_size.w.max(1.),
                height: tile_size.h.max(1.),
                squeeze: None,
                origin: true,
            }
        } else if self.columns[column_idx].tiles.len() == 1 {
            LiveReflowGap {
                position: InsertPosition::NewColumn(column_idx),
                width: col_w,
                height: tile_size.h.max(1.),
                squeeze: None,
                origin: true,
            }
        } else {
            LiveReflowGap {
                position: InsertPosition::InColumn(column_idx, tile_idx),
                width: col_w,
                height: tile_size.h.max(1.),
                squeeze: None,
                origin: true,
            }
        };

        let n = self.columns.len();
        let old_xs: Vec<f64> = (0..n).map(|i| self.column_x(i)).collect();
        let saved_view = self.view_pos();
        let removed_col = self.columns[column_idx].tiles.len() == 1;
        let removed = if removed_col {
            let mut column = self.remove_column_for_move(column_idx);
            RemovedTile {
                tile: column.tiles.remove(tile_idx),
                width: column.width,
                is_full_width: column.is_full_width,
                is_floating: false,
            }
        } else {
            self.remove_tile_from_column_for_move(column_idx, tile_idx, transaction)
        };

        if self.columns.is_empty() {
            return (removed, None);
        }

        if !self.columns.is_empty() {
            let new_active_x = self.column_x(self.active_column_idx);
            self.set_view_offset_value(saved_view - new_active_x);
            if removed_col {
                for i in 0..self.columns.len() {
                    let old_i = if i >= column_idx { i + 1 } else { i };
                    let dx = old_xs.get(old_i).copied().unwrap_or(0.) - self.column_x(i);
                    if dx.abs() > 0.001 {
                        self.columns[i].animate_move_from(dx);
                    }
                }
            }
        }

        (removed, Some(origin_gap))
    }

    fn remove_column_for_move(&mut self, column_idx: usize) -> Column<W> {
        if let Some(resize) = &self.interactive_resize {
            if self.columns[column_idx]
                .tiles
                .iter()
                .any(|tile| tile.window().id() == &resize.window)
            {
                self.interactive_resize = None;
            }
        }

        let column = self.columns.remove(column_idx);
        self.data.remove(column_idx);

        if column_idx + 1 == self.active_column_idx {
            self.activate_prev_column_on_removal = None;
        }
        if column_idx == self.active_column_idx {
            self.view_offset_to_restore = None;
        }
        if self.columns.is_empty() {
            return column;
        }

        if column_idx < self.active_column_idx {
            self.active_column_idx -= 1;
            self.activate_prev_column_on_removal = None;
        } else if self.active_column_idx >= self.columns.len() {
            self.active_column_idx = self.columns.len() - 1;
        }

        column
    }

    fn remove_tile_from_column_for_move(
        &mut self,
        column_idx: usize,
        tile_idx: usize,
        transaction: Transaction,
    ) -> RemovedTile<W> {
        let column = &mut self.columns[column_idx];

        if let Some(resize) = &self.interactive_resize {
            if column.tiles[tile_idx].window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        let was_normal = column.sizing_mode().is_normal();
        let was_group = column.group.is_some();
        let tile = column.tiles.remove(tile_idx);
        column.data.remove(tile_idx);
        column.group_member_removed(tile_idx);

        if column_idx == self.active_column_idx && !was_normal && column.sizing_mode().is_normal() {
            self.view_offset_to_restore = None;
        }

        if column.data.len() == 1 {
            if let WindowHeight::Auto { weight } = &mut column.data[0].height {
                *weight = 1.;
            }
        }

        if tile_idx < column.active_tile_idx {
            column.active_tile_idx -= 1;
        } else if tile_idx == column.active_tile_idx && tile_idx == column.tiles.len() {
            column.activate_idx(tile_idx - 1);
        }

        let width = if was_group {
            ColumnWidth::Fixed(tile.tile_expected_or_current_size().w)
        } else {
            column.width
        };
        let removed = RemovedTile {
            tile,
            width,
            is_full_width: column.is_full_width && !was_group,
            is_floating: false,
        };

        column.update_tile_sizes_with_transaction(false, transaction);
        self.data[column_idx].update(column);
        removed
    }

    pub fn remove_tile_by_idx(
        &mut self,
        column_idx: usize,
        tile_idx: usize,
        transaction: Transaction,
        anim_config: Option<niri_config::Animation>,
    ) -> RemovedTile<W> {
        self.set_live_reflow_gap(None);

        // If this is the only tile in the column, remove the whole column.
        if self.columns[column_idx].tiles.len() == 1 {
            let mut column = self.remove_column_by_idx(column_idx, anim_config);
            return RemovedTile {
                tile: column.tiles.remove(tile_idx),
                width: column.width,
                is_full_width: column.is_full_width,
                is_floating: false,
            };
        }

        let column = &mut self.columns[column_idx];
        let prev_width = self.data[column_idx].width;

        let movement_config = anim_config.unwrap_or(self.options.animations.window_movement.0);

        // Animate movement of other tiles.
        // FIXME: tiles can move by X too, in a centered or resizing layout with one window smaller
        // than the others.
        let was_group = column.group.is_some();
        let prev_offsets: Vec<_> = if was_group {
            column.tile_offsets().take(column.tiles.len()).collect()
        } else {
            let offset_y = column.tile_offset(tile_idx + 1).y - column.tile_offset(tile_idx).y;
            for tile in &mut column.tiles[tile_idx + 1..] {
                tile.animate_move_y_from(offset_y);
            }
            Vec::new()
        };

        if column.display_mode == ColumnDisplay::Tabbed && tile_idx != column.active_tile_idx {
            // Fade in when removing background tab from a tabbed column.
            let tile = &mut column.tiles[tile_idx];
            tile.animate_alpha(0., 1., movement_config);
        }

        let was_normal = column.sizing_mode().is_normal();

        let tile = column.tiles.remove(tile_idx);
        column.data.remove(tile_idx);
        column.group_member_removed(tile_idx);

        // If an active column became non-fullscreen after removing the tile, clear the stored
        // unfullscreen offset.
        if column_idx == self.active_column_idx && !was_normal && column.sizing_mode().is_normal() {
            self.view_offset_to_restore = None;
        }

        // If one window is left, reset its weight to 1.
        if column.data.len() == 1 {
            if let WindowHeight::Auto { weight } = &mut column.data[0].height {
                *weight = 1.;
            }
        }

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
            }
        }

        let width = if was_group {
            ColumnWidth::Fixed(tile.tile_expected_or_current_size().w)
        } else {
            column.width
        };
        let tile = RemovedTile {
            tile,
            width,
            is_full_width: column.is_full_width && !was_group,
            is_floating: false,
        };

        #[allow(clippy::comparison_chain)] // What do you even want here?
        if tile_idx < column.active_tile_idx {
            // A tile above was removed; preserve the current position.
            column.active_tile_idx -= 1;
        } else if tile_idx == column.active_tile_idx {
            // The active tile was removed, so the active tile index shifted to the next tile.
            if tile_idx == column.tiles.len() {
                // The bottom tile was removed and it was active, update active idx to remain valid.
                column.activate_idx(tile_idx - 1);
            } else {
                // Ensure the newly active tile animates to opaque.
                column.tiles[tile_idx].ensure_alpha_animates_to_1();
            }
        }

        column.update_tile_sizes_with_transaction(true, transaction);
        self.data[column_idx].update(column);
        if was_group {
            let mut prev_offsets = prev_offsets;
            prev_offsets.remove(tile_idx);
            let new_offsets: Vec<_> = column.tile_offsets().take(column.tiles.len()).collect();
            for ((tile, prev), new) in zip(&mut column.tiles, prev_offsets).zip(new_offsets) {
                tile.animate_move_from(prev - new);
            }
        }
        let offset = prev_width - column.width();

        // Animate movement of the other columns.
        if self.active_column_idx <= column_idx {
            for col in &mut self.columns[column_idx + 1..] {
                col.animate_move_from_with_config(offset, movement_config);
            }
        } else {
            for col in &mut self.columns[..=column_idx] {
                col.animate_move_from_with_config(-offset, movement_config);
            }
        }

        tile
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        if self.columns.is_empty() {
            return None;
        }

        Some(self.remove_column_by_idx(self.active_column_idx, None))
    }

    pub fn remove_column_by_idx(
        &mut self,
        column_idx: usize,
        anim_config: Option<niri_config::Animation>,
    ) -> Column<W> {
        self.set_live_reflow_gap(None);

        // Animate movement of the other columns.
        let movement_config = anim_config.unwrap_or(self.options.animations.window_movement.0);
        let offset = self.column_x(column_idx + 1) - self.column_x(column_idx);
        if self.active_column_idx <= column_idx {
            for col in &mut self.columns[column_idx + 1..] {
                col.animate_move_from_with_config(offset, movement_config);
            }
        } else {
            for col in &mut self.columns[..column_idx] {
                col.animate_move_from_with_config(-offset, movement_config);
            }
        }

        let column = self.columns.remove(column_idx);
        self.data.remove(column_idx);

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if column
                .tiles
                .iter()
                .any(|tile| tile.window().id() == &resize.window)
            {
                self.interactive_resize = None;
            }
        }

        if column_idx + 1 == self.active_column_idx {
            // The previous column, that we were going to activate upon removal of the active
            // column, has just been itself removed.
            self.activate_prev_column_on_removal = None;
        }

        if column_idx == self.active_column_idx {
            self.view_offset_to_restore = None;
        }

        if self.columns.is_empty() {
            return column;
        }

        let view_config = anim_config.unwrap_or(self.options.animations.horizontal_view_movement.0);

        if column_idx < self.active_column_idx {
            // A column to the left was removed; preserve the current position.
            // FIXME: preserve activate_prev_column_on_removal.
            self.active_column_idx -= 1;
            self.activate_prev_column_on_removal = None;
        } else if column_idx == self.active_column_idx
            && self.activate_prev_column_on_removal.is_some()
        {
            // The active column was removed, and we needed to activate the previous column.
            if 0 < column_idx {
                let prev_offset = self.activate_prev_column_on_removal.unwrap();

                self.activate_column_with_anim_config(self.active_column_idx - 1, view_config);

                // Restore the view offset but make sure to scroll the view in case the
                // previous window had resized.
                self.animate_view_offset_with_config(
                    self.active_column_idx,
                    prev_offset,
                    view_config,
                );
                self.animate_view_offset_to_column_with_config(
                    None,
                    self.active_column_idx,
                    None,
                    view_config,
                );
            }
        } else {
            self.activate_column_with_anim_config(
                min(self.active_column_idx, self.columns.len() - 1),
                view_config,
            );
        }

        self.restore_space_after_close(column.split_id, column.split_pair_width);
        column
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        let (col_idx, column) = self
            .columns
            .iter_mut()
            .enumerate()
            .find(|(_, col)| col.contains(window))
            .unwrap();
        let was_normal = column.sizing_mode().is_normal();
        let prev_origin = column.tiles_origin();

        let (tile_idx, tile) = column
            .tiles
            .iter_mut()
            .enumerate()
            .find(|(_, tile)| tile.window().id() == window)
            .unwrap();

        let resize = tile.window_mut().interactive_resize_data();

        // Do this before calling update_window() so it can get up-to-date info.
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }

        let prev_width = self.data[col_idx].width;

        column.update_window(window);
        self.data[col_idx].update(column);
        column.update_tile_sizes(false);

        let offset = prev_width - self.data[col_idx].width;

        // Move other columns in tandem with resizing.
        let ongoing_resize_anim = column.tiles[tile_idx].resize_animation().is_some();
        if offset != 0. {
            if self.active_column_idx <= col_idx {
                for col in &mut self.columns[col_idx + 1..] {
                    // If there's a resize animation on the tile (that may have just started in
                    // column.update_window()), then the apparent size change is smooth with no
                    // sudden jumps. This corresponds to adding an X animation to adjacent columns.
                    //
                    // There could also be no resize animation with nonzero offset. This could
                    // happen for example:
                    // - if the window resized on its own, which we don't animate
                    // - if the window resized by less than 10 px (the resize threshold)
                    //
                    // The latter case could also cancel an ongoing resize animation.
                    //
                    // Now, stationary columns shouldn't react to this offset change in any way,
                    // i.e. their apparent X position should jump together with the resize.
                    // However, adjacent columns that are already animating an X movement should
                    // offset their animations to avoid the jump.
                    //
                    // Notably, this is necessary to fix the animation jump when resizing width back
                    // and forth in quick succession (in a way that cancels the resize animation).
                    if ongoing_resize_anim {
                        col.animate_move_from_with_config(
                            offset,
                            self.options.animations.window_resize.anim,
                        );
                    } else {
                        col.offset_move_anim_current(offset);
                    }
                }
            } else {
                for col in &mut self.columns[..=col_idx] {
                    if ongoing_resize_anim {
                        col.animate_move_from_with_config(
                            -offset,
                            self.options.animations.window_resize.anim,
                        );
                    } else {
                        col.offset_move_anim_current(-offset);
                    }
                }
            }
        }

        // When a column goes between fullscreen and non-fullscreen, the tiles origin can change.
        // The change comes from things like ignoring struts and hiding the tab indicator in
        // fullscreen, so both in X and Y directions.
        let column = &mut self.columns[col_idx];
        let new_origin = column.tiles_origin();
        let origin_delta = prev_origin - new_origin;
        if origin_delta != Point::new(0., 0.) {
            for (tile, _pos) in column.tiles_mut() {
                tile.animate_move_from(origin_delta);
            }
        }

        if col_idx == self.active_column_idx {
            // If offset == 0, then don't mess with the view or the gesture. Some clients (Firefox,
            // Chromium, Electron) currently don't commit after the ack of a configure that drops
            // the Resizing state, which can trigger this code path for a while.
            let resize = if offset != 0. { resize } else { None };
            if let Some(resize) = resize {
                // Don't bother with the gesture.
                self.view_offset.cancel_gesture();

                // If this is an interactive resize commit of an active window, then we need to
                // either preserve the view offset or adjust it accordingly.
                let centered = self.is_centering_focused_column();

                let width = self.data[col_idx].width;
                let offset = if centered {
                    // FIXME: when view_offset becomes fractional, this can be made additive too.
                    let new_offset =
                        -(self.working_area.size.w - width) / 2. - self.working_area.loc.x;
                    new_offset - self.view_offset.target()
                } else if resize.edges.contains(ResizeEdge::LEFT) {
                    -offset
                } else {
                    0.
                };

                self.view_offset.offset(offset);
            }

            // When the active column goes fullscreen, store the view offset to restore later.
            let is_normal = self.columns[col_idx].sizing_mode().is_normal();
            if was_normal && !is_normal {
                self.view_offset_to_restore = Some(self.view_offset.stationary());
            }

            // Upon unfullscreening, restore the view offset.
            //
            // In tabbed display mode, there can be multiple tiles in a fullscreen column. They
            // will unfullscreen one by one, and the column width will shrink only when the
            // last tile unfullscreens. This is when we want to restore the view offset,
            // otherwise it will immediately reset back by the animate_view_offset below.
            let unfullscreen_offset = if !was_normal && is_normal {
                // Take the value unconditionally, even if the view is currently frozen by
                // a view gesture. It shouldn't linger around because it's only valid for this
                // particular unfullscreen.
                self.view_offset_to_restore.take()
            } else {
                None
            };

            // We might need to move the view to ensure the resized window is still visible. But
            // only do it when the view isn't frozen by an interactive resize, a view gesture,
            // or a user pan. A commit of the focused window must not yank the camera back.
            if self.interactive_resize.is_none()
                && !self.view_offset.is_gesture()
                && !self.user_panned_view
            {
                // Synchronize the horizontal view movement with the resize so that it looks nice.
                // This is especially important for always-centered view.
                let config = if ongoing_resize_anim {
                    self.options.animations.window_resize.anim
                } else {
                    self.options.animations.horizontal_view_movement.0
                };

                // Restore the view offset upon unfullscreening if needed.
                if let Some(prev_offset) = unfullscreen_offset {
                    self.animate_view_offset_with_config(col_idx, prev_offset, config);
                }

                // FIXME: we will want to skip the animation in some cases here to make continuously
                // resizing windows not look janky.
                self.animate_view_offset_to_column_with_config(None, col_idx, None, config);
            }
        }
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        let column_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if self.active_column_idx == column_idx {
            return 0.;
        }

        // Consider the end of an ongoing animation because that's what compute to fit does too.
        let target_x = self.target_view_pos();
        let new_view_offset = self.compute_new_view_offset_for_column(
            Some(target_x),
            column_idx,
            Some(self.active_column_idx),
        );

        let new_col_x = self.column_x(column_idx);
        let from_view_offset = target_x - new_col_x;

        (from_view_offset - new_view_offset).abs() / self.working_area.size.w
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        let column_idx = self.columns.iter().position(|col| col.contains(window));
        let Some(column_idx) = column_idx else {
            return false;
        };
        let column = &mut self.columns[column_idx];

        column.activate_window(window);
        // Fit the column into view (overview / explicit activate). Ordinary
        // focus paths use activate_column / keep_view / pull_if_needed instead.
        self.activate_column_and_fit(column_idx);

        true
    }

    /// Horizontal fraction of the column's width that intersects the working area.
    ///
    /// Height is ignored (stacked windows share the column width). Used to decide
    /// whether a click should pull the camera onto a mostly off-screen column.
    fn column_visible_width_fraction(&self, column_idx: usize) -> f64 {
        let width = self.columns[column_idx].width();
        if width <= 0. {
            return 1.;
        }

        let view_pos = self.view_pos();
        let col_left = self.column_x(column_idx);
        let col_right = col_left + width;
        let wa_left = view_pos + self.working_area.loc.x;
        let wa_right = wa_left + self.working_area.size.w;

        let visible = (col_right.min(wa_right) - col_left.max(wa_left)).max(0.);
        visible / width
    }

    /// Focus `window`, pulling the camera only if less than ~95% of its column
    /// width is visible in the working area.
    pub fn activate_window_pull_if_needed(&mut self, window: &W::Id) -> bool {
        let column_idx = self.columns.iter().position(|col| col.contains(window));
        let Some(column_idx) = column_idx else {
            return false;
        };

        self.columns[column_idx].activate_window(window);

        const MOSTLY_VISIBLE: f64 = 0.95;
        if self.column_visible_width_fraction(column_idx) >= MOSTLY_VISIBLE {
            self.activate_column_keep_view(column_idx);
        } else {
            self.activate_column_and_fit(column_idx);
        }

        true
    }

    /// Switch active column without moving the camera. Preserves ongoing view
    /// animations/gestures (offset is relative to the active column).
    fn activate_column_keep_view(&mut self, idx: usize) {
        if self.active_column_idx == idx {
            return;
        }

        let old_col_x = self.column_x(self.active_column_idx);
        let new_col_x = self.column_x(idx);
        self.view_offset.offset(old_col_x - new_col_x);
        self.active_column_idx = idx;
        self.activate_prev_column_on_removal = None;
        self.view_offset_to_restore = None;
        self.interactive_resize = None;
    }

    /// Focus a column and animate the view to fit it, even if the user had panned.
    fn activate_column_and_fit(&mut self, idx: usize) {
        // Intentional pull must move the camera; clear the pan flag so commits
        // can settle the fit, and animate regardless of the previous flag.
        self.user_panned_view = false;

        self.animate_view_offset_to_column_with_config(
            None,
            idx,
            Some(self.active_column_idx),
            self.options.animations.horizontal_view_movement.0,
        );

        if self.active_column_idx != idx {
            self.active_column_idx = idx;
            self.activate_prev_column_on_removal = None;
            self.view_offset_to_restore = None;
            self.interactive_resize = None;
        }
    }

    /// Focus `window` but keep the current camera position.
    ///
    /// Does not stop an ongoing view pan animation/gesture: the offset is
    /// rewritten relative to the new active column so `view_pos()` stays put.
    pub fn activate_window_keep_view(&mut self, window: &W::Id) -> bool {
        let column_idx = self.columns.iter().position(|col| col.contains(window));
        let Some(column_idx) = column_idx else {
            return false;
        };

        self.columns[column_idx].activate_window(window);
        self.activate_column_keep_view(column_idx);
        true
    }

    /// True while the user is freely panning the view (wheel coast or swipe gesture).
    pub fn is_user_view_pan_active(&self) -> bool {
        match &self.view_offset {
            ViewOffset::Gesture(gesture) if gesture.dnd_last_event_time.is_none() => true,
            ViewOffset::Animation(_) if self.user_panned_view => true,
            _ => false,
        }
    }

    /// Slide the camera by `delta` logical px without focusing another column.
    /// Positive moves the view toward later columns (content travels left).
    ///
    /// Uses the same `horizontal-view-movement` animation as column focus, and
    /// retargets from the current offset so a reverse mid-flight stays fluid.
    pub fn scroll_view_by(&mut self, delta: f64) {
        if self.columns.is_empty() || delta == 0. || self.view_offset_locked() {
            return;
        }
        // From the first pixel, the focused window must not animate the view back.
        self.user_panned_view = true;
        let from = self.view_offset.current();
        let mut to = self.view_offset.target() + delta;
        if let Some((lo, hi)) = self.free_pan_offset_bounds() {
            to = to.clamp(lo, hi);
        }
        let pixel = 1. / self.scale;
        if (to - from).abs() < pixel {
            self.view_offset = ViewOffset::Static(to);
            return;
        }
        self.view_offset = ViewOffset::Animation(Animation::new(
            self.clock.clone(),
            from,
            to,
            0.,
            self.options.animations.horizontal_view_movement.0,
        ));
    }

    /// Wheel notch: add velocity and coast. No static jump — fluid from the first tick.
    pub fn impulse_view(&mut self, delta: f64) {
        if self.columns.is_empty() || delta == 0. || self.view_offset_locked() {
            return;
        }
        self.user_panned_view = true;

        let now = self.clock.now_unadjusted();
        let sign = delta.signum();
        let gain = match self.wheel_accel {
            Some((last, last_sign, gain))
                if last_sign == sign && now.saturating_sub(last) <= WHEEL_ACCEL_WINDOW =>
            {
                (gain * WHEEL_ACCEL_STEP).min(WHEEL_ACCEL_MAX)
            }
            _ => 1.,
        };
        self.wheel_accel = Some((now, sign, gain));

        // Tiled interactive-move / DnD: add to dnd_velocity. Position is
        // integrated in dnd_scroll_gesture_scroll() each frame.
        // Floating drag must not take this path — it kills free-pan coast.
        if matches!(
            &self.view_offset,
            ViewOffset::Gesture(g) if g.dnd_last_event_time.is_some()
        ) {
            const IMPULSE_DT: f64 = 0.14;
            let applied = gain * delta;
            let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
                return;
            };
            gesture.dnd_braking = false;
            gesture.dnd_velocity = (gesture.dnd_velocity + applied / IMPULSE_DT)
                .clamp(-WHEEL_MAX_VELOCITY, WHEEL_MAX_VELOCITY);
            return;
        }

        // Map one notch (~120 px) to a short flick so the first event already glides.
        const IMPULSE_DT: f64 = 0.14;

        let from = self.view_offset.current();
        let current_vel = match &self.view_offset {
            ViewOffset::Animation(anim) => anim.velocity(),
            _ => 0.,
        };
        // Reversing direction drops the old momentum instead of fighting it.
        let current_vel = if current_vel * sign < 0. {
            0.
        } else {
            current_vel
        };
        let velocity = (current_vel + gain * delta / IMPULSE_DT)
            .clamp(-WHEEL_MAX_VELOCITY, WHEEL_MAX_VELOCITY);
        self.coast_view_from(from, velocity);
    }

    /// Brake a free pan like a bike: keep the current speed and bleed it off
    /// much faster than a normal glide, so a faster pan takes longer to stop.
    pub fn brake_view_pan(&mut self) -> bool {
        if !self.is_user_view_pan_active() {
            return false;
        }
        let ViewOffset::Animation(anim) = &self.view_offset else {
            return false;
        };
        let velocity = anim.velocity();
        let cur = self.view_offset.current();
        self.wheel_accel = None;
        if velocity.abs() < 1. {
            self.view_offset = ViewOffset::Static(cur);
            return true;
        }
        self.view_offset = ViewOffset::Animation(Animation::decelerate(
            self.clock.clone(),
            cur,
            velocity,
            WHEEL_BRAKE_RATE,
            0.5,
        ));
        true
    }

    /// Freeze a free pan (wheel glide or view animation) where it is right now.
    pub fn stop_view_pan(&mut self) -> bool {
        if !self.is_user_view_pan_active() {
            return false;
        }
        if let ViewOffset::Animation(_) = &self.view_offset {
            let cur = self.view_offset.current();
            self.view_offset = ViewOffset::Static(cur);
        }
        self.wheel_accel = None;
        true
    }

    /// Keep moving after the wheel stops, without snapping to a column.
    ///
    /// Same deceleration curve as the touchpad swipe tracker (`0.997`).
    /// Extra travel is capped so one flick does not cross the whole workspace.
    pub fn coast_view(&mut self, velocity: f64) {
        if self.columns.is_empty() || velocity.abs() < 1. {
            return;
        }
        self.coast_view_from(self.view_offset.current(), velocity);
    }

    fn coast_view_from(&mut self, from: f64, velocity: f64) {
        if velocity.abs() < 1. {
            return;
        }
        const RATE: f64 = 0.997;
        let coeff = 1000. * RATE.ln();
        let velocity = velocity.clamp(-WHEEL_MAX_VELOCITY, WHEEL_MAX_VELOCITY);
        let extra = -velocity / coeff;

        if let Some((lo, hi)) = self.free_pan_offset_bounds() {
            if from < lo || from > hi {
                self.view_offset = ViewOffset::Animation(Animation::new(
                    self.clock.clone(),
                    from,
                    from.clamp(lo, hi),
                    0.,
                    self.options.animations.horizontal_view_movement.0,
                ));
                return;
            }
            // Already against the edge and pushing into it: stay locked.
            if (extra > 0. && from >= hi) || (extra < 0. && from <= lo) {
                self.view_offset = ViewOffset::Static(from.clamp(lo, hi));
                return;
            }
            // Full speed into the edge; advance_animations() stops it dead there.
        }

        if extra.abs() < 0.5 {
            self.view_offset = ViewOffset::Static(from);
            return;
        }
        self.view_offset = ViewOffset::Animation(Animation::decelerate(
            self.clock.clone(),
            from,
            -extra * coeff,
            RATE,
            0.5,
        ));
    }

    /// `view_offset` limits for free panning, relative to `col_idx`.
    ///
    /// Scroll/pan clamps to the content edges (first/last column), not into
    /// leftover empty space from `(cap_width - occupied_width)`. Gaps stay on
    /// both ends so the wallpaper remains visible beside the content. Centering
    /// (`center_column` / `animate_view_offset_to_column_centered`) bypasses
    /// these bounds and may intentionally show empty padding.
    ///
    /// During a tiled interactive move (`drag_spread`), virtual edge-grow
    /// padding (`drag_edge_left` / `drag_edge_right`) extends the scrollable
    /// strip toward `cap_width` while the pointer is held at a screen edge.
    fn free_pan_offset_bounds_for(&self, col_idx: usize) -> Option<(f64, f64)> {
        if self.columns.is_empty() || col_idx >= self.columns.len() {
            return None;
        }
        let gaps = self.options.layout.gaps;
        let wa = self.working_area;
        let (edge_l, edge_r) = if self.drag_spread {
            (self.drag_edge_left, self.drag_edge_right)
        } else {
            (0., 0.)
        };
        let mut min_pos = -(wa.loc.x + gaps) - edge_l;
        let mut max_pos = self.occupied_width() - wa.loc.x - wa.size.w + gaps + edge_r;
        if min_pos > max_pos {
            let mid = (min_pos + max_pos) / 2.;
            min_pos = mid;
            max_pos = mid;
        }
        let active_x = self.column_x(col_idx);
        Some((min_pos - active_x, max_pos - active_x))
    }

    /// `view_offset` limits for free panning. Recomputed from the live columns,
    /// so the right edge grows as windows are added or widened.
    fn free_pan_offset_bounds(&self) -> Option<(f64, f64)> {
        if self.columns.is_empty() {
            None
        } else {
            self.free_pan_offset_bounds_for(self.active_column_idx)
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        self.start_close_animation_for_window_inner(renderer, window, blocker, false);
    }

    pub fn start_minimize_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
    ) {
        self.start_close_animation_for_window_inner(
            renderer,
            window,
            TransactionBlocker::completed(),
            true,
        );
    }

    fn start_close_animation_for_window_inner(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
        minimize: bool,
    ) {
        let (tile, mut tile_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == window)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        let (col_idx, tile_idx) = self
            .columns
            .iter()
            .enumerate()
            .find_map(|(col_idx, col)| {
                col.tiles
                    .iter()
                    .position(|tile| tile.window().id() == window)
                    .map(move |tile_idx| (col_idx, tile_idx))
            })
            .unwrap();

        let col = &self.columns[col_idx];
        let removing_last = col.tiles.len() == 1;

        // Skip closing animation for invisible tiles in a tabbed column.
        if col.display_mode == ColumnDisplay::Tabbed && tile_idx != col.active_tile_idx {
            return;
        }

        tile_pos.x += self.view_pos();

        if col_idx < self.active_column_idx {
            let offset = if removing_last {
                self.column_x(col_idx + 1) - self.column_x(col_idx)
            } else {
                self.data[col_idx].width
                    - col
                        .data
                        .iter()
                        .enumerate()
                        .filter_map(|(idx, data)| {
                            (idx != tile_idx).then_some(NotNan::new(data.size.w).unwrap())
                        })
                        .max()
                        .map(NotNan::into_inner)
                        .unwrap()
            };
            tile_pos.x -= offset;
        }

        self.start_close_animation_for_tile(
            renderer, snapshot, tile_size, tile_pos, blocker, minimize,
        );
    }

    fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
        minimize: bool,
    ) {
        let anim = if minimize {
            crate::animation::Animation::ease(
                self.clock.clone(),
                0.,
                1.,
                0.,
                220,
                crate::animation::Curve::EaseOutCubic,
            )
        } else {
            Animation::new(
                self.clock.clone(),
                0.,
                1.,
                0.,
                self.options.animations.window_close.anim,
            )
        };

        let blocker = if self.options.disable_transactions {
            TransactionBlocker::completed()
        } else {
            blocker
        };

        let scale = Scale::from(self.scale);
        let res = ClosingWindow::new(
            renderer, snapshot, scale, tile_size, tile_pos, blocker, anim,
        );
        match res {
            Ok(closing) => {
                self.closing_windows.push(closing);
            }
            Err(err) => {
                warn!("error creating a closing window animation: {err:?}");
            }
        }
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.columns
            .iter_mut()
            .any(|col| col.start_open_animation(id))
    }

    pub fn focus_left(&mut self) -> bool {
        if self.active_column_idx == 0 {
            return false;
        }
        self.activate_column(self.active_column_idx - 1);
        true
    }

    pub fn focus_right(&mut self) -> bool {
        if self.active_column_idx + 1 >= self.columns.len() {
            return false;
        }

        self.activate_column(self.active_column_idx + 1);
        true
    }

    pub fn focus_column_first(&mut self) {
        self.activate_column(0);
    }

    pub fn focus_column_last(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.activate_column(self.columns.len() - 1);
    }

    pub fn focus_column(&mut self, index: usize) {
        if self.columns.is_empty() {
            return;
        }

        self.activate_column(index.saturating_sub(1).min(self.columns.len() - 1));
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_index(index);
    }

    pub fn focus_down(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].focus_down()
    }

    pub fn focus_up(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].focus_up()
    }

    pub fn focus_down_or_left(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_down() {
            self.focus_left();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_down() {
            self.focus_right();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_up() {
            self.focus_left();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let column = &mut self.columns[self.active_column_idx];
        if !column.focus_up() {
            self.focus_right();
        }
    }

    pub fn focus_top(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_top()
    }

    pub fn focus_bottom(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.columns[self.active_column_idx].focus_bottom()
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        if self.columns.is_empty() {
            return;
        }

        self.move_column_to(index.saturating_sub(1).min(self.columns.len() - 1));
    }

    fn move_column_to(&mut self, new_idx: usize) {
        if self.active_column_idx == new_idx {
            return;
        }

        self.set_live_reflow_gap(None);

        let current_col_x = self.column_x(self.active_column_idx);
        let next_col_x = self.column_x(self.active_column_idx + 1);

        let mut column = self.columns.remove(self.active_column_idx);
        let data = self.data.remove(self.active_column_idx);
        cancel_resize_for_column(&mut self.interactive_resize, &mut column);
        self.columns.insert(new_idx, column);
        self.data.insert(new_idx, data);

        // Preserve the camera position when moving to the left.
        let view_offset_delta = -self.column_x(self.active_column_idx) + current_col_x;
        self.view_offset.offset(view_offset_delta);

        // The column we just moved is offset by the difference between its new and old position.
        let new_col_x = self.column_x(new_idx);
        self.columns[new_idx].animate_move_from(current_col_x - new_col_x);

        // All columns in between moved by the width of the column that we just moved.
        let others_x_offset = next_col_x - current_col_x;
        if self.active_column_idx < new_idx {
            for col in &mut self.columns[self.active_column_idx..new_idx] {
                col.animate_move_from(others_x_offset);
            }
        } else {
            for col in &mut self.columns[new_idx + 1..=self.active_column_idx] {
                col.animate_move_from(-others_x_offset);
            }
        }

        self.activate_column_with_anim_config(new_idx, self.options.animations.window_movement.0);
    }

    pub fn move_left(&mut self) -> bool {
        if self.active_column_idx == 0 {
            return false;
        }

        self.move_column_to(self.active_column_idx - 1);
        true
    }

    pub fn move_right(&mut self) -> bool {
        let new_idx = self.active_column_idx + 1;
        if new_idx >= self.columns.len() {
            return false;
        }

        self.move_column_to(new_idx);
        true
    }

    pub fn move_column_to_first(&mut self) {
        self.move_column_to(0);
    }

    pub fn move_column_to_last(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let new_idx = self.columns.len() - 1;
        self.move_column_to(new_idx);
    }

    pub fn move_down(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].move_down()
    }

    pub fn move_up(&mut self) -> bool {
        if self.columns.is_empty() {
            return false;
        }

        self.columns[self.active_column_idx].move_up()
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let (source_col_idx, source_tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col_idx, tile_idx))
                })
                .unwrap()
        } else {
            let source_col_idx = self.active_column_idx;
            let source_tile_idx = self.columns[self.active_column_idx].active_tile_idx;
            (source_col_idx, source_tile_idx)
        };

        let source_column = &self.columns[source_col_idx];
        if source_column.group.is_some()
            || (source_column.tiles.len() == 1
                && self
                    .columns
                    .get(source_col_idx.wrapping_sub(1))
                    .is_some_and(Column::is_group))
        {
            return;
        }
        let prev_off = source_column.tile_offset(source_tile_idx);

        let source_tile_was_active = self.active_column_idx == source_col_idx
            && source_column.active_tile_idx == source_tile_idx;

        if source_column.tiles.len() == 1 {
            if source_col_idx == 0 {
                return;
            }

            // Move into adjacent column.
            let target_column_idx = source_col_idx - 1;

            let offset = if self.active_column_idx <= source_col_idx {
                // Tiles to the right animate from the following column.
                self.column_x(source_col_idx) - self.column_x(target_column_idx)
            } else {
                // Tiles to the left animate to preserve their right edge position.
                f64::max(
                    0.,
                    self.data[target_column_idx].width - self.data[source_col_idx].width,
                )
            };
            let mut offset = Point::from((offset, 0.));

            if source_tile_was_active {
                // Make sure the previous (target) column is activated so the animation looks right.
                //
                // However, if it was already going to be activated, leave the offset as is. This
                // improves the workflow that has become common with tabbed columns: open a new
                // window, then immediately consume it left as a new tab.
                self.activate_prev_column_on_removal
                    .get_or_insert(self.view_offset.stationary() + offset.x);
            }

            offset.x += self.columns[source_col_idx].render_offset().x;
            let RemovedTile { tile, .. } = self.remove_tile_by_idx(
                source_col_idx,
                0,
                Transaction::new(),
                Some(self.options.animations.window_movement.0),
            );
            self.add_tile_to_column(target_column_idx, None, tile, source_tile_was_active);

            let target_column = &mut self.columns[target_column_idx];
            offset.x -= target_column.render_offset().x;
            offset += prev_off - target_column.tile_offset(target_column.tiles.len() - 1);

            let new_tile = target_column.tiles.last_mut().unwrap();
            new_tile.animate_move_from(offset);
        } else {
            // Move out of column.
            let mut offset = Point::from((source_column.render_offset().x, 0.));

            let removed =
                self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

            // We're inserting into the source column position.
            let target_column_idx = source_col_idx;

            self.add_tile(
                Some(target_column_idx),
                removed.tile,
                source_tile_was_active,
                removed.width,
                removed.is_full_width,
                Some(self.options.animations.window_movement.0),
            );

            if source_tile_was_active {
                // We added to the left, don't activate even further left on removal.
                self.activate_prev_column_on_removal = None;
            }

            if target_column_idx <= self.active_column_idx {
                // Tiles to the left animate from the following column.
                offset.x += self.column_x(target_column_idx + 1) - self.column_x(target_column_idx);
            }

            let new_col = &mut self.columns[target_column_idx];
            offset += prev_off - new_col.tile_offset(0);
            new_col.tiles[0].animate_move_from(offset);
        }
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let (source_col_idx, source_tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col_idx, tile_idx))
                })
                .unwrap()
        } else {
            let source_col_idx = self.active_column_idx;
            let source_tile_idx = self.columns[self.active_column_idx].active_tile_idx;
            (source_col_idx, source_tile_idx)
        };

        let cur_x = self.column_x(source_col_idx);

        let source_column = &self.columns[source_col_idx];
        if source_column.group.is_some()
            || (source_column.tiles.len() == 1
                && self
                    .columns
                    .get(source_col_idx + 1)
                    .is_some_and(Column::is_group))
        {
            return;
        }
        let mut offset = Point::from((source_column.render_offset().x, 0.));
        let prev_off = source_column.tile_offset(source_tile_idx);

        let source_tile_was_active = self.active_column_idx == source_col_idx
            && source_column.active_tile_idx == source_tile_idx;

        if source_column.tiles.len() == 1 {
            if source_col_idx + 1 == self.columns.len() {
                return;
            }

            // Move into adjacent column.
            let target_column_idx = source_col_idx;

            offset.x += cur_x - self.column_x(source_col_idx + 1);
            offset.x -= self.columns[source_col_idx + 1].render_offset().x;

            if source_tile_was_active {
                // Make sure the target column gets activated.
                self.activate_prev_column_on_removal = None;
            }

            let RemovedTile { tile, .. } = self.remove_tile_by_idx(
                source_col_idx,
                0,
                Transaction::new(),
                Some(self.options.animations.window_movement.0),
            );
            self.add_tile_to_column(target_column_idx, None, tile, source_tile_was_active);

            let target_column = &mut self.columns[target_column_idx];
            offset += prev_off - target_column.tile_offset(target_column.tiles.len() - 1);

            let new_tile = target_column.tiles.last_mut().unwrap();
            new_tile.animate_move_from(offset);
        } else {
            // Move out of column.
            let prev_width = self.data[source_col_idx].width;

            let removed =
                self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

            let target_column_idx = source_col_idx + 1;

            self.add_tile(
                Some(target_column_idx),
                removed.tile,
                source_tile_was_active,
                removed.width,
                removed.is_full_width,
                Some(self.options.animations.window_movement.0),
            );

            offset.x += if self.active_column_idx <= target_column_idx {
                // Tiles to the right animate to the following column.
                cur_x - self.column_x(target_column_idx)
            } else {
                // Tiles to the left animate for a change in width.
                -f64::max(0., prev_width - self.data[target_column_idx].width)
            };

            let new_col = &mut self.columns[target_column_idx];
            offset += prev_off - new_col.tile_offset(0);
            new_col.tiles[0].animate_move_from(offset);
        }
    }

    pub fn consume_into_column(&mut self) {
        if self.columns.len() < 2 {
            return;
        }

        if self.active_column_idx == self.columns.len() - 1 {
            return;
        }

        let target_column_idx = self.active_column_idx;
        let source_column_idx = self.active_column_idx + 1;
        if self.columns[target_column_idx].group.is_some()
            || self.columns[source_column_idx].group.is_some()
        {
            return;
        }

        let offset = self.column_x(source_column_idx)
            + self.columns[source_column_idx].render_offset().x
            - self.column_x(target_column_idx);
        let mut offset = Point::from((offset, 0.));
        let prev_off = self.columns[source_column_idx].tile_offset(0);

        let removed = self.remove_tile_by_idx(source_column_idx, 0, Transaction::new(), None);
        self.add_tile_to_column(target_column_idx, None, removed.tile, false);

        let target_column = &mut self.columns[target_column_idx];
        offset += prev_off - target_column.tile_offset(target_column.tiles.len() - 1);
        offset.x -= target_column.render_offset().x;

        let new_tile = target_column.tiles.last_mut().unwrap();
        new_tile.animate_move_from(offset);
    }

    pub fn expel_from_column(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let source_col_idx = self.active_column_idx;
        let target_col_idx = self.active_column_idx + 1;
        let cur_x = self.column_x(source_col_idx);

        let source_column = &self.columns[self.active_column_idx];
        if source_column.tiles.len() == 1 || source_column.group.is_some() {
            return;
        }

        let source_tile_idx = source_column.tiles.len() - 1;

        let mut offset = Point::from((source_column.render_offset().x, 0.));
        let prev_off = source_column.tile_offset(source_tile_idx);

        let removed =
            self.remove_tile_by_idx(source_col_idx, source_tile_idx, Transaction::new(), None);

        self.add_tile(
            Some(target_col_idx),
            removed.tile,
            false,
            removed.width,
            removed.is_full_width,
            Some(self.options.animations.window_movement.0),
        );

        offset.x += cur_x - self.column_x(target_col_idx);

        let new_col = &mut self.columns[target_col_idx];
        offset += prev_off - new_col.tile_offset(0);
        new_col.tiles[0].animate_move_from(offset);
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        if self.columns.is_empty() {
            return;
        }

        // if this is the first (resp. last column), then this operation is equivalent
        // to an `consume_or_expel_window_left` (resp. `consume_or_expel_window_right`)
        match direction {
            ScrollDirection::Left => {
                if self.active_column_idx == 0 {
                    return;
                }
            }
            ScrollDirection::Right => {
                if self.active_column_idx == self.columns.len() - 1 {
                    return;
                }
            }
        }

        let source_column_idx = self.active_column_idx;
        let target_column_idx = self.active_column_idx.wrapping_add_signed(match direction {
            ScrollDirection::Left => -1,
            ScrollDirection::Right => 1,
        });

        if self.columns[source_column_idx].group.is_some()
            || self.columns[target_column_idx].group.is_some()
        {
            return;
        }

        // if both source and target columns contain a single tile, then the operation is equivalent
        // to a simple column move
        if self.columns[source_column_idx].tiles.len() == 1
            && self.columns[target_column_idx].tiles.len() == 1
        {
            return self.move_column_to(target_column_idx);
        }

        let source_tile_idx = self.columns[source_column_idx].active_tile_idx;
        let target_tile_idx = self.columns[target_column_idx].active_tile_idx;
        let source_column_drained = self.columns[source_column_idx].tiles.len() == 1;

        // capture the original positions of the tiles
        let (mut source_pt, mut target_pt) = (
            self.columns[source_column_idx].render_offset()
                + self.columns[source_column_idx].tile_offset(source_tile_idx),
            self.columns[target_column_idx].render_offset()
                + self.columns[target_column_idx].tile_offset(target_tile_idx),
        );
        source_pt.x += self.column_x(source_column_idx);
        target_pt.x += self.column_x(target_column_idx);

        let transaction = Transaction::new();

        // If the source column contains a single tile, this will also remove the column.
        // When this happens `source_column_drained` will be set and the column will need to be
        // recreated with `add_tile`
        let source_removed = self.remove_tile_by_idx(
            source_column_idx,
            source_tile_idx,
            transaction.clone(),
            None,
        );

        {
            // special case when the source column disappears after removing its last tile
            let adjusted_target_column_idx =
                if direction == ScrollDirection::Right && source_column_drained {
                    target_column_idx - 1
                } else {
                    target_column_idx
                };

            self.add_tile_to_column(
                adjusted_target_column_idx,
                Some(target_tile_idx),
                source_removed.tile,
                false,
            );

            let RemovedTile {
                tile: target_tile, ..
            } = self.remove_tile_by_idx(
                adjusted_target_column_idx,
                target_tile_idx + 1,
                transaction.clone(),
                None,
            );

            if source_column_drained {
                // recreate the drained column with only the target tile
                self.add_tile(
                    Some(source_column_idx),
                    target_tile,
                    true,
                    source_removed.width,
                    source_removed.is_full_width,
                    None,
                )
            } else {
                // simply add the removed target tile to the source column
                self.add_tile_to_column(
                    source_column_idx,
                    Some(source_tile_idx),
                    target_tile,
                    false,
                );
            }
        }

        // update the active tile in the modified columns
        self.columns[source_column_idx].active_tile_idx = source_tile_idx;
        self.columns[target_column_idx].active_tile_idx = target_tile_idx;

        // Animations
        self.columns[target_column_idx].tiles[target_tile_idx]
            .animate_move_from(source_pt - target_pt);
        self.columns[target_column_idx].tiles[target_tile_idx].ensure_alpha_animates_to_1();

        // FIXME: this stop_move_animations() causes the target tile animation to "reset" when
        // swapping. It's here as a workaround to stop the unwanted animation of moving the source
        // tile down when adding the target tile above it. This code needs to be written in some
        // other way not to trigger that animation, or to cancel it properly, so that swap doesn't
        // cancel all ongoing target tile animations.
        self.columns[source_column_idx].tiles[source_tile_idx].stop_move_animations();
        self.columns[source_column_idx].tiles[source_tile_idx]
            .animate_move_from(target_pt - source_pt);
        self.columns[source_column_idx].tiles[source_tile_idx].ensure_alpha_animates_to_1();

        self.activate_column(target_column_idx);
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let col = &mut self.columns[self.active_column_idx];
        let display = match col.display_mode {
            ColumnDisplay::Normal => ColumnDisplay::Tabbed,
            ColumnDisplay::Tabbed => ColumnDisplay::Normal,
        };

        self.set_column_display(display);
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        if self.columns.is_empty() {
            return;
        }

        let col = &mut self.columns[self.active_column_idx];
        if col.display_mode == display || col.group.is_some() {
            return;
        }

        cancel_resize_for_column(&mut self.interactive_resize, col);
        col.set_column_display(display);

        // With place_within_column, the tab indicator changes the column size immediately.
        self.data[self.active_column_idx].update(col);
        col.update_tile_sizes(true);

        // Disable fullscreen if needed.
        if col.display_mode != ColumnDisplay::Tabbed && col.tiles.len() > 1 {
            let window = col.tiles[col.active_tile_idx].window().id().clone();
            self.set_fullscreen(&window, false);
            self.set_maximized(&window, false);
        }
    }

    pub fn center_column(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        self.animate_view_offset_to_column_centered(
            None,
            self.active_column_idx,
            self.options.animations.horizontal_view_movement.0,
        );

        let col = &mut self.columns[self.active_column_idx];
        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn center_window(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = if let Some(window) = window {
            self.columns
                .iter()
                .position(|col| col.contains(window))
                .unwrap()
        } else {
            self.active_column_idx
        };

        // We can reasonably center only the active column.
        if col_idx != self.active_column_idx {
            return;
        }

        self.center_column();
    }

    pub fn center_visible_columns(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        if self.is_centering_focused_column() {
            return;
        }

        // Consider the end of an ongoing animation because that's what compute to fit does too.
        let view_x = self.target_view_pos();
        let working_x = self.working_area.loc.x;
        let working_w = self.working_area.size.w;

        // Count all columns that are fully visible inside the working area.
        let mut width_taken = 0.;
        let mut leftmost_col_x = None;
        let mut active_col_x = None;

        let gap = self.options.layout.gaps;
        let col_xs = self.column_xs(self.data.iter().copied());
        for (idx, col_x) in col_xs.take(self.columns.len()).enumerate() {
            if col_x < view_x + working_x + gap {
                // Column goes off-screen to the left.
                continue;
            }

            leftmost_col_x.get_or_insert(col_x);

            let width = self.data[idx].width;
            if view_x + working_x + working_w < col_x + width + gap {
                // Column goes off-screen to the right. We can stop here.
                break;
            }

            if idx == self.active_column_idx {
                active_col_x = Some(col_x);
            }

            width_taken += width + gap;
        }

        if active_col_x.is_none() {
            // The active column wasn't fully on screen, so we can't meaningfully do anything.
            return;
        }

        let col = &mut self.columns[self.active_column_idx];
        cancel_resize_for_column(&mut self.interactive_resize, col);

        let free_space = working_w - width_taken + gap;
        let new_view_x = leftmost_col_x.unwrap() - free_space / 2. - working_x;

        self.animate_view_offset(self.active_column_idx, new_view_x - active_col_x.unwrap());
        // Just in case.
        self.animate_view_offset_to_column(None, self.active_column_idx, None);
    }

    pub fn view_pos(&self) -> f64 {
        self.column_x(self.active_column_idx) + self.view_offset.current()
    }

    pub fn target_view_pos(&self) -> f64 {
        self.column_x(self.active_column_idx) + self.view_offset.target()
    }

    /// Working area used for tiled columns (exclusive zone plus struts).
    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    /// Animate the camera so a strip rectangle `[strip_x, strip_x + width]` is
    /// centered in the working area. Does not move the window. Clamped to the
    /// free-pan strip; `user_panned_view` keeps later commits from yanking back.
    pub fn animate_view_to_center_strip(&mut self, strip_x: f64, width: f64) {
        let centered =
            self.compute_new_view_offset_centered(None, strip_x, width, SizingMode::Normal);
        let target_view_pos = strip_x + centered;
        self.user_panned_view = true;
        if self.columns.is_empty() {
            self.animate_view_offset(0, target_view_pos);
            return;
        }
        let col_x = self.column_x(self.active_column_idx);
        self.animate_view_offset(self.active_column_idx, target_view_pos - col_x);
    }

    // HACK: pass a self.data iterator in manually as a workaround for the lack of method partial
    // borrowing. Note that this method's return value does not borrow the entire &Self!
    fn column_xs(&self, data: impl Iterator<Item = ColumnData>) -> impl Iterator<Item = f64> {
        let gaps = self.column_gap();
        let mut x = 0.;

        // Chain with a dummy value to be able to get one past all columns' X.
        let dummy = ColumnData { width: 0. };
        let data = data.chain(iter::once(dummy));
        let insert_col = match self.live_reflow_gap {
            Some(LiveReflowGap {
                position: InsertPosition::NewColumn(idx),
                width,
                ..
            }) => Some((idx, width)),
            _ => None,
        };

        data.enumerate().map(move |(idx, data)| {
            if let Some((insert_idx, width)) = insert_col {
                if idx == insert_idx {
                    x += width + gaps;
                }
            }
            let rv = x;
            x += data.width + gaps;
            rv
        })
    }

    fn column_x(&self, column_idx: usize) -> f64 {
        self.column_xs(self.data.iter().copied())
            .nth(column_idx)
            .unwrap()
    }

    fn column_xs_in_render_order(
        &self,
        data: impl Iterator<Item = ColumnData>,
    ) -> impl Iterator<Item = f64> {
        let active_idx = self.active_column_idx;
        let active_pos = self.column_x(active_idx);
        let offsets = self
            .column_xs(data)
            .enumerate()
            .filter_map(move |(idx, pos)| (idx != active_idx).then_some(pos));
        iter::once(active_pos).chain(offsets)
    }

    pub fn columns(&self) -> impl Iterator<Item = &Column<W>> {
        self.columns.iter()
    }

    fn columns_mut(&mut self) -> impl Iterator<Item = (&mut Column<W>, f64)> + '_ {
        let offsets = self.column_xs(self.data.iter().copied());
        zip(&mut self.columns, offsets)
    }

    fn columns_in_render_order(&self) -> impl Iterator<Item = (&Column<W>, f64)> + '_ {
        let offsets = self.column_xs_in_render_order(self.data.iter().copied());

        let (first, active, rest) = if self.columns.is_empty() {
            (&[][..], &[][..], &[][..])
        } else {
            let (first, rest) = self.columns.split_at(self.active_column_idx);
            let (active, rest) = rest.split_at(1);
            (first, active, rest)
        };

        let columns = active.iter().chain(first).chain(rest);
        zip(columns, offsets)
    }

    fn columns_in_render_order_mut(&mut self) -> impl Iterator<Item = (&mut Column<W>, f64)> + '_ {
        let offsets = self.column_xs_in_render_order(self.data.iter().copied());

        let (first, active, rest) = if self.columns.is_empty() {
            (&mut [][..], &mut [][..], &mut [][..])
        } else {
            let (first, rest) = self.columns.split_at_mut(self.active_column_idx);
            let (active, rest) = rest.split_at_mut(1);
            (first, active, rest)
        };

        let columns = active.iter_mut().chain(first).chain(rest);
        zip(columns, offsets)
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> {
        let scale = self.scale;
        let view_off = Point::from((-self.view_pos(), 0.));
        self.columns_in_render_order()
            .flat_map(move |(col, col_x)| {
                let col_off = Point::from((col_x, 0.));
                let col_render_off = col.render_offset();
                col.tiles_in_render_order()
                    .map(move |(tile, tile_off, visible)| {
                        let pos =
                            view_off + col_off + col_render_off + tile_off + tile.render_offset();
                        // Round to physical pixels.
                        let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        (tile, pos, visible)
                    })
            })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        let view_off = Point::from((-self.view_pos(), 0.));
        self.columns_in_render_order_mut()
            .flat_map(move |(col, col_x)| {
                let col_off = Point::from((col_x, 0.));
                let col_render_off = col.render_offset();
                col.tiles_in_render_order_mut()
                    .map(move |(tile, tile_off)| {
                        let mut pos =
                            view_off + col_off + col_render_off + tile_off + tile.render_offset();
                        // Round to physical pixels.
                        if round {
                            pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        }
                        (tile, pos)
                    })
            })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        // Include tile_pos relative to the current workspace view so IPC clients
        // (and WindowLayoutsChanged) track view_offset during trackpad gestures.
        // Omit per-tile animated render offsets to avoid spam from bobbing/etc.;
        // view_pos changes still update every tile's position.
        let scale = self.scale;
        let view_off = Point::from((-self.view_pos(), 0.));
        let offsets = self.column_xs(self.data.iter().copied());
        zip(&self.columns, offsets)
            .enumerate()
            .flat_map(move |(col_idx, (col, col_x))| {
                let col_off = Point::from((col_x, 0.));
                col.tiles()
                    .enumerate()
                    .map(move |(tile_idx, (tile, tile_off))| {
                        let pos = view_off + col_off + tile_off;
                        let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                        let layout = WindowLayout {
                            // Our indices are 1-based, consistent with the actions.
                            pos_in_scrolling_layout: Some((col_idx + 1, tile_idx + 1)),
                            tile_pos_in_workspace_view: Some(pos.into()),
                            ..tile.ipc_layout_template()
                        };
                        (tile, layout)
                    })
            })
    }

    fn live_reflow_hint_area(&self, gap: LiveReflowGap) -> Option<Rectangle<f64, Logical>> {
        let gaps = self.options.layout.gaps;
        let y = self.working_area.loc.y + gaps;
        let h = (self.working_area.size.h - gaps * 2.).max(1.);

        if let Some((col_idx, _)) = gap.squeeze {
            let col = self.columns.get(col_idx)?;
            let col_w = self.unsqueezed_col_width(col_idx);
            let x0 = self.column_x(col_idx);
            let half = ((col_w - gaps) / 2.).max(1.);
            let insert_left =
                matches!(gap.position, InsertPosition::NewColumn(idx) if idx <= col_idx);
            let x = if insert_left { x0 } else { x0 + col_w - half };
            return Some(Rectangle::new(
                Point::from((x + col.tiles_origin().x, y)),
                Size::from((half, h)),
            ));
        }

        match gap.position {
            InsertPosition::NewColumn(idx) => {
                if self.columns.is_empty() && idx != 0 {
                    return None;
                }
                // Shadow matches the current (possibly drag-spread) gutter, not
                // the dragged window width — leftover dests do not open a slot.
                let w = self.column_gap().max(1.);
                let x = if self.columns.is_empty() {
                    0.
                } else if idx < self.columns.len() {
                    self.column_x(idx) - w
                } else {
                    let last = self.columns.len() - 1;
                    self.column_x(last) + self.data[last].width
                };
                Some(Rectangle::new(Point::from((x, y)), Size::from((w, h))))
            }
            InsertPosition::InColumn(col_idx, tile_idx) => {
                let col = self.columns.get(col_idx)?;
                if tile_idx > col.tiles.len() {
                    return None;
                }
                let col_w = self.unsqueezed_col_width(col_idx).max(1.);
                let x = self.column_x(col_idx) + col.tiles_origin().x;
                if gap.origin {
                    let gap_h = col
                        .live_reflow_tile_gap
                        .map(|(_, h)| h)
                        .unwrap_or(gap.height)
                        .max(1.);
                    let hy = if col.tiles.is_empty() {
                        col.tiles_origin().y
                    } else {
                        col.tile_offset(tile_idx.min(col.tiles.len())).y - gap_h - gaps
                    };
                    Some(Rectangle::new(
                        Point::from((x, hy)),
                        Size::from((col_w, gap_h)),
                    ))
                } else {
                    let half_h = ((h - gaps) / 2.).max(1.);
                    let hy = if tile_idx == 0 { y } else { y + h - half_h };
                    Some(Rectangle::new(
                        Point::from((x, hy)),
                        Size::from((col_w, half_h)),
                    ))
                }
            }
            InsertPosition::Floating => None,
        }
    }

    pub(super) fn insert_hint_area(
        &self,
        position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        let mut hint_area = if let Some(gap) = self.drop_dest.or(self.live_reflow_gap) {
            self.live_reflow_hint_area(gap)?
        } else {
            match position {
                InsertPosition::NewColumn(column_index) => {
                    if column_index == 0 || column_index == self.columns.len() {
                        let size = Size::from((
                            300.,
                            self.working_area.size.h - self.options.layout.gaps * 2.,
                        ));
                        let mut loc = Point::from((
                            self.column_x(column_index),
                            self.working_area.loc.y + self.options.layout.gaps,
                        ));
                        if column_index == 0 && !self.columns.is_empty() {
                            loc.x -= size.w + self.options.layout.gaps;
                        }
                        Rectangle::new(loc, size)
                    } else if column_index > self.columns.len() {
                        error!("insert hint column index is out of range");
                        return None;
                    } else {
                        let size = Size::from((
                            300.,
                            self.working_area.size.h - self.options.layout.gaps * 2.,
                        ));
                        let loc = Point::from((
                            self.column_x(column_index)
                                - size.w / 2.
                                - self.options.layout.gaps / 2.,
                            self.working_area.loc.y + self.options.layout.gaps,
                        ));
                        Rectangle::new(loc, size)
                    }
                }
                InsertPosition::InColumn(column_index, tile_index) => {
                    if column_index > self.columns.len() {
                        error!("insert hint column index is out of range");
                        return None;
                    }

                    let col = &self.columns[column_index];
                    if tile_index > col.tiles.len() {
                        error!("insert hint tile index is out of range");
                        return None;
                    }

                    let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;

                    let (height, y) = if is_tabbed {
                        // In tabbed mode, there's only one tile visible, and we want to draw the hint
                        // at its top or bottom.
                        let top = col.tile_offset(col.active_tile_idx).y;
                        let bottom = top + col.data[col.active_tile_idx].size.h;

                        if tile_index <= col.active_tile_idx {
                            (150., top)
                        } else {
                            (150., bottom - 150.)
                        }
                    } else {
                        let top = col.tile_offset(tile_index).y;

                        if tile_index == 0 {
                            (150., top)
                        } else if tile_index == col.tiles.len() {
                            (150., top - self.options.layout.gaps - 150.)
                        } else {
                            (300., top - self.options.layout.gaps / 2. - 150.)
                        }
                    };

                    // Adjust for place-within-column tab indicator.
                    let origin_x = col.tiles_origin().x;
                    let extra_w = if is_tabbed && col.sizing_mode().is_normal() {
                        col.tab_indicator.extra_size(col.tiles.len(), col.scale).w
                    } else {
                        0.
                    };

                    let size = Size::from((self.data[column_index].width - extra_w, height));
                    let loc = Point::from((self.column_x(column_index) + origin_x, y));
                    Rectangle::new(loc, size)
                }
                InsertPosition::Floating => return None,
            }
        };

        // First window on an empty workspace will cancel out any view offset. Replicate this
        // effect here.
        if self.columns.is_empty() {
            let view_offset = if self.is_centering_focused_column() {
                self.compute_new_view_offset_centered(
                    Some(0.),
                    0.,
                    hint_area.size.w,
                    SizingMode::Normal,
                )
            } else {
                self.compute_new_view_offset_fit(Some(0.), 0., hint_area.size.w, SizingMode::Normal)
            };
            hint_area.loc.x -= view_offset;
        } else {
            hint_area.loc.x -= self.view_pos();
        }

        Some(hint_area)
    }

    /// Returns the geometry of the active tile relative to and clamped to the view.
    ///
    /// During animations, assumes the final view position.
    pub fn active_tile_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        let col = self.columns.get(self.active_column_idx)?;

        let final_view_offset = self.view_offset.target();
        let view_off = Point::from((-final_view_offset, 0.));

        let (tile, tile_off) = col.tiles().nth(col.active_tile_idx).unwrap();

        let tile_pos = view_off + tile_off;
        let tile_size = tile.tile_size();
        let tile_rect = Rectangle::new(tile_pos, tile_size);

        let view = Rectangle::from_size(self.view_size);
        view.intersection(tile_rect)
    }

    pub fn popup_target_rect(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        for col in &self.columns {
            for (tile, pos) in col.tiles() {
                if tile.window().id() == id {
                    // In the scrolling layout, we try to position popups horizontally within the
                    // window geometry (so they remain visible even if the window scrolls flush with
                    // the left/right edge of the screen), and vertically within the whole parent
                    // working area.
                    let width = tile.window_size().w;
                    let height = self.parent_area.size.h;

                    let mut target = Rectangle::from_size(Size::from((width, height)));
                    target.loc.y += self.parent_area.loc.y;
                    target.loc.y -= pos.y;
                    target.loc.y -= tile.window_loc().y;

                    return Some(target);
                }
            }
        }
        None
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = self.active_column_idx;
        {
            let col = &mut self.columns[col_idx];
            col.toggle_width(None, forwards);
            col.natural_width = col.width();
            col.split_id = None;
            cancel_resize_for_column(&mut self.interactive_resize, col);
        }
        self.data[col_idx].update(&self.columns[col_idx]);
        self.enforce_cap(col_idx, true);
    }

    pub fn toggle_full_width(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = self.active_column_idx;

        // Turning off full-width / maximized never takes space from neighbors.
        if self.columns[col_idx].is_full_width || self.columns[col_idx].is_pending_maximized {
            {
                let col = &mut self.columns[col_idx];
                col.toggle_full_width();
                cancel_resize_for_column(&mut self.interactive_resize, col);
            }
            self.data[col_idx].update(&self.columns[col_idx]);
            self.columns[col_idx].natural_width = self.columns[col_idx].width();
            return;
        }

        let leftover = (self.cap_width() - self.occupied_width()).max(0.);
        let full = self.columns[col_idx].resolve_column_width(ColumnWidth::Proportion(1.));
        // Use the stored column width target, not cached tile size — tiles may
        // still be catching up after a resize, and is_full_width resolves to
        // Proportion(1.) which would shrink a Fixed(>1vp) column.
        let current = self.columns[col_idx].resolve_column_width(self.columns[col_idx].width);
        // Already wider than one viewport: do not set is_full_width.
        if current > full + 1. {
            return;
        }
        let need = (full - current).max(0.);

        if need <= leftover + 1. {
            {
                let col = &mut self.columns[col_idx];
                col.toggle_full_width();
                cancel_resize_for_column(&mut self.interactive_resize, col);
            }
            self.data[col_idx].update(&self.columns[col_idx]);
            return;
        }

        // Not enough leftover for a full viewport: grow only into remaining
        // space, never shrink other columns. At the cap this is a no-op.
        if leftover <= 1. {
            return;
        }

        {
            let col = &mut self.columns[col_idx];
            col.width = ColumnWidth::Fixed(current + leftover);
            col.preset_width_idx = None;
            col.is_full_width = false;
            col.natural_width = current + leftover;
            col.update_tile_sizes(true);
            cancel_resize_for_column(&mut self.interactive_resize, col);
        }
        self.data[col_idx].update(&self.columns[col_idx]);
    }

    /// Fill the working area including layout gaps (wallpaper sobrinha).
    /// Unlike `toggle_full_width`, this never grows only into leftover space,
    /// and edge-maximize is converted to gapped full-width instead of restored.
    pub fn set_full_width_gapped(&mut self, full: bool) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = self.active_column_idx;
        if full {
            let target = self.columns[col_idx].resolve_column_width(ColumnWidth::Proportion(1.));
            let current = self.columns[col_idx].resolve_column_width(self.columns[col_idx].width);
            // Client maximize / yellow FullViewport must not shrink a column
            // that the user already made wider than one viewport.
            if current > target + 1. {
                return;
            }
        }
        {
            let col = &mut self.columns[col_idx];
            cancel_resize_for_column(&mut self.interactive_resize, col);
            if col.is_pending_maximized {
                col.set_maximized(false);
            }
            col.is_full_width = full;
            col.update_tile_sizes(true);
        }
        self.data[col_idx].update(&self.columns[col_idx]);
        if full {
            self.columns[col_idx].natural_width = self.columns[col_idx].width();
        }
    }

    pub fn toggle_full_width_gapped(&mut self) {
        if self.columns.is_empty() {
            return;
        }
        let col = &self.columns[self.active_column_idx];
        if col.is_pending_maximized {
            self.set_full_width_gapped(true);
            return;
        }
        let already = col.is_full_width;
        self.set_full_width_gapped(!already);
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if self.columns.is_empty() {
            return;
        }

        let (col_idx, tile_idx) = if let Some(window) = window {
            self.columns
                .iter()
                .enumerate()
                .find_map(|(col_idx, col)| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col_idx, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (self.active_column_idx, None)
        };

        {
            let col = &mut self.columns[col_idx];
            col.set_column_width(change, tile_idx, true);
            col.natural_width = col.width();
            col.split_id = None;
            cancel_resize_for_column(&mut self.interactive_resize, col);
        }
        self.data[col_idx].update(&self.columns[col_idx]);
        self.enforce_cap(col_idx, true);
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if self.columns.is_empty() {
            return;
        }

        let (col, tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .find_map(|col| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (&mut self.columns[self.active_column_idx], None)
        };

        col.set_window_height(change, tile_idx, true);

        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        if self.columns.is_empty() {
            return;
        }

        let (col, tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .find_map(|col| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (&mut self.columns[self.active_column_idx], None)
        };

        col.reset_window_height(tile_idx);

        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        if self.columns.is_empty() {
            return;
        }

        let (col, tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .find_map(|col| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (&mut self.columns[self.active_column_idx], None)
        };

        col.toggle_width(tile_idx, forwards);

        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        if self.columns.is_empty() {
            return;
        }

        let (col, tile_idx) = if let Some(window) = window {
            self.columns
                .iter_mut()
                .find_map(|col| {
                    col.tiles
                        .iter()
                        .position(|tile| tile.window().id() == window)
                        .map(|tile_idx| (col, Some(tile_idx)))
                })
                .unwrap()
        } else {
            (&mut self.columns[self.active_column_idx], None)
        };

        col.toggle_window_height(tile_idx, forwards);

        cancel_resize_for_column(&mut self.interactive_resize, col);
    }

    pub fn expand_column_to_available_width(&mut self) {
        if self.columns.is_empty() {
            return;
        }

        let col_idx = self.active_column_idx;
        if !self.columns[col_idx].pending_sizing_mode().is_normal()
            || self.columns[col_idx].is_full_width
        {
            return;
        }

        if self.is_centering_focused_column() {
            // Always-centered mode is different since the active window position cannot be
            // controlled (it's always at the center). I guess you could come up with different
            // logic here that computes the width in such a way so as to leave nearby columns fully
            // on screen while taking into account that the active column will remain centered
            // after resizing. But I'm not sure it's that useful? So let's do the simple thing.
            // Never shrink a column already wider than one viewport.
            let full = self.columns[col_idx].resolve_column_width(ColumnWidth::Proportion(1.));
            let current = self.columns[col_idx].resolve_column_width(self.columns[col_idx].width);
            if current <= full + 1. {
                let col = &mut self.columns[col_idx];
                col.toggle_full_width();
                cancel_resize_for_column(&mut self.interactive_resize, col);
            }
            return;
        }

        // NOTE: This logic won't work entirely correctly with small fixed-size maximized windows
        // (they have a different area and padding).

        // Consider the end of an ongoing animation because that's what compute to fit does too.
        let view_x = self.target_view_pos();
        let working_x = self.working_area.loc.x;
        let working_w = self.working_area.size.w;

        // Count all columns that are fully visible inside the working area.
        let mut width_taken = 0.;
        let mut leftmost_col_x = None;
        let mut active_col_x = None;
        let mut counted_non_active_column = false;

        let gap = self.options.layout.gaps;
        let col_xs = self.column_xs(self.data.iter().copied());
        for (idx, col_x) in col_xs.take(self.columns.len()).enumerate() {
            if col_x < view_x + working_x + gap {
                // Column goes off-screen to the left.
                continue;
            }

            leftmost_col_x.get_or_insert(col_x);

            let width = self.data[idx].width;
            if view_x + working_x + working_w < col_x + width + gap {
                // Column goes off-screen to the right. We can stop here.
                break;
            }

            if idx == self.active_column_idx {
                active_col_x = Some(col_x);
            } else {
                counted_non_active_column = true;
            }

            width_taken += width + gap;
        }

        if active_col_x.is_none() {
            // The active column wasn't fully on screen, so we can't meaningfully do anything.
            return;
        }

        let col = &mut self.columns[self.active_column_idx];

        let available_width = working_w - gap - width_taken - col.extra_size().w;
        if available_width <= 0. {
            // Nowhere to expand.
            return;
        }

        cancel_resize_for_column(&mut self.interactive_resize, col);

        if !counted_non_active_column {
            // Only the active column was fully on-screen (maybe it's the only column), so we're
            // about to set its width to 100% of the working area. Let's do it via
            // toggle_full_width() as it lets you back out of it more intuitively.
            // Skip when already wider than one viewport (would shrink via is_full_width).
            let full = self.columns[self.active_column_idx]
                .resolve_column_width(ColumnWidth::Proportion(1.));
            let current = self.columns[self.active_column_idx]
                .resolve_column_width(self.columns[self.active_column_idx].width);
            if current <= full + 1. {
                let col = &mut self.columns[self.active_column_idx];
                col.toggle_full_width();
            }
            return;
        }

        let active_width = self.data[self.active_column_idx].width;
        col.width = ColumnWidth::Fixed(active_width + available_width);
        col.preset_width_idx = None;
        col.is_full_width = false;
        col.update_tile_sizes(true);

        // Put the leftmost window into the view.
        let new_view_x = leftmost_col_x.unwrap() - gap - working_x;
        self.animate_view_offset(self.active_column_idx, new_view_x - active_col_x.unwrap());
        // Just in case.
        self.animate_view_offset_to_column(None, self.active_column_idx, None);
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) -> bool {
        let mut col_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if is_fullscreen == self.columns[col_idx].is_pending_fullscreen {
            return false;
        }
        if self.columns[col_idx].group.is_some() {
            return false;
        }

        let mut col = &mut self.columns[col_idx];
        let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;

        cancel_resize_for_column(&mut self.interactive_resize, col);

        if is_fullscreen && (col.tiles.len() > 1 && !is_tabbed) {
            // This wasn't the only window in its column; extract it into a separate column.
            self.consume_or_expel_window_right(Some(window));
            col_idx += 1;
            col = &mut self.columns[col_idx];
        }

        col.set_fullscreen(is_fullscreen);

        // With place_within_column, the tab indicator changes the column size immediately.
        self.data[col_idx].update(col);

        true
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) -> bool {
        let mut col_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        if maximize == self.columns[col_idx].is_pending_maximized {
            return false;
        }
        if self.columns[col_idx].group.is_some() {
            return false;
        }

        let mut col = &mut self.columns[col_idx];
        let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;

        cancel_resize_for_column(&mut self.interactive_resize, col);

        if maximize && (col.tiles.len() > 1 && !is_tabbed) {
            // This wasn't the only window in its column; extract it into a separate column.
            self.consume_or_expel_window_right(Some(window));
            col_idx += 1;
            col = &mut self.columns[col_idx];
        }

        col.set_maximized(maximize);

        // With place_within_column, the tab indicator changes the column size immediately.
        self.data[col_idx].update(col);

        true
    }

    pub fn render_above_top_layer(&self) -> bool {
        // Render above the top layer if we're on a fullscreen window and the view is stationary.
        if self.columns.is_empty() {
            return false;
        }

        if !self.view_offset.is_static() {
            return false;
        }

        self.columns[self.active_column_idx]
            .sizing_mode()
            .is_fullscreen()
    }

    /// Horizontal view pan is locked while the active column is pending fullscreen.
    fn view_offset_locked(&self) -> bool {
        !self.columns.is_empty() && self.columns[self.active_column_idx].is_pending_fullscreen()
    }

    pub fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(ScrollingSpaceRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);

        // Draw the closing windows on top of the other windows.
        let view_rect = Rectangle::new(Point::from((self.view_pos(), 0.)), self.view_size);
        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            push(elem.into());
        }

        if self.columns.is_empty() {
            return;
        }

        let mut first = true;

        // This matches self.tiles_in_render_order().
        let view_off = Point::from((-self.view_pos(), 0.));
        for (col, col_x) in self.columns_in_render_order() {
            let col_off = Point::from((col_x, 0.));
            let col_render_off = col.render_offset();

            // Draw the tab indicator on top.
            {
                let pos = view_off + col_off + col_render_off;
                let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                col.tab_indicator
                    .render(ctx.renderer, pos, &mut |elem| push(elem.into()));
            }

            for (tile, tile_off, visible) in col.tiles_in_render_order() {
                let tile_pos =
                    view_off + col_off + col_render_off + tile_off + tile.render_offset();
                // Round to physical pixels.
                let tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                // And now the drawing logic.

                // For the active tile (which comes first), draw the focus ring.
                let focus_ring = focus_ring && first;
                first = false;

                // In the scrolling layout, we currently use visible only for hidden tabs in the
                // tabbed mode. We want to animate their opacity when going in and out of tabbed
                // mode, so we don't want to apply "visible" immediately. However, "visible" is
                // also used for input handling, and there we *do* want to apply it immediately.
                // So, let's just selectively ignore "visible" here when animating alpha.
                let visible = visible || tile.alpha_animation.is_some();
                if !visible {
                    continue;
                }

                let xray_pos = xray_pos.offset(tile_pos);
                tile.render(ctx.r(), tile_pos, xray_pos, focus_ring, &mut |elem| {
                    push(elem.into())
                });
            }

            if let (Some(group), Some(bar)) = (&col.group, col.group_side_bar()) {
                let pos = view_off + col_off + col_render_off + col.tiles_origin() + bar.loc;
                let pos = pos.to_physical_precise_round(scale).to_logical(scale);
                let clip_bottom = col
                    .tiles
                    .first()
                    .and_then(|t| t.bottom_clip())
                    .filter(|_| !self.render_above_top_layer());
                group.chrome.set_clips(clip_bottom, None);
                if self.render_above_top_layer() {
                    group.chrome.render(ctx.renderer, pos, &mut |elem| {
                        push(GroupChromeLayer::Chrome(elem).into())
                    });
                } else {
                    group
                        .chrome
                        .render_layers(ctx.r(), pos, xray_pos.offset(pos), &mut |elem| {
                            push(elem.into())
                        });
                }
                group.chrome.set_clips(None, None);
            }
        }
    }

    /// Side and bottom bar rects of the tiled groups, workspace view coordinates.
    pub fn group_chrome_rects(&self) -> Vec<Rectangle<f64, Logical>> {
        if self.render_above_top_layer() {
            return Vec::new();
        }
        let view_off = Point::from((-self.view_pos(), 0.));
        let scale = self.scale;
        self.columns_in_render_order()
            .filter_map(|(col, col_x)| {
                let group = col.group.as_ref()?;
                let bar = col.group_side_bar()?;
                let pos = view_off + Point::from((col_x, 0.)) + col.render_offset();
                let pos = (pos + col.tiles_origin() + bar.loc)
                    .to_physical_precise_round(scale)
                    .to_logical(scale);
                Some(group.chrome.rects(pos))
            })
            .flatten()
            .collect()
    }

    /// Join zone of a tiled group column under `pos` (workspace view coordinates) for a dragged
    /// window: a target member and the hint. Full groups have no zones.
    pub fn group_join_target(
        &self,
        pos: Point<f64, Logical>,
        incoming: CellLimits,
    ) -> Option<(W::Id, FloatingJoinHint)> {
        let gap = self.options.layout.gaps;
        let view_off = Point::from((-self.view_pos(), 0.));
        for (col, col_x) in self.columns_in_render_order() {
            let Some(group) = &col.group else {
                continue;
            };
            if !col.sizing_mode().is_normal() {
                continue;
            }
            let origin = view_off + Point::from((col_x, 0.)) + col.render_offset();
            let origin = origin + col.tiles_origin() + Point::from((group_extra(gap).w, 0.));
            let mut bbox = group.bbox();
            bbox.loc += origin;
            if !bbox.contains(pos) {
                continue;
            }
            let cells: Vec<_> = group
                .join_base_cells(gap)
                .into_iter()
                .map(|mut c| {
                    c.loc += origin;
                    c
                })
                .collect();
            let limits: Vec<_> = col
                .tiles
                .iter()
                .map(|t| cell_limits_of(t.min_size_nonfullscreen(), t.max_size_nonfullscreen()))
                .collect();
            let (idx, hint) = group_join_at(&cells, &limits, pos, gap, incoming)?;
            return Some((col.tiles[idx].window().id().clone(), hint));
        }
        None
    }

    /// Insert `tile` into the tiled group of `target` by `op`, from the cells of the join
    /// zones. Gives the tile back when the group is full, gone, or the sizes don't fit.
    pub fn join_group(
        &mut self,
        target: &W::Id,
        op: FloatingJoinOp,
        mut tile: Tile<W>,
    ) -> Result<(), Tile<W>> {
        let Some(col_idx) = self.column_idx_of(target) else {
            return Err(tile);
        };
        let gaps = self.options.layout.gaps;
        let incoming = cell_limits_of(tile.min_size_nonfullscreen(), tile.max_size_nonfullscreen());
        let col = &mut self.columns[col_idx];
        let Some(tidx) = col.position(target) else {
            return Err(tile);
        };
        let Some(group) = &mut col.group else {
            return Err(tile);
        };
        if col.tiles.len() >= MAX_GROUP_MEMBERS {
            return Err(tile);
        }
        let base = group.join_base_cells(gaps);
        let member_limits: Vec<_> = col
            .tiles
            .iter()
            .map(|t| cell_limits_of(t.min_size_nonfullscreen(), t.max_size_nonfullscreen()))
            .collect();
        let Some((mut cells, incoming_cell)) =
            join_cells(&base, &member_limits, tidx, op, gaps, incoming)
        else {
            return Err(tile);
        };
        cells.push(incoming_cell);
        group.reserved = None;
        group.set_all_cells(cells);
        group.restore_width = None;

        tile.update_config(col.view_size, col.scale, col.options.clone());
        col.data.push(TileData::new(&tile, WindowHeight::auto_1()));
        col.tiles.push(tile);
        let new_idx = col.tiles.len() - 1;
        col.update_tile_sizes(true);
        col.activate_idx(new_idx);
        self.data[col_idx].update(col);
        if self.active_column_idx != col_idx {
            self.activate_column(col_idx);
        }
        Ok(())
    }

    /// Pull `window` out of its tiled group but keep its cell reserved, so the group keeps its
    /// box until [`Self::release_reserved_cell`] or [`Self::restore_group_member`].
    pub fn remove_group_member_reserving(&mut self, window: &W::Id) -> RemovedTile<W> {
        self.set_live_reflow_gap(None);
        let col_idx = self.column_idx_of(window).unwrap();
        if let Some(resize) = &self.interactive_resize {
            if &resize.window == window {
                self.interactive_resize = None;
            }
        }
        let column = &mut self.columns[col_idx];
        let tile_idx = column.position(window).unwrap();
        let tile = column.tiles.remove(tile_idx);
        column.data.remove(tile_idx);
        if let Some(group) = &mut column.group {
            let cell = group.cells.remove(tile_idx);
            group.reserved = Some(cell);
        }
        if tile_idx < column.active_tile_idx {
            column.active_tile_idx -= 1;
        } else if column.active_tile_idx >= column.tiles.len() {
            column.activate_idx(column.tiles.len() - 1);
        }
        column.update_tile_sizes(false);
        self.data[col_idx].update(column);

        let width = ColumnWidth::Fixed(tile.tile_expected_or_current_size().w);
        RemovedTile {
            tile,
            width,
            is_full_width: false,
            is_floating: false,
        }
    }

    /// Resolve a deferred ungroup of `member`'s group: the reserved cell is freed (the others
    /// redistribute, or the group dissolves into a normal column).
    pub fn release_reserved_cell(&mut self, member: &W::Id) -> bool {
        let Some(col_idx) = self.column_idx_of(member) else {
            return false;
        };
        let col = &mut self.columns[col_idx];
        let extra_w = col.group_extra_w();
        let Some(group) = &mut col.group else {
            return false;
        };
        let Some(removed) = group.reserved else {
            return false;
        };
        let total_w = extra_w + group.bbox().size.w;
        group.reserved = None;
        col.group_cell_freed(removed, total_w);
        col.update_tile_sizes(true);
        self.data[col_idx].update(col);
        true
    }

    /// Put `tile` back into the tiled group of the other members of `cells` (tile order,
    /// content coordinates, the picked window included) with exactly those cells. Works when
    /// the others are still that group (possibly with a reserved cell) or, for a pair, the
    /// normal column the group dissolved into.
    pub fn restore_group_member(
        &mut self,
        mut tile: Tile<W>,
        cells: &[(W::Id, Rectangle<f64, Logical>)],
    ) -> Result<(), Tile<W>> {
        let picked = tile.window().id().clone();
        let Some(tile_idx) = cells.iter().position(|(id, _)| *id == picked) else {
            return Err(tile);
        };
        let others: Vec<_> = cells
            .iter()
            .filter(|(id, _)| *id != picked)
            .map(|(id, _)| id.clone())
            .collect();
        if others.is_empty() || cells.len() > MAX_GROUP_MEMBERS {
            return Err(tile);
        }
        let Some(col_idx) = self.column_idx_of(&others[0]) else {
            return Err(tile);
        };
        let col = &mut self.columns[col_idx];
        let ids: Vec<_> = col.tiles.iter().map(|t| t.window().id().clone()).collect();
        let ok = ids == others
            && col.sizing_mode().is_normal()
            && (col.group.is_some() || (others.len() == 1 && col.tiles.len() == 1));
        if !ok {
            return Err(tile);
        }

        tile.update_config(col.view_size, col.scale, col.options.clone());
        col.data
            .insert(tile_idx, TileData::new(&tile, WindowHeight::auto_1()));
        col.tiles.insert(tile_idx, tile);
        let cells = normalize_cells(cells.iter().map(|(_, c)| *c).collect());
        let width = col.group_extra_w() + union_all(&cells).size.w;
        match &mut col.group {
            Some(group) => {
                group.reserved = None;
                group.cells = cells;
                group.restore_width = None;
            }
            None => col.group = Some(TiledGroup::new(cells)),
        }
        col.display_mode = ColumnDisplay::Normal;
        col.width = ColumnWidth::Fixed(width);
        col.is_full_width = false;
        col.preset_width_idx = None;
        col.update_tile_sizes(true);
        col.activate_idx(tile_idx);
        self.data[col_idx].update(col);
        if self.active_column_idx != col_idx {
            self.activate_column(col_idx);
        }
        Ok(())
    }

    /// Tiled group bar under `pos` (workspace view coordinates): a member id and what was hit.
    pub fn group_bar_under(&self, pos: Point<f64, Logical>) -> Option<(W::Id, GroupBarHit)> {
        let view_off = Point::from((-self.view_pos(), 0.));
        for (col, col_x) in self.columns_in_render_order() {
            let Some(group) = &col.group else {
                continue;
            };
            let Some(bar) = col.group_side_bar() else {
                continue;
            };
            let origin = view_off + Point::from((col_x, 0.)) + col.render_offset();
            let origin = origin + col.tiles_origin() + bar.loc;
            let bottom_w = bottom_bar_w(group.bbox(), self.options.layout.gaps);
            if let Some(hit) = group_bar_hit_at(bar.size.h, bottom_w, pos - origin) {
                let idx = match hit {
                    GroupBarHit::Edge(edges) => member_on_edges(&group.cells, edges).unwrap_or(0),
                    _ => 0,
                };
                let tile = col.tiles.get(idx).unwrap_or(&col.tiles[0]);
                return Some((tile.window().id().clone(), hit));
            }
        }
        None
    }

    /// Update the hovered tiled group bar ball. Returns true if anything changed.
    pub fn update_group_bar_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let hovered = pos.and_then(|pos| {
            let (id, hit) = self.group_bar_under(pos)?;
            let GroupBarHit::Ball(action) = hit else {
                return None;
            };
            let i = (0..super::window_chrome::BALL_COUNT)
                .find(|&i| super::window_chrome::ball_action(i) == action)?;
            Some((id, i))
        });
        let mut changed = false;
        for col in &mut self.columns {
            let ball = hovered
                .as_ref()
                .filter(|(id, _)| col.contains(id))
                .map(|(_, i)| *i);
            if let Some(group) = &mut col.group {
                changed |= group.chrome.set_hover(ball);
            }
        }
        changed
    }

    /// Whether `window` is a member of a tiled group column.
    pub fn is_in_group(&self, window: &W::Id) -> bool {
        self.columns
            .iter()
            .any(|col| col.group.is_some() && col.contains(window))
    }

    /// Index of the column containing `window`.
    pub fn column_idx_of(&self, window: &W::Id) -> Option<usize> {
        self.columns.iter().position(|col| col.contains(window))
    }

    /// Lime Half on member `window` of a tiled group, inside the same group box (see
    /// [`half_member`]).
    pub fn group_member_half(&mut self, window: &W::Id) -> bool {
        let Some(col_idx) = self.column_idx_of(window) else {
            return false;
        };
        let gaps = self.options.layout.gaps;
        let col = &mut self.columns[col_idx];
        let Some(tile_idx) = col.tiles.iter().position(|t| t.window().id() == window) else {
            return false;
        };
        let n_tiles = col.tiles.len();
        let member_limits: Vec<_> = col
            .tiles
            .iter()
            .map(|t| cell_limits_of(t.min_size_nonfullscreen(), t.max_size_nonfullscreen()))
            .collect();
        let Some(group) = &mut col.group else {
            return false;
        };
        if group.reserved.is_some() || group.cells.len() != n_tiles {
            return false;
        }
        group.cells = normalize_cells(half_member(&group.cells, &member_limits, tile_idx, gaps));
        col.update_tile_sizes(true);
        self.data[col_idx].update(col);
        true
    }

    /// Strip X and width of column `col_idx`.
    pub fn column_strip_span(&self, col_idx: usize) -> (f64, f64) {
        (self.column_x(col_idx), self.data[col_idx].width)
    }

    /// Column insert index for a group whose left edge is at strip `x`: before the first
    /// column whose center is right of `x`.
    pub fn insert_idx_for_strip_x(&self, x: f64) -> usize {
        (0..self.columns.len())
            .filter(|&i| self.column_x(i) + self.data[i].width / 2. < x)
            .count()
    }

    /// Members of `window`'s tiled group with their cells relative to the group content
    /// origin, in tile order.
    pub fn group_cells(&self, window: &W::Id) -> Vec<(W::Id, Rectangle<f64, Logical>)> {
        let Some(col) = self.columns.iter().find(|col| col.contains(window)) else {
            return Vec::new();
        };
        let Some(group) = &col.group else {
            return Vec::new();
        };
        zip(&col.tiles, &group.cells)
            .map(|(tile, cell)| (tile.window().id().clone(), *cell))
            .collect()
    }

    /// Add a tiled group column at `idx`. `members` cells may be in any coordinates; they are
    /// normalized, capped to the band width and scaled to the band height.
    pub fn add_group_column(
        &mut self,
        idx: usize,
        members: Vec<(Tile<W>, Rectangle<f64, Logical>)>,
        activate: bool,
    ) {
        let n = members.len();
        if n == 0 {
            return;
        }
        let gaps = self.options.layout.gaps;
        let cells: Vec<_> = members.iter().map(|(_, c)| *c).collect();
        let from = union_all(&cells);
        let extra = group_extra(gaps);
        let max_w = (self.working_area.size.w - gaps * 2. - extra.w).max(1.);
        let max_h = (self.working_area.size.h - gaps * 2. - extra.h).max(1.);
        let content = Size::from((from.size.w.clamp(1., max_w), max_h));
        let cells = scale_cells(&cells, from, Rectangle::from_size(content), gaps);
        let width = ColumnWidth::Fixed(extra.w + content.w);

        let mut tiles = members.into_iter().map(|(t, _)| t);
        let first = tiles.next().unwrap();
        let mut column = Column::new_with_tile(
            first,
            self.view_size,
            self.working_area,
            self.parent_area,
            self.scale,
            width,
            false,
        );
        column.is_pending_fullscreen = false;
        column.is_pending_maximized = false;
        column.display_mode = ColumnDisplay::Normal;
        for mut tile in tiles {
            tile.update_config(self.view_size, self.scale, self.options.clone());
            column
                .data
                .push(TileData::new(&tile, WindowHeight::auto_1()));
            column.tiles.push(tile);
        }
        if n >= 2 {
            column.group = Some(TiledGroup::new(cells));
        }
        column.update_tile_sizes(false);
        for (tile, data) in zip(&column.tiles, &mut column.data) {
            data.update(tile);
        }
        column.natural_width = column.width();

        let idx = idx.min(self.columns.len());
        self.add_column(Some(idx), column, activate, None);
    }

    /// Remove tiled group column `col_idx`; returns the members with their current cells in
    /// strip coordinates.
    pub fn take_group_column(&mut self, col_idx: usize) -> Vec<(Tile<W>, Rectangle<f64, Logical>)> {
        let col_x = self.column_x(col_idx);
        let column = self.remove_column_by_idx(col_idx, None);
        let origin = Point::from((
            col_x + column.group_chrome_extra().w,
            column.tiles_origin().y,
        ));
        let cells: Vec<_> = match &column.group {
            Some(group) => group.cells.clone(),
            None => column
                .tiles
                .iter()
                .map(|t| Rectangle::from_size(t.tile_size()))
                .collect(),
        };
        zip(column.tiles, cells)
            .map(|(tile, mut cell)| {
                cell.loc += origin;
                (tile, cell)
            })
            .collect()
    }

    /// Group action: set the width of `window`'s group column; the cells scale along.
    /// `fill_height` also stretches the cells to the band height.
    pub fn set_group_width(&mut self, window: &W::Id, width: ColumnWidth, fill_height: bool) {
        let Some(col_idx) = self.column_idx_of(window) else {
            return;
        };
        let col = &mut self.columns[col_idx];
        if col.group.is_none() {
            return;
        }
        if fill_height {
            col.fill_group_height();
        }
        col.width = width;
        col.is_full_width = false;
        col.preset_width_idx = None;
        if let Some(group) = &mut col.group {
            group.restore_width = None;
        }
        col.update_tile_sizes(true);
        self.data[col_idx].update(col);
    }

    /// Group Fullscreen: the column takes the whole view width; a second call restores the
    /// previous width.
    pub fn toggle_group_fullscreen(&mut self, window: &W::Id) {
        let Some(col_idx) = self.column_idx_of(window) else {
            return;
        };
        let view_w = self.view_size.w;
        let col = &mut self.columns[col_idx];
        let Some(group) = &mut col.group else {
            return;
        };
        match group.restore_width.take() {
            Some((width, full)) => {
                col.width = width;
                col.is_full_width = full;
            }
            None => {
                group.restore_width = Some((col.width, col.is_full_width));
                col.width = ColumnWidth::Fixed(view_w);
                col.is_full_width = false;
                col.fill_group_height();
            }
        }
        col.preset_width_idx = None;
        col.update_tile_sizes(true);
        self.data[col_idx].update(col);
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // This matches self.tiles_with_render_positions().
        let scale = self.scale;
        let view_off = Point::from((-self.view_pos(), 0.));
        for (col, col_x) in self.columns_in_render_order() {
            let col_off = Point::from((col_x, 0.));
            let col_render_off = col.render_offset();

            // Hit the tab indicator.
            if col.display_mode == ColumnDisplay::Tabbed && col.sizing_mode().is_normal() {
                let col_pos = view_off + col_off + col_render_off;
                let col_pos = col_pos.to_physical_precise_round(scale).to_logical(scale);

                if let Some(idx) = col.tab_indicator.hit(
                    col.tab_indicator_area(),
                    col.tiles.len(),
                    scale,
                    pos - col_pos,
                ) {
                    let hit = HitType::Activate {
                        is_tab_indicator: true,
                    };
                    return Some((col.tiles[idx].window(), hit));
                }
            }

            for (tile, tile_off, visible) in col.tiles_in_render_order() {
                if !visible {
                    continue;
                }

                let tile_pos =
                    view_off + col_off + col_render_off + tile_off + tile.render_offset();
                // Round to physical pixels.
                let tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                if let Some(rv) = HitType::hit_tile(tile, tile_pos, pos) {
                    return Some(rv);
                }
            }
        }

        None
    }

    pub fn view_offset_gesture_begin(&mut self, is_touchpad: bool) {
        if self.columns.is_empty() {
            return;
        }

        if self.interactive_resize.is_some() {
            return;
        }

        if self.view_offset_locked() {
            return;
        }

        self.user_panned_view = true;

        let gesture = ViewGesture {
            current_view_offset: self.view_offset.current(),
            animation: None,
            tracker: SwipeTracker::new(),
            delta_from_tracker: self.view_offset.current(),
            stationary_view_offset: self.view_offset.stationary(),
            is_touchpad,
            dnd_last_event_time: None,
            dnd_nonzero_start_time: None,
            dnd_velocity: 0.,
            dnd_edge_gain: EDGE_GAIN_START,
            dnd_braking: false,
        };
        self.view_offset = ViewOffset::Gesture(gesture);
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        if self.view_offset_locked() {
            return;
        }

        if let ViewOffset::Gesture(ViewGesture {
            dnd_last_event_time: Some(_),
            ..
        }) = &self.view_offset
        {
            // Already active.
            return;
        }

        let gesture = ViewGesture {
            current_view_offset: self.view_offset.current(),
            animation: None,
            tracker: SwipeTracker::new(),
            delta_from_tracker: self.view_offset.current(),
            stationary_view_offset: self.view_offset.stationary(),
            is_touchpad: false,
            dnd_last_event_time: Some(self.clock.now_unadjusted()),
            dnd_nonzero_start_time: None,
            dnd_velocity: 0.,
            dnd_edge_gain: EDGE_GAIN_START,
            dnd_braking: false,
        };
        self.view_offset = ViewOffset::Gesture(gesture);

        self.interactive_resize = None;
    }

    /// End a DnD view-offset gesture without snapping or zeroing momentum.
    ///
    /// Used when an interactive move becomes floating: keep `wheel_accel` and
    /// turn leftover `dnd_velocity` into the same coast as a free pan.
    pub fn release_dnd_scroll_to_coast(&mut self) {
        let ViewOffset::Gesture(gesture) = &self.view_offset else {
            return;
        };
        if gesture.dnd_last_event_time.is_none() {
            return;
        }

        let current = gesture.current_view_offset;
        let velocity = gesture.dnd_velocity;
        self.user_panned_view = true;
        if velocity.abs() < 1. {
            self.view_offset = ViewOffset::Static(current);
            return;
        }
        self.coast_view_from(current, velocity);
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return None;
        };

        if gesture.is_touchpad != is_touchpad || gesture.dnd_last_event_time.is_some() {
            return None;
        }

        gesture.tracker.push(delta_x, timestamp);

        let norm_factor = if gesture.is_touchpad {
            self.working_area.size.w / VIEW_GESTURE_WORKING_AREA_MOVEMENT
        } else {
            1.
        };
        let pos = gesture.tracker.pos() * norm_factor;
        let view_offset = pos + gesture.delta_from_tracker;
        let bounds = self.free_pan_offset_bounds();
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            unreachable!();
        };
        let clamped = match bounds {
            Some((lo, hi)) => view_offset.clamp(lo, hi),
            None => view_offset,
        };
        gesture.delta_from_tracker += clamped - view_offset;
        gesture.current_view_offset = clamped;

        Some(true)
    }

    fn dnd_view_offset_bounds(&self) -> (f64, f64) {
        self.free_pan_offset_bounds().unwrap_or((0., 0.))
    }

    fn clamp_dnd_view_offset(&self, view_offset: f64) -> f64 {
        let (lo, hi) = self.dnd_view_offset_bounds();
        let min_offset = f64::min(lo, hi);
        let max_offset = f64::max(lo, hi);
        view_offset.clamp(min_offset, max_offset)
    }

    /// Integrate view offset by `step`. Returns true if the step hit a pan bound.
    fn apply_dnd_view_step(&mut self, step: f64) -> bool {
        if step == 0. {
            return false;
        }
        let cur = match &self.view_offset {
            ViewOffset::Gesture(g) => g.current_view_offset,
            _ => return false,
        };
        let unclamped = cur + step;
        let clamped = self.clamp_dnd_view_offset(unclamped);
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return false;
        };
        gesture.delta_from_tracker += clamped - cur;
        gesture.current_view_offset = clamped;
        (unclamped - clamped).abs() > 0.5 && step.signum() == (unclamped - clamped).signum()
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, delta: f64) -> bool {
        let ViewOffset::Gesture(gesture) = &self.view_offset else {
            return false;
        };
        let Some(last_time) = gesture.dnd_last_event_time else {
            return false;
        };

        let now = self.clock.now_unadjusted();
        let time_delta = now.saturating_sub(last_time).as_secs_f64();
        let max_speed = self.options.gestures.dnd_edge_view_scroll.max_speed;
        let delay = if self.drag_spread {
            Duration::ZERO
        } else {
            Duration::from_millis(u64::from(
                self.options.gestures.dnd_edge_view_scroll.delay_ms,
            ))
        };

        let mut step = 0.;
        let keep = {
            let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
                return false;
            };
            gesture.dnd_last_event_time = Some(now);

            if delta.abs() < 0.04 {
                if gesture.dnd_nonzero_start_time.take().is_some() {
                    gesture.dnd_braking = true;
                }
                gesture.dnd_edge_gain = EDGE_GAIN_START;
                if gesture.dnd_velocity.abs() < 8. {
                    gesture.dnd_velocity = 0.;
                    gesture.dnd_braking = false;
                    false
                } else {
                    let rate = if gesture.dnd_braking {
                        WHEEL_BRAKE_RATE
                    } else {
                        WHEEL_COAST_RATE
                    };
                    if time_delta > 0. {
                        gesture.dnd_velocity *= rate.powf(time_delta * 1000.);
                        step = gesture.dnd_velocity * time_delta;
                    }
                    true
                }
            } else {
                let nonzero_start = *gesture.dnd_nonzero_start_time.get_or_insert(now);
                if now.saturating_sub(nonzero_start) < delay {
                    if time_delta > 0. && gesture.dnd_velocity.abs() >= 8. {
                        step = gesture.dnd_velocity * time_delta;
                    }
                    true
                } else {
                    gesture.dnd_braking = false;
                    if time_delta > 0. {
                        gesture.dnd_edge_gain = (gesture.dnd_edge_gain
                            + EDGE_GAIN_PER_SEC * time_delta)
                            .min(EDGE_GAIN_MAX);
                        let target = (delta * max_speed * gesture.dnd_edge_gain)
                            .clamp(-WHEEL_MAX_VELOCITY, WHEEL_MAX_VELOCITY);
                        let vel = gesture.dnd_velocity;
                        let same_dir = target.signum() == vel.signum() || vel.abs() < 8.;
                        if same_dir && vel.abs() > target.abs() {
                            gesture.dnd_velocity *= WHEEL_COAST_RATE.powf(time_delta * 1000.);
                        } else {
                            let k =
                                (1. - (-EDGE_APPROACH_PER_SEC * time_delta).exp()).clamp(0., 1.);
                            gesture.dnd_velocity = vel + (target - vel) * k;
                        }
                        step = gesture.dnd_velocity * time_delta;
                    }
                    true
                }
            }
        };
        let hit_bound = if step != 0. {
            self.apply_dnd_view_step(step)
        } else {
            false
        };
        if hit_bound {
            // Tiled drag only: at the content edge, grow virtual strip padding
            // toward the 4-viewport cap so the view can keep moving while the
            // pointer stays in the edge zone. Floating never sets drag_spread.
            let grew = self.grow_drag_edge_toward_cap(step);
            if grew > 0.5 {
                let remain = step.signum() * (step.abs() - grew).max(0.);
                let follow = if remain.abs() > 0.01 { remain } else { step };
                let still_blocked = self.apply_dnd_view_step(follow);
                if still_blocked {
                    if let ViewOffset::Gesture(gesture) = &mut self.view_offset {
                        gesture.dnd_velocity = 0.;
                    }
                }
            } else if let ViewOffset::Gesture(gesture) = &mut self.view_offset {
                gesture.dnd_velocity = 0.;
            }
        }
        keep
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return false;
        };

        if is_touchpad.is_some_and(|x| gesture.is_touchpad != x) {
            return false;
        }

        // We do not handle cancelling, just like GNOME Shell doesn't. For this gesture, proper
        // cancelling would require keeping track of the original active column, and then updating
        // it in all the right places (adding columns, removing columns, etc.) -- quite a bit of
        // effort and bug potential.

        // Take into account any idle time between the last event and now.
        let now = self.clock.now_unadjusted();
        gesture.tracker.push(0., now);

        let norm_factor = if gesture.is_touchpad {
            self.working_area.size.w / VIEW_GESTURE_WORKING_AREA_MOVEMENT
        } else {
            1.
        };
        let velocity = gesture.tracker.velocity() * norm_factor;
        let pos = gesture.tracker.pos() * norm_factor;
        let current_view_offset = pos + gesture.delta_from_tracker;
        let is_touchpad = gesture.is_touchpad;

        if self.columns.is_empty() {
            self.view_offset = ViewOffset::Static(current_view_offset);
            return true;
        }

        // Touchpad view swipe: same free pan + coast as Super+wheel. No column snap.
        if is_touchpad {
            self.user_panned_view = true;
            if velocity.abs() < 1. {
                self.view_offset = ViewOffset::Static(current_view_offset);
            } else {
                self.coast_view_from(current_view_offset, velocity);
            }
            return true;
        }

        // Figure out where the gesture would stop after deceleration.
        let end_pos = gesture.tracker.projected_end_pos() * norm_factor;
        let target_view_offset = end_pos + gesture.delta_from_tracker;

        // Compute the snapping points. These are where the view aligns with column boundaries on
        // either side.
        struct Snap {
            // View position relative to x = 0 (the first column).
            view_pos: f64,
            // Column to activate for this snapping point.
            col_idx: usize,
        }

        let mut snapping_points = Vec::new();

        if self.is_centering_focused_column() {
            let mut col_x = 0.;
            for (col_idx, col) in self.columns.iter().enumerate() {
                let col_w = col.width();
                let mode = col.sizing_mode();

                let area = if mode.is_maximized() {
                    self.parent_area
                } else {
                    self.working_area
                };

                let left_strut = area.loc.x;

                let view_pos = if mode.is_fullscreen() {
                    col_x
                } else if area.size.w <= col_w {
                    col_x - left_strut
                } else {
                    col_x - (area.size.w - col_w) / 2. - left_strut
                };
                snapping_points.push(Snap { view_pos, col_idx });

                col_x += col_w + self.options.layout.gaps;
            }
        } else {
            let center_on_overflow = matches!(
                self.options.layout.center_focused_column,
                CenterFocusedColumn::OnOverflow
            );

            let view_width = self.view_size.w;
            let gaps = self.options.layout.gaps;

            let snap_points =
                |col_x, col: &Column<W>, prev_col_w: Option<f64>, next_col_w: Option<f64>| {
                    let col_w = col.width();
                    let mode = col.sizing_mode();

                    let area = if mode.is_maximized() {
                        self.parent_area
                    } else {
                        self.working_area
                    };

                    let left_strut = area.loc.x;
                    let right_strut = self.view_size.w - area.size.w - area.loc.x;

                    // Normal columns align with the working area, but fullscreen columns align with
                    // the view size.
                    if mode.is_fullscreen() {
                        let left = col_x;
                        let right = left + col_w;
                        (left, right)
                    } else {
                        // Logic from compute_new_view_offset.
                        let padding = if mode.is_maximized() {
                            0.
                        } else {
                            ((area.size.w - col_w) / 2.).clamp(0., gaps)
                        };

                        let center = if area.size.w <= col_w {
                            col_x - left_strut
                        } else {
                            col_x - (area.size.w - col_w) / 2. - left_strut
                        };
                        let is_overflowing = |adj_col_w: Option<f64>| {
                            center_on_overflow
                                && adj_col_w
                                    .filter(|adj_col_w| {
                                        // NOTE: This logic won't work entirely correctly with small
                                        // fixed-size maximized windows (they have a different area
                                        // and padding).
                                        center_on_overflow
                                            && adj_col_w + 3.0 * gaps + col_w > area.size.w
                                    })
                                    .is_some()
                        };

                        let left = if is_overflowing(next_col_w) {
                            center
                        } else {
                            col_x - padding - left_strut
                        };
                        let right = if is_overflowing(prev_col_w) {
                            center + view_width
                        } else {
                            col_x + col_w + padding + right_strut
                        };
                        (left, right)
                    }
                };

            // Prevent the gesture from snapping further than the first/last column, as this is
            // generally undesired.
            //
            // It's ok if leftmost_snap is > rightmost_snap (this happens if the columns on a
            // workspace total up to less than the workspace width).

            // The first column's left snap isn't actually guaranteed to be the *leftmost* snap.
            // With weird enough left strut and perhaps a maximized small fixed-size window, you
            // can make the second window's left snap be further to the left than the first
            // window's. The same goes for the rightmost snap.
            //
            // This isn't actually a big problem because it's very much an obscure edge case. Just
            // need to make sure the code doesn't panic when that happens.
            let leftmost_snap = snap_points(
                0.,
                &self.columns[0],
                None,
                self.columns.get(1).map(|c| c.width()),
            )
            .0;
            let last_col_idx = self.columns.len() - 1;
            let last_col_x = self
                .columns
                .iter()
                .take(last_col_idx)
                .fold(0., |col_x, col| col_x + col.width() + gaps);
            let rightmost_snap = snap_points(
                last_col_x,
                &self.columns[last_col_idx],
                last_col_idx
                    .checked_sub(1)
                    .and_then(|idx| self.columns.get(idx).map(|c| c.width())),
                None,
            )
            .1 - view_width;

            snapping_points.push(Snap {
                view_pos: leftmost_snap,
                col_idx: 0,
            });
            snapping_points.push(Snap {
                view_pos: rightmost_snap,
                col_idx: last_col_idx,
            });

            let mut push = |col_idx, left, right| {
                if leftmost_snap < left && left < rightmost_snap {
                    snapping_points.push(Snap {
                        view_pos: left,
                        col_idx,
                    });
                }

                let right = right - view_width;
                if leftmost_snap < right && right < rightmost_snap {
                    snapping_points.push(Snap {
                        view_pos: right,
                        col_idx,
                    });
                }
            };

            let mut col_x = 0.;
            for (col_idx, col) in self.columns.iter().enumerate() {
                let (left, right) = snap_points(
                    col_x,
                    col,
                    col_idx
                        .checked_sub(1)
                        .and_then(|idx| self.columns.get(idx).map(|c| c.width())),
                    self.columns.get(col_idx + 1).map(|c| c.width()),
                );
                push(col_idx, left, right);

                col_x += col.width() + gaps;
            }
        }

        // Find the closest snapping point.
        snapping_points.sort_by_key(|snap| NotNan::new(snap.view_pos).unwrap());

        let active_col_x = self.column_x(self.active_column_idx);
        let target_view_pos = active_col_x + target_view_offset;
        let target_snap = snapping_points
            .iter()
            .min_by_key(|snap| NotNan::new((snap.view_pos - target_view_pos).abs()).unwrap())
            .unwrap();

        let mut new_col_idx = target_snap.col_idx;

        if !self.is_centering_focused_column() {
            // Focus the furthest window towards the direction of the gesture.
            if target_view_offset >= current_view_offset {
                for col_idx in (new_col_idx + 1)..self.columns.len() {
                    let col = &self.columns[col_idx];
                    let col_x = self.column_x(col_idx);
                    let col_w = col.width();
                    let mode = col.sizing_mode();

                    let area = if mode.is_maximized() {
                        self.parent_area
                    } else {
                        self.working_area
                    };

                    let left_strut = area.loc.x;

                    if mode.is_fullscreen() {
                        if target_snap.view_pos + self.view_size.w < col_x + col_w {
                            break;
                        }
                    } else {
                        let padding = if mode.is_maximized() {
                            0.
                        } else {
                            ((area.size.w - col_w) / 2.).clamp(0., self.options.layout.gaps)
                        };

                        if target_snap.view_pos + left_strut + area.size.w < col_x + col_w + padding
                        {
                            break;
                        }
                    }

                    new_col_idx = col_idx;
                }
            } else {
                for col_idx in (0..new_col_idx).rev() {
                    let col = &self.columns[col_idx];
                    let col_x = self.column_x(col_idx);
                    let col_w = col.width();
                    let mode = col.sizing_mode();

                    let area = if mode.is_maximized() {
                        self.parent_area
                    } else {
                        self.working_area
                    };

                    let left_strut = area.loc.x;

                    if mode.is_fullscreen() {
                        if col_x < target_snap.view_pos {
                            break;
                        }
                    } else {
                        let padding = if mode.is_maximized() {
                            0.
                        } else {
                            ((area.size.w - col_w) / 2.).clamp(0., self.options.layout.gaps)
                        };

                        if col_x - padding < target_snap.view_pos + left_strut {
                            break;
                        }
                    }

                    new_col_idx = col_idx;
                }
            }
        }

        let new_col_x = self.column_x(new_col_idx);
        let delta = active_col_x - new_col_x;

        if self.active_column_idx != new_col_idx {
            self.view_offset_to_restore = None;
        }

        self.active_column_idx = new_col_idx;

        let target_view_offset = target_snap.view_pos - new_col_x;

        self.view_offset = ViewOffset::Animation(Animation::new(
            self.clock.clone(),
            current_view_offset + delta,
            target_view_offset,
            velocity,
            self.options.animations.horizontal_view_movement.0,
        ));

        // Fitting the column here pulls the camera back onto the focused window
        // and cancels a free pan. Leave the gesture's own snap animation.
        if !self.user_panned_view {
            self.animate_view_offset_to_column(None, new_col_idx, None);
        }

        true
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        let ViewOffset::Gesture(gesture) = &mut self.view_offset else {
            return;
        };

        if gesture.dnd_last_event_time.is_some() && gesture.tracker.pos() == 0. {
            // DnD didn't scroll anything, so preserve the current view position (rather than
            // snapping the window).

            // If there's an ongoing animation within the gesture (e.g. from a window being removed
            // during DnD), preserve it.
            if let Some(mut anim) = gesture.animation.take() {
                anim.offset(gesture.current_view_offset);
                self.view_offset = ViewOffset::Animation(anim);
            } else {
                self.view_offset = ViewOffset::Static(gesture.delta_from_tracker);
            }

            if !self.columns.is_empty() && !self.user_panned_view {
                // Just in case, make sure the active window remains on screen.
                self.animate_view_offset_to_column(None, self.active_column_idx, None);
            }
            return;
        }

        self.view_offset_gesture_end(None);
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let col = self
            .columns
            .iter_mut()
            .find(|col| col.contains(&window))
            .unwrap();

        if !col.pending_sizing_mode().is_normal() {
            return false;
        }

        self.group_resize_start = col
            .group
            .as_ref()
            .map(|group| (window.clone(), group.cells.clone()));

        let tile = col
            .tiles
            .iter_mut()
            .find(|tile| tile.window().id() == &window)
            .unwrap();

        let original_window_size = tile.window_size();

        let resize = InteractiveResize {
            window,
            original_window_size,
            data: InteractiveResizeData { edges },
        };
        self.interactive_resize = Some(resize);

        self.view_offset.stop_anim_and_gesture();

        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else {
            return false;
        };

        if window != &resize.window {
            return false;
        }

        let is_centering = self.is_centering_focused_column();
        let edges = resize.data.edges;
        let orig_w = resize.original_window_size.w;
        let orig_h = resize.original_window_size.h;

        let col_idx = self
            .columns
            .iter()
            .position(|col| col.contains(window))
            .unwrap();

        let tile_idx = self.columns[col_idx]
            .tiles
            .iter()
            .position(|tile| tile.window().id() == window)
            .unwrap();

        if self.columns[col_idx].group.is_some() {
            let Some((id, start)) = &self.group_resize_start else {
                return false;
            };
            if id != window
                || start.len() != self.columns[col_idx].tiles.len()
                || self.columns[col_idx]
                    .group
                    .as_ref()
                    .is_some_and(|g| g.reserved.is_some())
            {
                return false;
            }
            let gaps = self.options.layout.gaps;
            let col = &mut self.columns[col_idx];
            let limits = col.group_cell_limits();
            let member_limits: Vec<_> = col
                .tiles
                .iter()
                .map(|t| cell_limits_of(t.min_size_nonfullscreen(), t.max_size_nonfullscreen()))
                .collect();
            let cells = normalize_cells(resize_member(
                start,
                &member_limits,
                tile_idx,
                edges,
                delta,
                gaps,
                limits,
            ));
            let w = col.group_extra_w() + union_all(&cells).size.w;
            if let Some(group) = &mut col.group {
                group.cells = cells;
                group.restore_width = None;
            }
            col.width = ColumnWidth::Fixed(w);
            col.is_full_width = false;
            col.preset_width_idx = None;
            col.update_tile_sizes(false);
            col.natural_width = col.width();
            self.data[col_idx].update(col);
            return true;
        }

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let mut dx = delta.x;
            if edges.contains(ResizeEdge::LEFT) {
                dx = -dx;
            };

            if is_centering {
                dx *= 2.;
            }

            let window_width = (orig_w + dx).round() as i32;
            let col = &mut self.columns[col_idx];
            col.set_column_width(SizeChange::SetFixed(window_width), Some(tile_idx), false);
            col.natural_width = col.width();
            self.data[col_idx].update(col);
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            let mut dy = delta.y;
            if edges.contains(ResizeEdge::TOP) {
                dy = -dy;
            };

            // FIXME: some smarter height distribution would be nice here so that vertical
            // resizes work as expected in more cases.

            let window_height = (orig_h + dy).round() as i32;
            self.columns[col_idx].set_window_height(
                SizeChange::SetFixed(window_height),
                Some(tile_idx),
                false,
            );
        }

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            self.enforce_cap(col_idx, edges.contains(ResizeEdge::RIGHT));
        }

        true
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        let Some(resize) = &self.interactive_resize else {
            return;
        };

        if let Some(window) = window {
            if window != &resize.window {
                return;
            }

            // Animate the active window into view right away.
            if self.columns[self.active_column_idx].contains(window) {
                self.animate_view_offset_to_column(None, self.active_column_idx, None);
            }
        }

        self.interactive_resize = None;
        self.group_resize_start = None;
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        for (col_idx, col) in self.columns.iter_mut().enumerate() {
            let mut col_resize_data = None;
            if let Some(resize) = &self.interactive_resize {
                if col.contains(&resize.window) {
                    col_resize_data = Some(resize.data);
                }
            }

            let is_tabbed = col.display_mode == ColumnDisplay::Tabbed;
            let extra_size = col.extra_size();

            // If transactions are disabled, also disable combined throttling, for more intuitive
            // behavior. In tabbed display mode, only one window is visible, so individual
            // throttling makes more sense.
            let individual_throttling = self.options.disable_transactions || is_tabbed;

            let intent = if self.options.disable_resize_throttling {
                ConfigureIntent::CanSend
            } else if individual_throttling {
                // In this case, we don't use combined throttling, but rather compute throttling
                // individually below.
                ConfigureIntent::CanSend
            } else {
                col.tiles
                    .iter()
                    .fold(ConfigureIntent::NotNeeded, |intent, tile| {
                        match (intent, tile.window().configure_intent()) {
                            (_, ConfigureIntent::ShouldSend) => ConfigureIntent::ShouldSend,
                            (ConfigureIntent::NotNeeded, tile_intent) => tile_intent,
                            (ConfigureIntent::CanSend, ConfigureIntent::Throttled) => {
                                ConfigureIntent::Throttled
                            }
                            (intent, _) => intent,
                        }
                    })
            };

            for (tile_idx, tile) in col.tiles.iter_mut().enumerate() {
                let win = tile.window_mut();

                let active_in_column = col.active_tile_idx == tile_idx;
                win.set_active_in_column(active_in_column);
                win.set_floating(false);
                win.set_minimized(false);

                let mut active = is_active && self.active_column_idx == col_idx;
                if self.options.deactivate_unfocused_windows {
                    active &= active_in_column && is_focused;
                } else {
                    // In tabbed mode, all tabs have activated state to reduce unnecessary
                    // animations when switching tabs.
                    active &= active_in_column || is_tabbed;
                }
                win.set_activated(active);

                win.set_interactive_resize(col_resize_data);

                let border_config = self.options.layout.border.merged_with(&win.rules().border);
                let bounds = compute_toplevel_bounds(
                    border_config,
                    self.working_area.size,
                    extra_size,
                    self.options.layout.gaps,
                );
                win.set_bounds(bounds);

                let intent = if individual_throttling {
                    win.configure_intent()
                } else {
                    intent
                };

                if matches!(
                    intent,
                    ConfigureIntent::CanSend | ConfigureIntent::ShouldSend
                ) {
                    win.send_pending_configure();
                }

                win.refresh();
            }
        }
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    #[cfg(test)]
    pub fn parent_area(&self) -> Rectangle<f64, Logical> {
        self.parent_area
    }

    #[cfg(test)]
    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    #[cfg(test)]
    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn active_column_idx(&self) -> usize {
        self.active_column_idx
    }

    #[cfg(test)]
    pub(super) fn view_offset(&self) -> &ViewOffset {
        &self.view_offset
    }

    #[cfg(test)]
    pub fn is_drag_spread(&self) -> bool {
        self.drag_spread
    }

    #[cfg(test)]
    pub fn drag_edge_extra(&self) -> (f64, f64) {
        (self.drag_edge_left, self.drag_edge_right)
    }

    #[cfg(test)]
    pub fn live_reflow_layout_gap(&self) -> Option<LiveReflowGap> {
        self.live_reflow_gap
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        assert_eq!(self.columns.len(), self.data.len());
        if let Some(gap) = self.live_reflow_gap {
            match gap.position {
                InsertPosition::NewColumn(idx) => {
                    assert!(idx <= self.columns.len());
                    assert!(gap.width > 0.);
                }
                InsertPosition::InColumn(col_idx, tile_idx) => {
                    assert!(col_idx < self.columns.len());
                    assert!(tile_idx <= self.columns[col_idx].tiles.len());
                    assert!(gap.height > 0.);
                    assert_eq!(
                        self.columns[col_idx].live_reflow_tile_gap,
                        Some((tile_idx, gap.height))
                    );
                    let heights = self.columns[col_idx]
                        .live_reflow_tile_heights
                        .as_ref()
                        .expect("in-column live-reflow must store split heights");
                    assert_eq!(heights.len(), self.columns[col_idx].tiles.len());
                }
                InsertPosition::Floating => panic!("live-reflow gap cannot be floating"),
            }
        } else {
            assert!(self
                .columns
                .iter()
                .all(|col| col.live_reflow_tile_gap.is_none()
                    && col.live_reflow_tile_heights.is_none()));
        }
        assert_eq!(
            self.working_area,
            compute_working_area(self.parent_area, self.scale, self.options.layout.struts)
        );

        if !self.columns.is_empty() {
            assert!(self.active_column_idx < self.columns.len());

            for (column, data) in zip(&self.columns, &self.data) {
                assert!(Rc::ptr_eq(&self.options, &column.options));
                assert_eq!(self.clock, column.clock);
                assert_eq!(self.scale, column.scale);
                column.verify_invariants();

                let mut data2 = *data;
                data2.update(column);
                assert_eq!(data, &data2, "column data must be up to date");
            }

            let col = &self.columns[self.active_column_idx];

            if self.view_offset_to_restore.is_some() {
                assert!(
                    !col.sizing_mode().is_normal(),
                    "when view_offset_to_restore is set, \
                     the active column must be fullscreen or maximized"
                );
            }
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.columns
                    .iter()
                    .flat_map(|col| &col.tiles)
                    .any(|tile| tile.window().id() == &resize.window),
                "interactive resize window must be present in the layout"
            );
        }
    }
}

impl ViewOffset {
    /// Returns the current view offset.
    pub fn current(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.value(),
            ViewOffset::Gesture(gesture) => {
                gesture.current_view_offset
                    + gesture.animation.as_ref().map_or(0., |anim| anim.value())
            }
        }
    }

    /// Returns the target view offset suitable for computing the new view offset.
    pub fn target(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            ViewOffset::Animation(anim) => anim.to(),
            // This can be used for example if a gesture is interrupted.
            ViewOffset::Gesture(gesture) => gesture.current_view_offset,
        }
    }

    /// Returns a view offset value suitable for saving and later restoration.
    ///
    /// This means that it shouldn't return an in-progress animation or gesture value.
    fn stationary(&self) -> f64 {
        match self {
            ViewOffset::Static(offset) => *offset,
            // For animations we can return the final value.
            ViewOffset::Animation(anim) => anim.to(),
            ViewOffset::Gesture(gesture) => gesture.stationary_view_offset,
        }
    }

    pub fn is_static(&self) -> bool {
        matches!(self, Self::Static(_))
    }

    pub fn is_gesture(&self) -> bool {
        matches!(self, Self::Gesture(_))
    }

    pub fn is_animation_ongoing(&self) -> bool {
        match self {
            ViewOffset::Static(_) => false,
            ViewOffset::Animation(_) => true,
            ViewOffset::Gesture(gesture) => {
                gesture.animation.is_some()
                    || (gesture.dnd_last_event_time.is_some() && gesture.dnd_velocity.abs() > 1.)
            }
        }
    }

    pub fn offset(&mut self, delta: f64) {
        match self {
            ViewOffset::Static(offset) => *offset += delta,
            ViewOffset::Animation(anim) => anim.offset(delta),
            ViewOffset::Gesture(gesture) => {
                gesture.stationary_view_offset += delta;
                gesture.delta_from_tracker += delta;
                gesture.current_view_offset += delta;
            }
        }
    }

    pub fn cancel_gesture(&mut self) {
        if let ViewOffset::Gesture(gesture) = self {
            *self = ViewOffset::Static(gesture.current_view_offset);
        }
    }

    pub fn stop_anim_and_gesture(&mut self) {
        *self = ViewOffset::Static(self.current());
    }
}

impl ViewGesture {
    fn animate_from(&mut self, from: f64, clock: Clock, config: niri_config::Animation) {
        let current = self.animation.as_ref().map_or(0., Animation::value);
        self.animation = Some(Animation::new(clock, from + current, 0., 0., config));
    }
}

impl ColumnData {
    pub fn new<W: LayoutElement>(column: &Column<W>) -> Self {
        let mut rv = Self { width: 0. };
        rv.update(column);
        rv
    }

    pub fn update<W: LayoutElement>(&mut self, column: &Column<W>) {
        self.width = column.width();
    }
}

impl TileData {
    pub fn new<W: LayoutElement>(tile: &Tile<W>, height: WindowHeight) -> Self {
        let mut rv = Self {
            height,
            size: Size::default(),
            interactively_resizing_by_left_edge: false,
        };
        rv.update(tile);
        rv
    }

    pub fn update<W: LayoutElement>(&mut self, tile: &Tile<W>) {
        self.size = tile.tile_size();
        self.interactively_resizing_by_left_edge = tile
            .window()
            .interactive_resize_data()
            .is_some_and(|data| data.edges.contains(ResizeEdge::LEFT));
    }
}

impl From<PresetSize> for ColumnWidth {
    fn from(value: PresetSize) -> Self {
        match value {
            PresetSize::Proportion(p) => Self::Proportion(p.clamp(0., 10000.)),
            PresetSize::Fixed(f) => Self::Fixed(f64::from(f.clamp(1, 100000))),
        }
    }
}

impl WindowHeight {
    const fn auto_1() -> Self {
        Self::Auto { weight: 1. }
    }
}

impl<W: LayoutElement> Column<W> {
    #[allow(clippy::too_many_arguments)]
    fn new_with_tile(
        tile: Tile<W>,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        width: ColumnWidth,
        is_full_width: bool,
    ) -> Self {
        let options = tile.options.clone();

        let display_mode = tile
            .window()
            .rules()
            .default_column_display
            .unwrap_or(options.layout.default_column_display);

        // Try to match width to a preset width. Consider the following case: a terminal (foot)
        // sizes itself to the terminal grid. We open it with default-column-width 0.5. It shrinks
        // by a few pixels to evenly match the terminal grid. Then we press
        // switch-preset-column-width intending to go to proportion 0.667, but the preset width
        // matching code picks the proportion 0.5 preset because it's the next smallest width after
        // the current foot's window width. Effectively, this makes the first
        // switch-preset-column-width press ignored.
        //
        // However, here, we do know that width = proportion 0.5 (regardless of what the window
        // opened with), and we can match it to a preset right away, if one exists.
        let preset_width_idx = options
            .layout
            .preset_column_widths
            .iter()
            .position(|preset| width == ColumnWidth::from(*preset));

        let mut rv = Self {
            tiles: vec![],
            data: vec![],
            active_tile_idx: 0,
            width,
            preset_width_idx,
            is_full_width,
            is_pending_maximized: false,
            is_pending_fullscreen: false,
            display_mode,
            tab_indicator: TabIndicator::new(options.layout.tab_indicator),
            move_animation: None,
            view_size,
            working_area,
            parent_area,
            scale,
            clock: tile.clock.clone(),
            options,
            live_reflow_tile_gap: None,
            live_reflow_tile_heights: None,
            live_reflow_width: None,
            live_reflow_orig_width: None,
            natural_width: 0.,
            split_id: None,
            split_pair_width: 0.,
            split_seq: 0,
            group: None,
        };

        let pending_sizing_mode = tile.window().pending_sizing_mode();

        rv.add_tile_at(0, tile);
        rv.natural_width = rv.width();

        match pending_sizing_mode {
            SizingMode::Normal => (),
            SizingMode::Maximized => rv.set_maximized(true),
            SizingMode::Fullscreen => rv.set_fullscreen(true),
        }

        // Animate the tab indicator for new columns.
        if display_mode == ColumnDisplay::Tabbed
            && !rv.options.layout.tab_indicator.hide_when_single_tab
            && rv.sizing_mode().is_normal()
        {
            // Usually new columns are created together with window movement actions. For new
            // windows, we handle that in start_open_animation().
            rv.tab_indicator
                .start_open_animation(rv.clock.clone(), rv.options.animations.window_movement.0);
        }

        rv
    }

    fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        parent_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let mut update_sizes = false;

        if self.view_size != view_size
            || self.working_area != working_area
            || self.parent_area != parent_area
        {
            update_sizes = true;
        }

        // If preset widths changed, clear our stored preset index.
        if self.options.layout.preset_column_widths != options.layout.preset_column_widths {
            self.preset_width_idx = None;
        }

        // If preset heights changed, make our heights non-preset.
        if self.options.layout.preset_window_heights != options.layout.preset_window_heights {
            self.convert_heights_to_auto();
            update_sizes = true;
        }

        if self.options.layout.gaps != options.layout.gaps {
            update_sizes = true;
        }

        if self.options.layout.border.off != options.layout.border.off
            || self.options.layout.border.width != options.layout.border.width
        {
            update_sizes = true;
        }

        if self.options.layout.tab_indicator != options.layout.tab_indicator {
            update_sizes = true;
        }

        for (tile, data) in zip(&mut self.tiles, &mut self.data) {
            tile.update_config(view_size, scale, options.clone());
            data.update(tile);
        }

        self.tab_indicator
            .update_config(options.layout.tab_indicator);
        self.view_size = view_size;
        self.working_area = working_area;
        self.parent_area = parent_area;
        self.scale = scale;
        self.options = options;

        if update_sizes {
            self.update_tile_sizes(false);
            if self.split_id.is_none()
                && (self.is_full_width || matches!(self.width, ColumnWidth::Proportion(_)))
            {
                self.natural_width = self.width();
            }
        }
    }

    pub fn update_shaders(&mut self) {
        for tile in &mut self.tiles {
            tile.update_shaders();
        }

        self.tab_indicator.update_shaders();
        if let Some(group) = &mut self.group {
            group.chrome.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        if let Some(move_) = &mut self.move_animation {
            if move_.anim.is_done() {
                self.move_animation = None;
            }
        }

        for tile in &mut self.tiles {
            tile.advance_animations();
        }

        self.tab_indicator.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.move_animation.is_some()
            || self.tab_indicator.are_animations_ongoing()
            || self.tiles.iter().any(Tile::are_animations_ongoing)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.move_animation.is_some()
            || self.tab_indicator.are_animations_ongoing()
            || self.tiles.iter().any(Tile::are_transitions_ongoing)
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        if let Some(bar) = self.group_side_bar() {
            let radius = self
                .tiles
                .first()
                .map(|tile| tile.window().geometry_corner_radius().top_left)
                .unwrap_or_default();
            let gaps = self.options.layout.gaps;
            let scale = self.scale;
            let options = self.options.clone();
            if let Some(group) = &mut self.group {
                let bottom_w = bottom_bar_w(group.bbox(), gaps);
                group.chrome.update(bar.size.h, bottom_w, radius, scale);
                group
                    .chrome
                    .update_glass(&options.layout.group_border, options.blur);
            }
        }

        let active_idx = self.active_tile_idx;
        let in_group = self.group.is_some();
        for (tile_idx, (tile, tile_off)) in self.tiles_mut().enumerate() {
            let is_active = is_active && tile_idx == active_idx;

            let mut tile_view_rect = view_rect;
            tile_view_rect.loc -= tile_off + tile.render_offset();
            tile.set_chrome_in_group(in_group);
            tile.update_render_elements(is_active, tile_view_rect);
        }

        let config = self.tab_indicator.config();
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        let tabs = zip(&self.tiles, offsets)
            .enumerate()
            .map(|(tile_idx, (tile, tile_off))| {
                let is_active = tile_idx == active_idx;
                let is_urgent = tile.window().is_urgent();
                let tile_pos = tile_off + tile.render_offset();
                TabInfo::from_tile(tile, tile_pos, is_active, is_urgent, &config)
            });

        // Hide the tab indicator in fullscreen. If you have it configured to overlap the window,
        // you don't want that to happen in fullscreen. Also, laying things out correctly when the
        // tab indicator is within the column and the column goes fullscreen, would require too
        // many changes to the code for too little benefit (it's mostly invisible anyway).
        let enabled = self.display_mode == ColumnDisplay::Tabbed && self.sizing_mode().is_normal();

        self.tab_indicator.update_render_elements(
            enabled,
            self.tab_indicator_area(),
            view_rect,
            self.tiles.len(),
            tabs,
            is_active,
            self.scale,
        );
    }

    pub fn is_pending_fullscreen(&self) -> bool {
        self.is_pending_fullscreen
    }

    pub fn is_pending_maximized(&self) -> bool {
        self.is_pending_maximized
    }

    pub fn pending_sizing_mode(&self) -> SizingMode {
        if self.is_pending_fullscreen {
            SizingMode::Fullscreen
        } else if self.is_pending_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn render_offset(&self) -> Point<f64, Logical> {
        let mut offset = Point::from((0., 0.));

        if let Some(move_) = &self.move_animation {
            offset.x += move_.from * move_.anim.value();
        }

        offset
    }

    pub fn animate_move_from(&mut self, from_x_offset: f64) {
        self.animate_move_from_with_config(
            from_x_offset,
            self.options.animations.window_movement.0,
        );
    }

    pub fn animate_move_from_with_config(
        &mut self,
        from_x_offset: f64,
        config: niri_config::Animation,
    ) {
        let current_offset = self
            .move_animation
            .as_ref()
            .map_or(0., |move_| move_.from * move_.anim.value());

        let anim = Animation::new(self.clock.clone(), 1., 0., 0., config);
        self.move_animation = Some(MoveAnimation {
            anim,
            from: from_x_offset + current_offset,
        });
    }

    pub fn offset_move_anim_current(&mut self, offset: f64) {
        if let Some(move_) = self.move_animation.as_mut() {
            // If the anim is almost done, there's little point trying to offset it; we can let
            // things jump. If it turns out like a bad idea, we could restart the anim instead.
            let value = move_.anim.value();
            if value > 0.001 {
                move_.from += offset / value;
            }
        }
    }

    /// Returns whether this column is currently fullscreen.
    ///
    /// As in, if it contains one currently-fullscreen tile, or in tabbed mode, if it contains at
    /// least one currently-fullscreen tile.
    ///
    /// This will lag behind is_pending_fullscreen, depending on when the tiles actually respond to
    /// the un/fullscreen request. But, it's possible for is_fullscreen() to flip instantly, for
    /// example when consuming a fullscreen tile into a non-pending-fullscreen column.
    ///
    /// This controls things like:
    ///
    /// - whether the column draws at the top of the screen or at the start of the working area
    /// - whether the column draws above the top layer-shell layer
    /// - whether the tab indicator is shown
    /// - restoring view_offset_before_fullscreen
    ///
    /// Edge cases to watch out for:
    ///
    /// - Consuming a fullscreen tile into a non-tabbed column will keep that tile fullscreen until
    ///   it responds to the unfullscreen request. This tile may be anywhere in the column,
    ///   including at the active position.
    ///
    /// - Changing a fullscreen tabbed column into normal mode is an easy way to get randomly
    ///   delayed unfullscreening tiles in a normal column.
    ///
    /// - is_fullscreen() can suddenly change when consuming/expelling a fullscreen tile into/from a
    ///   non-fullscreen column. This can influence the code that saves/restores the unfullscreen
    ///   view offset.
    fn sizing_mode(&self) -> SizingMode {
        // Behaviors that we want:
        //
        // 1. The common case: single tile in a column. Assume no animations. Fullscreening the tile
        //    should make it jump to the top-left of the screen only when the tile finishes
        //    fullscreening. Similarly, unfullscreening should keep it at the top-left until the
        //    tile had unfullscreened.
        //
        // 2. Unfullscreening a tabbed column with multiple tiles should restore the view offset
        //    correctly. This means waiting for *all* tiles to unfullscreen, because otherwise the
        //    restored view offset will immediately get overwritten by the still screen-wide column
        //    (it uses the largest tile's width).
        //
        // 3. Changing a fullscreen tabbed column to normal should probably also restore the view
        //    offset correctly. Same problem as above, but now for normal columns (since display
        //    mode change applies instantly).
        //
        // The logic that satisfies these behaviors is to check if *any* tile is fullscreen.
        let mut any_fullscreen = false;
        let mut any_maximized = false;
        for tile in &self.tiles {
            match tile.sizing_mode() {
                SizingMode::Normal => (),
                SizingMode::Maximized => any_maximized = true,
                SizingMode::Fullscreen => any_fullscreen = true,
            }
        }

        if any_fullscreen {
            SizingMode::Fullscreen
        } else if any_maximized {
            SizingMode::Maximized
        } else {
            SizingMode::Normal
        }
    }

    pub fn contains(&self, window: &W::Id) -> bool {
        self.tiles
            .iter()
            .map(Tile::window)
            .any(|win| win.id() == window)
    }

    pub fn position(&self, window: &W::Id) -> Option<usize> {
        self.tiles
            .iter()
            .map(Tile::window)
            .position(|win| win.id() == window)
    }

    fn activate_idx(&mut self, idx: usize) -> bool {
        if self.active_tile_idx == idx {
            return false;
        }

        self.active_tile_idx = idx;

        self.tiles[idx].ensure_alpha_animates_to_1();

        true
    }

    fn activate_window(&mut self, window: &W::Id) {
        let idx = self.position(window).unwrap();
        self.activate_idx(idx);
    }

    fn add_tile_at(&mut self, idx: usize, mut tile: Tile<W>) {
        tile.update_config(self.view_size, self.scale, self.options.clone());

        // Inserting a tile pushes down all tiles below it, but also in always-centering mode it
        // will affect the X position of all tiles in the column.
        let mut prev_offsets = Vec::with_capacity(self.tiles.len() + 1);
        prev_offsets.extend(self.tile_offsets().take(self.tiles.len()));

        if self.display_mode != ColumnDisplay::Tabbed {
            self.is_pending_fullscreen = false;
            self.is_pending_maximized = false;
        }

        self.data
            .insert(idx, TileData::new(&tile, WindowHeight::auto_1()));
        self.tiles.insert(idx, tile);
        self.update_tile_sizes(true);

        // Animate tiles according to the offset changes.
        prev_offsets.insert(idx, Point::default());
        for (i, ((tile, offset), prev)) in zip(self.tiles_mut(), prev_offsets).enumerate() {
            if i == idx {
                continue;
            }

            tile.animate_move_from(prev - offset);
        }
    }

    fn update_window(&mut self, window: &W::Id) {
        let (tile_idx, tile) = self
            .tiles
            .iter_mut()
            .enumerate()
            .find(|(_, tile)| tile.window().id() == window)
            .unwrap();

        let prev_height = self.data[tile_idx].size.h;

        tile.update_window();
        self.data[tile_idx].update(tile);

        let offset = prev_height - self.data[tile_idx].size.h;

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed || self.group.is_some();

        // Move windows below in tandem with resizing.
        //
        // FIXME: in always-centering mode, window resizing will affect the offsets of all other
        // windows in the column, so they should all be animated. How should this interact with
        // animated vs. non-animated resizes? For example, an animated +20 resize followed by two
        // non-animated -10 resizes.
        if !is_tabbed && offset != 0. {
            if tile.resize_animation().is_some() {
                // If there's a resize animation (that may have just started in
                // tile.update_window()), then the apparent size change is smooth with no sudden
                // jumps. This corresponds to adding an Y animation to tiles below.
                for tile in &mut self.tiles[tile_idx + 1..] {
                    tile.animate_move_y_from_with_config(
                        offset,
                        self.options.animations.window_resize.anim,
                    );
                }
            } else {
                // There's no resize animation, but the offset is nonzero. This could happen for
                // example:
                // - if the window resized on its own, which we don't animate
                // - if the window resized by less than 10 px (the resize threshold)
                //
                // The latter case could also cancel an ongoing resize animation.
                //
                // Now, stationary tiles below shouldn't react to this offset change in any way,
                // i.e. their apparent Y position should jump together with the resize. However,
                // tiles below that are already animating an Y movement should offset their
                // animations to avoid the jump.
                //
                // Notably, this is necessary to fix the animation jump when resizing height back
                // and forth in quick succession (in a way that cancels the resize animation).
                for tile in &mut self.tiles[tile_idx + 1..] {
                    tile.offset_move_y_anim_current(offset);
                }
            }
        }
    }

    /// Extra size taken up by elements in the column such as the tab indicator.
    fn extra_size(&self) -> Size<f64, Logical> {
        if self.display_mode == ColumnDisplay::Tabbed {
            self.tab_indicator.extra_size(self.tiles.len(), self.scale)
        } else {
            Size::from((0., 0.))
        }
    }

    fn resolve_preset_width(&self, preset: PresetSize) -> ResolvedSize {
        let extra = self.extra_size();
        resolve_preset_size(preset, &self.options, self.working_area.size.w, extra.w)
    }

    fn resolve_preset_height(&self, preset: PresetSize) -> ResolvedSize {
        let extra = self.extra_size();
        resolve_preset_size(preset, &self.options, self.working_area.size.h, extra.h)
    }

    fn resolve_column_width(&self, width: ColumnWidth) -> f64 {
        let working_size = self.working_area.size;
        let gaps = self.options.layout.gaps;
        let extra = self.extra_size();

        match width {
            ColumnWidth::Proportion(proportion) => {
                (working_size.w - gaps) * proportion - gaps - extra.w
            }
            ColumnWidth::Fixed(width) => width,
        }
    }

    fn update_tile_sizes(&mut self, animate: bool) {
        self.update_tile_sizes_with_transaction(animate, Transaction::new());
    }

    fn update_tile_sizes_with_transaction(&mut self, animate: bool, transaction: Transaction) {
        let sizing_mode = self.pending_sizing_mode();
        if matches!(sizing_mode, SizingMode::Fullscreen | SizingMode::Maximized) {
            for (tile_idx, tile) in self.tiles.iter_mut().enumerate() {
                // In tabbed mode, only the visible window participates in the transaction.
                let is_active = tile_idx == self.active_tile_idx;
                let transaction = if self.display_mode == ColumnDisplay::Tabbed && !is_active {
                    None
                } else {
                    Some(transaction.clone())
                };

                if matches!(sizing_mode, SizingMode::Fullscreen) {
                    tile.request_fullscreen(animate, transaction);
                } else {
                    tile.request_maximized(self.parent_area.size, animate, transaction);
                }
            }
            return;
        }

        if self.group.is_some() {
            self.update_group_tile_sizes(animate, transaction);
            return;
        }

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed;

        // Alone in a column: always fill working height (no Hyprland-style hole underneath).
        if self.tiles.len() == 1 && !is_tabbed {
            self.data[0].height = WindowHeight::auto_1();
        }

        let min_size: Vec<_> = self
            .tiles
            .iter()
            .map(Tile::min_size_nonfullscreen)
            .map(|mut size| {
                size.w = size.w.max(1.);
                size.h = size.h.max(1.);
                size
            })
            .collect();
        let max_size: Vec<_> = self
            .tiles
            .iter()
            .map(Tile::max_size_nonfullscreen)
            .collect();

        // Compute the column width.
        let min_width = min_size
            .iter()
            .map(|size| NotNan::new(size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap();
        let max_width = max_size
            .iter()
            .filter_map(|size| {
                let w = size.w;
                if w == 0. {
                    None
                } else {
                    Some(NotNan::new(w).unwrap())
                }
            })
            .min()
            .map(NotNan::into_inner)
            .unwrap_or(f64::from(i32::MAX));
        let max_width = f64::max(max_width, min_width);

        let width = if let Some(w) = self.live_reflow_width {
            w
        } else {
            let width = if self.is_full_width {
                ColumnWidth::Proportion(1.)
            } else {
                self.width
            };
            self.resolve_column_width(width)
        };
        let width = f64::max(f64::min(width, max_width), min_width);
        let working_size = self.working_area.size;
        let extra_size = self.extra_size();
        let max_tile_height = working_size.h - self.options.layout.gaps * 2. - extra_size.h;

        // If there are multiple windows in a column, clamp the non-auto window's height according
        // to other windows' min sizes.
        let mut max_non_auto_window_height = None;
        if self.tiles.len() > 1 && !is_tabbed {
            if let Some(non_auto_idx) = self
                .data
                .iter()
                .position(|data| !matches!(data.height, WindowHeight::Auto { .. }))
            {
                let min_height_taken = min_size
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != non_auto_idx)
                    .map(|(_, min_size)| min_size.h + self.options.layout.gaps)
                    .sum::<f64>();

                let tile = &self.tiles[non_auto_idx];
                let height_left = max_tile_height - min_height_taken;
                max_non_auto_window_height = Some(f64::max(
                    1.,
                    tile.window_height_for_tile_height(height_left).round(),
                ));
            }
        }

        // Compute the tile heights. Start by converting window heights to tile heights.
        let mut heights = zip(&self.tiles, &self.data)
            .map(|(tile, data)| match data.height {
                auto @ WindowHeight::Auto { .. } => auto,
                WindowHeight::Fixed(height) => {
                    let mut window_height = height.round().max(1.);
                    if let Some(max) = max_non_auto_window_height {
                        window_height = f64::min(window_height, max);
                    } else {
                        // In any case, clamp to the working area height.
                        let max = tile.window_height_for_tile_height(max_tile_height).round();
                        window_height = f64::min(window_height, max);
                    }

                    WindowHeight::Fixed(tile.tile_height_for_window_height(window_height))
                }
                WindowHeight::Preset(idx) => {
                    let preset = self.options.layout.preset_window_heights[idx];
                    let window_height = match self.resolve_preset_height(preset) {
                        ResolvedSize::Tile(h) => tile.window_height_for_tile_height(h),
                        ResolvedSize::Window(h) => h,
                    };

                    let mut window_height = window_height.round().clamp(1., 100000.);
                    if let Some(max) = max_non_auto_window_height {
                        window_height = f64::min(window_height, max);
                    }

                    let tile_height = tile.tile_height_for_window_height(window_height);
                    WindowHeight::Fixed(tile_height)
                }
            })
            .collect::<Vec<_>>();

        // In tabbed display mode, fill fixed heights right away.
        if is_tabbed {
            // All tiles have the same height, equal to the height of the only fixed tile (if any).
            let tabbed_height = heights
                .iter()
                .find_map(|h| {
                    if let WindowHeight::Fixed(h) = h {
                        Some(*h)
                    } else {
                        None
                    }
                })
                .unwrap_or(max_tile_height);

            // We also take min height of all tabs into account.
            let min_height = min_size
                .iter()
                .map(|size| NotNan::new(size.h).unwrap())
                .max()
                .map(NotNan::into_inner)
                .unwrap();
            // But, if there's a larger-than-workspace tab, we don't want to force all tabs to that
            // size.
            let min_height = f64::min(max_tile_height, min_height);
            let tabbed_height = f64::max(tabbed_height, min_height);

            heights.fill(WindowHeight::Fixed(tabbed_height));

            // The following logic will apply individual min/max height, etc.
        }

        let extra_auto = if self.live_reflow_tile_gap.is_some() && !is_tabbed {
            1usize
        } else {
            0
        };

        let gaps_left = self.options.layout.gaps * (self.tiles.len() + 1 + extra_auto) as f64;
        let mut height_left = working_size.h - gaps_left;
        let mut auto_tiles_left = self.tiles.len();

        // Subtract all fixed-height tiles.
        for (h, (min_size, max_size)) in zip(&mut heights, zip(&min_size, &max_size)) {
            // Check if the tile has an exact height constraint.
            if min_size.h == max_size.h {
                *h = WindowHeight::Fixed(min_size.h);
            }

            if let WindowHeight::Fixed(h) = h {
                if max_size.h > 0. {
                    *h = f64::min(*h, max_size.h);
                }
                *h = f64::max(*h, min_size.h);

                height_left -= *h;
                auto_tiles_left -= 1;
            }
        }

        let mut total_weight: f64 = heights
            .iter()
            .filter_map(|h| {
                if let WindowHeight::Auto { weight } = *h {
                    Some(weight)
                } else {
                    None
                }
            })
            .sum();
        if extra_auto > 0 {
            total_weight += extra_auto as f64;
        }

        // Iteratively try to distribute the remaining height, checking against tile min heights.
        // Pick an auto height according to the current sizes, then check if it satisfies all
        // remaining min heights. If not, allocate fixed height to those tiles and repeat the
        // loop. On each iteration the auto height will get smaller.
        //
        // NOTE: we do not respect max height here. Doing so would complicate things: if the current
        // auto height is above some tile's max height, then the auto height can become larger.
        // Combining this with the min height loop is where the complexity appears.
        //
        // However, most max height uses are for fixed-size dialogs, where min height == max_height.
        // This case is separately handled above.
        'outer: while auto_tiles_left > 0 {
            // Wayland requires us to round the requested size for a window to integer logical
            // pixels, therefore we compute the remaining auto height dynamically.
            let mut height_left_2 = height_left;
            let mut total_weight_2 = total_weight;
            for ((h, tile), min_size) in zip(zip(&mut heights, &self.tiles), &min_size) {
                let weight = match *h {
                    WindowHeight::Auto { weight } => weight,
                    WindowHeight::Fixed(_) => continue,
                    WindowHeight::Preset(_) => unreachable!(),
                };
                let factor = weight / total_weight_2;

                // Compute the current auto height.
                let mut auto = height_left_2 * factor;

                // Check if the auto height satisfies the min height.
                if min_size.h > auto {
                    auto = min_size.h;
                    *h = WindowHeight::Fixed(auto);
                    height_left -= auto;
                    total_weight -= weight;
                    auto_tiles_left -= 1;

                    // If a min height was unsatisfied, then we allocate the tile more than the
                    // auto height, which means that the remaining auto tiles now have less height
                    // to work with, and the loop must run again.
                    //
                    // If we keep going in this loop and break out later, we may allocate less
                    // height to the subsequent tiles than would be available next iteration and
                    // potentially trip their min height check earlier than necessary, leading to
                    // visible snapping.
                    continue 'outer;
                }

                auto = tile.tile_height_for_window_height(
                    tile.window_height_for_tile_height(auto).round().max(1.),
                );

                height_left_2 -= auto;
                total_weight_2 -= weight;
            }

            // All min heights were satisfied, fill them in.
            for (h, tile) in zip(&mut heights, &self.tiles) {
                let weight = match *h {
                    WindowHeight::Auto { weight } => weight,
                    WindowHeight::Fixed(_) => continue,
                    WindowHeight::Preset(_) => unreachable!(),
                };
                let factor = weight / total_weight;

                // Compute the current auto height.
                let auto = height_left * factor;
                let auto = tile.tile_height_for_window_height(
                    tile.window_height_for_tile_height(auto).round().max(1.),
                );

                *h = WindowHeight::Fixed(auto);
                height_left -= auto;
                total_weight -= weight;
                auto_tiles_left -= 1;
            }

            assert_eq!(auto_tiles_left, 0);
        }

        let mut layout_heights = Vec::with_capacity(self.tiles.len());
        for (tile_idx, (tile, h)) in zip(&mut self.tiles, heights).enumerate() {
            let WindowHeight::Fixed(height) = h else {
                unreachable!()
            };
            layout_heights.push(height);

            let size = Size::from((width, height));

            // In tabbed mode, only the visible window participates in the transaction.
            let is_active = tile_idx == self.active_tile_idx;
            let transaction = if self.display_mode == ColumnDisplay::Tabbed && !is_active {
                None
            } else {
                Some(transaction.clone())
            };

            tile.request_tile_size(size, animate, transaction);
        }

        if extra_auto > 0 {
            if let Some((_, gap_h)) = &mut self.live_reflow_tile_gap {
                *gap_h = height_left.max(1.);
            }
            self.live_reflow_tile_heights = Some(layout_heights);
        } else {
            self.live_reflow_tile_heights = None;
        }
    }

    /// Space a tiled group's chrome takes besides its cells: the side bar on the left and the
    /// bottom bar below, each with a gap.
    fn group_chrome_extra(&self) -> Size<f64, Logical> {
        group_extra(self.options.layout.gaps)
    }

    /// Column width taken by a tiled group besides its cells.
    fn group_extra_w(&self) -> f64 {
        self.group_chrome_extra().w
    }

    /// Side bar rect of the tiled group relative to the tiles origin.
    fn group_side_bar(&self) -> Option<Rectangle<f64, Logical>> {
        let group = self.group.as_ref()?;
        let mut cells_box = group.bbox();
        cells_box.loc.x += self.group_extra_w();
        Some(side_bar_rect(cells_box, self.options.layout.gaps))
    }

    /// Limits of a tiled group's cells box: the column band minus the side and bottom bars.
    fn group_cell_limits(&self) -> GroupLimits {
        let mut limits = GroupLimits::from_band(self.working_area, self.options.layout.gaps);
        let extra = self.group_chrome_extra();
        limits.max.w = (limits.max.w - extra.w).max(1.);
        limits.max.h = (limits.max.h - extra.h).max(1.);
        limits.min.w = limits.min.w.min(limits.max.w);
        limits.min.h = limits.min.h.min(limits.max.h);
        limits
    }

    /// Group sizing: the cells scale to the column width (minus the bar) and never exceed the
    /// band height; every member is asked for its cell size.
    fn update_group_tile_sizes(&mut self, animate: bool, transaction: Transaction) {
        let width = if let Some(w) = self.live_reflow_width {
            w
        } else {
            let width = if self.is_full_width {
                ColumnWidth::Proportion(1.)
            } else {
                self.width
            };
            self.resolve_column_width(width)
        };
        let gaps = self.options.layout.gaps;
        let max = self.group_cell_limits().max;
        let content_w = (width - self.group_extra_w()).clamp(1., max.w);
        let Some(group) = &mut self.group else {
            return;
        };
        let all = group.all_cells();
        let from = union_all(&all);
        let to_h = from.size.h.min(max.h);
        if (from.size.w - content_w).abs() > 0.01
            || (from.size.h - to_h).abs() > 0.01
            || from.loc != Point::default()
        {
            let to = Rectangle::from_size(Size::from((content_w, to_h)));
            group.set_all_cells(scale_cells(&all, from, to, gaps));
        }
        for (tile, cell) in zip(&mut self.tiles, &group.cells) {
            tile.request_tile_size(cell.size, animate, Some(transaction.clone()));
        }
        self.live_reflow_tile_heights = None;
    }

    /// Scale a group's cells to fill the band height (explicit group actions).
    fn fill_group_height(&mut self) {
        let gaps = self.options.layout.gaps;
        let max_h = self.group_cell_limits().max.h;
        if let Some(group) = &mut self.group {
            let all = group.all_cells();
            let from = union_all(&all);
            let to = Rectangle::from_size(Size::from((from.size.w, max_h)));
            group.set_all_cells(scale_cells(&all, from, to, gaps));
        }
    }

    pub fn is_group(&self) -> bool {
        self.group.is_some()
    }

    /// A member at `idx` already left `tiles`/`data`: the rest grow into its cell, or the
    /// column becomes a normal column when one member is left.
    fn group_member_removed(&mut self, idx: usize) {
        let Some(group) = &mut self.group else {
            return;
        };
        if idx >= group.cells.len() {
            return;
        }
        let total_w = group_extra(self.options.layout.gaps).w + group.bbox().size.w;
        let removed = group.cells.remove(idx);
        self.group_cell_freed(removed, total_w);
    }

    /// The cell `removed` of a group whose box was `total_w` wide (bar included) was freed:
    /// the members grow into it, or the column becomes a normal column when one slot is left.
    /// A reserved cell counts as a slot and is kept.
    fn group_cell_freed(&mut self, removed: Rectangle<f64, Logical>, total_w: f64) {
        let gaps = self.options.layout.gaps;
        let Some(group) = &mut self.group else {
            return;
        };
        let slots = group.cells.len() + usize::from(group.reserved.is_some());
        if slots >= 2 {
            if group.cells.len() >= 2 {
                group.cells = redistribute(&group.cells, removed, gaps);
            }
            let all = group.all_cells();
            group.set_all_cells(all);
        } else {
            self.group = None;
            self.width = ColumnWidth::Fixed(total_w);
            self.preset_width_idx = None;
            for data in &mut self.data {
                data.height = WindowHeight::auto_1();
            }
        }
    }

    fn width(&self) -> f64 {
        if let Some(w) = self.live_reflow_width {
            return w;
        }

        if let Some(group) = &self.group {
            return self.group_extra_w() + group.bbox().size.w;
        }

        let mut tiles_width = self
            .data
            .iter()
            .map(|data| NotNan::new(data.size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap();

        if self.display_mode == ColumnDisplay::Tabbed && self.sizing_mode().is_normal() {
            let extra_size = self.tab_indicator.extra_size(self.tiles.len(), self.scale);
            tiles_width += extra_size.w;
        }

        tiles_width
    }

    fn focus_index(&mut self, index: u8) {
        let idx = min(usize::from(index.saturating_sub(1)), self.tiles.len() - 1);
        self.activate_idx(idx);
    }

    fn focus_up(&mut self) -> bool {
        self.activate_idx(self.active_tile_idx.saturating_sub(1))
    }

    fn focus_down(&mut self) -> bool {
        self.activate_idx(min(self.active_tile_idx + 1, self.tiles.len() - 1))
    }

    fn focus_top(&mut self) {
        self.activate_idx(0);
    }

    fn focus_bottom(&mut self) {
        self.activate_idx(self.tiles.len() - 1);
    }

    fn move_up(&mut self) -> bool {
        if self.group.is_some() {
            return false;
        }
        let new_idx = self.active_tile_idx.saturating_sub(1);
        if self.active_tile_idx == new_idx {
            return false;
        }

        let mut ys = self.tile_offsets().skip(self.active_tile_idx);
        let active_y = ys.next().unwrap().y;
        let next_y = ys.next().unwrap().y;
        drop(ys);

        self.tiles.swap(self.active_tile_idx, new_idx);
        self.data.swap(self.active_tile_idx, new_idx);
        self.active_tile_idx = new_idx;

        // Animate the movement.
        let new_active_y = self.tile_offset(new_idx).y;
        self.tiles[new_idx].animate_move_y_from(active_y - new_active_y);
        self.tiles[new_idx + 1].animate_move_y_from(active_y - next_y);

        true
    }

    fn move_down(&mut self) -> bool {
        if self.group.is_some() {
            return false;
        }
        let new_idx = min(self.active_tile_idx + 1, self.tiles.len() - 1);
        if self.active_tile_idx == new_idx {
            return false;
        }

        let mut ys = self.tile_offsets().skip(self.active_tile_idx);
        let active_y = ys.next().unwrap().y;
        let next_y = ys.next().unwrap().y;
        drop(ys);

        self.tiles.swap(self.active_tile_idx, new_idx);
        self.data.swap(self.active_tile_idx, new_idx);
        self.active_tile_idx = new_idx;

        // Animate the movement.
        let new_active_y = self.tile_offset(new_idx).y;
        self.tiles[new_idx].animate_move_y_from(active_y - new_active_y);
        self.tiles[new_idx - 1].animate_move_y_from(next_y - active_y);

        true
    }

    fn toggle_width(&mut self, tile_idx: Option<usize>, forwards: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);

        let preset_idx = if self.is_full_width || self.is_pending_maximized {
            None
        } else {
            self.preset_width_idx
        };

        let len = self.options.layout.preset_column_widths.len();
        let preset_idx = if let Some(idx) = preset_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let tile = &self.tiles[tile_idx];
            let current_window = tile.window_expected_or_current_size().w;
            let current_tile = tile.tile_expected_or_current_size().w;

            let mut it = self
                .options
                .layout
                .preset_column_widths
                .iter()
                .map(|preset| self.resolve_preset_width(*preset));

            if forwards {
                it.position(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => current_tile + 1. < resolved,
                        ResolvedSize::Window(resolved) => current_window + 1. < resolved,
                    }
                })
                .unwrap_or(0)
            } else {
                it.rposition(|resolved| {
                    match resolved {
                        // Some allowance for fractional scaling purposes.
                        ResolvedSize::Tile(resolved) => resolved + 1. < current_tile,
                        ResolvedSize::Window(resolved) => resolved + 1. < current_window,
                    }
                })
                .unwrap_or(len - 1)
            }
        };

        let preset = self.options.layout.preset_column_widths[preset_idx];
        self.set_column_width(SizeChange::from(preset), Some(tile_idx), true);

        self.preset_width_idx = Some(preset_idx);
    }

    fn toggle_full_width(&mut self) {
        if self.is_pending_maximized {
            // Treat it as unmaximize.
            self.is_pending_maximized = false;
            self.is_full_width = false;
        } else {
            self.is_full_width = !self.is_full_width;
        }

        self.update_tile_sizes(true);
    }

    fn set_column_width(&mut self, change: SizeChange, tile_idx: Option<usize>, animate: bool) {
        let current = if self.is_full_width || self.is_pending_maximized {
            ColumnWidth::Proportion(1.)
        } else {
            self.width
        };

        let current_px = self.resolve_column_width(current);

        // FIXME: fix overflows then remove limits.
        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let width = match (current, change) {
            (_, SizeChange::SetFixed(fixed)) => {
                // As a special case, setting a fixed column width will compute it in such a way
                // that the specified (usually active) window gets that width. This is the
                // intention behind the ability to set a fixed size.
                let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);
                let tile = &self.tiles[tile_idx];
                ColumnWidth::Fixed(
                    tile.tile_width_for_window_width(f64::from(fixed))
                        .clamp(1., MAX_PX),
                )
            }
            (_, SizeChange::SetProportion(proportion)) => {
                ColumnWidth::Proportion((proportion / 100.).clamp(0., MAX_F))
            }
            (_, SizeChange::AdjustFixed(delta)) => {
                let width = (current_px + f64::from(delta)).clamp(1., MAX_PX);
                ColumnWidth::Fixed(width)
            }
            (ColumnWidth::Proportion(current), SizeChange::AdjustProportion(delta)) => {
                let proportion = (current + delta / 100.).clamp(0., MAX_F);
                ColumnWidth::Proportion(proportion)
            }
            (ColumnWidth::Fixed(_), SizeChange::AdjustProportion(delta)) => {
                let full = self.working_area.size.w - self.options.layout.gaps;
                let current = if full == 0. {
                    1.
                } else {
                    (current_px + self.options.layout.gaps + self.extra_size().w) / full
                };
                let proportion = (current + delta / 100.).clamp(0., MAX_F);
                ColumnWidth::Proportion(proportion)
            }
        };

        self.width = width;
        self.preset_width_idx = None;
        self.is_full_width = false;
        self.is_pending_maximized = false;
        self.update_tile_sizes(animate);
    }

    fn set_window_height(&mut self, change: SizeChange, tile_idx: Option<usize>, animate: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);

        // Start by converting all heights to automatic, since only one window in the column can be
        // non-auto-height. If the current tile is already non-auto, however, we can skip that
        // step. Which is not only for optimization, but also preserves automatic weights in case
        // one window is resized in such a way that other windows hit their min size, and then
        // back.
        if matches!(self.data[tile_idx].height, WindowHeight::Auto { .. }) {
            self.convert_heights_to_auto();
        }

        let current = self.data[tile_idx].height;
        let tile = &self.tiles[tile_idx];
        let current_window_px = match current {
            WindowHeight::Auto { .. } | WindowHeight::Preset(_) => tile.window_size().h,
            WindowHeight::Fixed(height) => height,
        };
        let current_tile_px = tile.tile_height_for_window_height(current_window_px);

        let working_size = self.working_area.size.h;
        let gaps = self.options.layout.gaps;
        let extra_size = self.extra_size().h;
        let full = working_size - gaps;
        let current_prop = if full == 0. {
            1.
        } else {
            (current_tile_px + gaps) / full
        };

        // FIXME: fix overflows then remove limits.
        const MAX_PX: f64 = 100000.;

        let mut window_height = match change {
            SizeChange::SetFixed(fixed) => f64::from(fixed),
            SizeChange::SetProportion(proportion) => {
                let tile_height = (working_size - gaps) * (proportion / 100.) - gaps - extra_size;
                tile.window_height_for_tile_height(tile_height)
            }
            SizeChange::AdjustFixed(delta) => current_window_px + f64::from(delta),
            SizeChange::AdjustProportion(delta) => {
                let proportion = current_prop + delta / 100.;
                let tile_height = (working_size - gaps) * proportion - gaps - extra_size;
                tile.window_height_for_tile_height(tile_height)
            }
        };

        // Clamp the height according to other windows' min sizes, or simply to working area height.
        let min_height_taken = if self.display_mode == ColumnDisplay::Tabbed {
            0.
        } else {
            self.tiles
                .iter()
                .enumerate()
                .filter(|(idx, _)| *idx != tile_idx)
                .map(|(_, tile)| f64::max(1., tile.min_size_nonfullscreen().h) + gaps)
                .sum::<f64>()
        };
        let height_left = working_size - extra_size - gaps - min_height_taken - gaps;
        let height_left = f64::max(1., tile.window_height_for_tile_height(height_left));
        window_height = f64::min(height_left, window_height);

        // Clamp it against the window height constraints.
        let win = &self.tiles[tile_idx].window();
        let min_h = win.min_size().h;
        let max_h = win.max_size().h;

        if max_h > 0 {
            window_height = f64::min(window_height, f64::from(max_h));
        }
        if min_h > 0 {
            window_height = f64::max(window_height, f64::from(min_h));
        }

        self.data[tile_idx].height = WindowHeight::Fixed(window_height.clamp(1., MAX_PX));
        self.is_pending_maximized = false;
        self.update_tile_sizes(animate);
    }

    fn reset_window_height(&mut self, tile_idx: Option<usize>) {
        if self.display_mode == ColumnDisplay::Tabbed {
            // When tabbed, reset window height should work on any window, not just the fixed-size
            // one.
            for data in &mut self.data {
                data.height = WindowHeight::auto_1();
            }
        } else {
            let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);
            self.data[tile_idx].height = WindowHeight::auto_1();
        }

        self.update_tile_sizes(true);
    }

    fn toggle_window_height(&mut self, tile_idx: Option<usize>, forwards: bool) {
        let tile_idx = tile_idx.unwrap_or(self.active_tile_idx);

        // Start by converting all heights to automatic, since only one window in the column can be
        // non-auto-height. If the current tile is already non-auto, however, we can skip that
        // step. Which is not only for optimization, but also preserves automatic weights in case
        // one window is resized in such a way that other windows hit their min size, and then
        // back.
        if matches!(self.data[tile_idx].height, WindowHeight::Auto { .. }) {
            self.convert_heights_to_auto();
        }

        let len = self.options.layout.preset_window_heights.len();
        let preset_idx = match self.data[tile_idx].height {
            WindowHeight::Preset(idx) if !self.is_pending_maximized => {
                (idx + if forwards { 1 } else { len - 1 }) % len
            }
            _ => {
                let current = self.data[tile_idx].size.h;
                let tile = &self.tiles[tile_idx];

                let mut it = self
                    .options
                    .layout
                    .preset_window_heights
                    .iter()
                    .copied()
                    .map(|preset| {
                        let window_height = match self.resolve_preset_height(preset) {
                            ResolvedSize::Tile(h) => tile.window_height_for_tile_height(h),
                            ResolvedSize::Window(h) => h,
                        };
                        tile.tile_height_for_window_height(window_height.round().clamp(1., 100000.))
                    });

                if forwards {
                    it.position(|resolved| {
                        // Some allowance for fractional scaling purposes.
                        current + 1. < resolved
                    })
                    .unwrap_or(0)
                } else {
                    it.rposition(|resolved| {
                        // Some allowance for fractional scaling purposes.
                        resolved + 1. < current
                    })
                    .unwrap_or(len - 1)
                }
            }
        };
        self.data[tile_idx].height = WindowHeight::Preset(preset_idx);
        self.is_pending_maximized = false;
        self.update_tile_sizes(true);
    }

    /// Converts all heights in the column to automatic, preserving the apparent heights.
    ///
    /// All weights are recomputed to preserve the current tile heights while "centering" the
    /// weights at the median window height (it gets weight = 1).
    ///
    /// One case where apparent heights will not be preserved is when the column is taller than the
    /// working area.
    fn convert_heights_to_auto(&mut self) {
        let heights: Vec<_> = self.tiles.iter().map(|tile| tile.tile_size().h).collect();

        // Weights are invariant to multiplication: a column with weights 2, 2, 1 is equivalent to
        // a column with weights 4, 4, 2. So we find the median window height and use that as 1.
        let mut sorted = heights.clone();
        sorted.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];

        for (data, height) in zip(&mut self.data, heights) {
            let weight = height / median;
            data.height = WindowHeight::Auto { weight };
        }
    }

    fn set_fullscreen(&mut self, is_fullscreen: bool) {
        if self.is_pending_fullscreen == is_fullscreen {
            return;
        }

        if is_fullscreen {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        self.is_pending_fullscreen = is_fullscreen;
        self.update_tile_sizes(true);
    }

    fn set_maximized(&mut self, maximize: bool) {
        if self.is_pending_maximized == maximize {
            return;
        }

        if maximize {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        self.is_pending_maximized = maximize;
        self.update_tile_sizes(true);
    }

    fn set_column_display(&mut self, display: ColumnDisplay) {
        if self.display_mode == display {
            return;
        }

        // Animate the movement.
        //
        // We're doing some shortcuts here because we know that currently normal vs. tabbed can
        // only cause a vertical shift + a shift to the origin.
        //
        // Doing it this way to avoid storing all tile positions in a vector. If more display modes
        // are added it might be simpler to just collect everything into a smallvec.
        let prev_origin = self.tiles_origin();
        self.display_mode = display;
        let new_origin = self.tiles_origin();
        let origin_delta = prev_origin - new_origin;

        // When need to walk the tiles in the normal display mode to get the right offsets.
        self.display_mode = ColumnDisplay::Normal;
        for (tile, pos) in self.tiles_mut() {
            let mut y_delta = pos.y - prev_origin.y;

            // Invert the Y motion when transitioning *to* normal display mode.
            if display == ColumnDisplay::Normal {
                y_delta *= -1.;
            }

            let mut delta = origin_delta;
            delta.y += y_delta;
            tile.animate_move_from(delta);
        }

        // Animate the opacity.
        for (idx, tile) in self.tiles.iter_mut().enumerate() {
            let is_active = idx == self.active_tile_idx;
            if !is_active {
                let (from, to) = if display == ColumnDisplay::Tabbed {
                    (1., 0.)
                } else {
                    (0., 1.)
                };
                tile.animate_alpha(from, to, self.options.animations.window_movement.0);
            }
        }

        // Animate the appearance of the tab indicator.
        if display == ColumnDisplay::Tabbed {
            self.tab_indicator.start_open_animation(
                self.clock.clone(),
                self.options.animations.window_movement.0,
            );
        }

        // Now switch the display mode for real.
        self.display_mode = display;
        self.update_tile_sizes(true);
    }

    fn tiles_origin(&self) -> Point<f64, Logical> {
        let mut origin = Point::from((0., 0.));

        match self.sizing_mode() {
            SizingMode::Normal => (),
            SizingMode::Maximized => {
                origin.y += self.parent_area.loc.y;
                return origin;
            }
            SizingMode::Fullscreen => return origin,
        }

        origin.y += self.working_area.loc.y + self.options.layout.gaps;

        if self.display_mode == ColumnDisplay::Tabbed {
            origin += self
                .tab_indicator
                .content_offset(self.tiles.len(), self.scale);
        }

        origin
    }

    // HACK: pass a self.data iterator in manually as a workaround for the lack of method partial
    // borrowing. Note that this method's return value does not borrow the entire &Self!
    fn tile_offsets_iter(
        &self,
        data: impl Iterator<Item = TileData>,
    ) -> impl Iterator<Item = Point<f64, Logical>> {
        // FIXME: this should take into account always-center-single-column, which means that
        // Column should somehow know when it is being centered due to being the single column on
        // the workspace or some other reason.
        let center = self.options.layout.center_focused_column == CenterFocusedColumn::Always;
        let gaps = self.options.layout.gaps;
        let tabbed = self.display_mode == ColumnDisplay::Tabbed;

        // Does not include extra size from the tab indicator.
        let tiles_width = self
            .data
            .iter()
            .map(|data| NotNan::new(data.size.w).unwrap())
            .max()
            .map(NotNan::into_inner)
            .unwrap_or(0.);

        let mut origin = self.tiles_origin();
        let group_cells = self.group.as_ref().map(|g| g.cells.clone());
        let extra = self.group_chrome_extra();
        let group_h = self
            .group
            .as_ref()
            .map_or(0., |g| g.bbox().size.h + extra.h);

        // Chain with a dummy value to be able to get one past all tiles' Y.
        let dummy = TileData {
            height: WindowHeight::auto_1(),
            size: Size::default(),
            interactively_resizing_by_left_edge: false,
        };
        let data = data.chain(iter::once(dummy));
        let tile_gap = self.live_reflow_tile_gap;
        let layout_heights = self.live_reflow_tile_heights.clone();
        let mut idx = 0usize;

        data.map(move |data| {
            if let Some(cells) = &group_cells {
                let pos = match cells.get(idx) {
                    Some(c) => origin + Point::from((extra.w + c.loc.x, c.loc.y)),
                    None if cells.is_empty() => origin,
                    None => origin + Point::from((0., group_h + gaps)),
                };
                idx += 1;
                return pos;
            }

            if !tabbed {
                if let Some((gap_idx, height)) = tile_gap {
                    if idx == gap_idx {
                        origin.y += height + gaps;
                    }
                }
            }

            let mut pos = origin;

            if center {
                pos.x += (tiles_width - data.size.w) / 2.;
            } else if data.interactively_resizing_by_left_edge {
                pos.x += tiles_width - data.size.w;
            }

            if !tabbed {
                let h = layout_heights
                    .as_ref()
                    .and_then(|hs| hs.get(idx).copied())
                    .unwrap_or(data.size.h);
                origin.y += h + gaps;
            }

            idx += 1;
            pos
        })
    }

    fn tile_offsets(&self) -> impl Iterator<Item = Point<f64, Logical>> + '_ {
        self.tile_offsets_iter(self.data.iter().copied())
    }

    fn tile_insert_ys(&self) -> Vec<f64> {
        let gaps = self.options.layout.gaps;
        let mut y = self.tiles_origin().y;
        let mut ys = Vec::with_capacity(self.data.len() + 2);
        let layout_heights = self.live_reflow_tile_heights.as_deref();

        for (idx, data) in self.data.iter().enumerate() {
            if let Some((gap_idx, height)) = self.live_reflow_tile_gap {
                if idx == gap_idx {
                    ys.push(y);
                    y += height + gaps;
                }
            }
            ys.push(y);
            let h = layout_heights
                .and_then(|hs| hs.get(idx).copied())
                .unwrap_or(data.size.h);
            y += h + gaps;
        }

        if let Some((gap_idx, height)) = self.live_reflow_tile_gap {
            if gap_idx == self.data.len() {
                ys.push(y);
                y += height + gaps;
            }
        }

        ys.push(y);
        ys
    }

    fn tile_offset(&self, tile_idx: usize) -> Point<f64, Logical> {
        self.tile_offsets().nth(tile_idx).unwrap()
    }

    fn tile_offsets_in_render_order(
        &self,
        data: impl Iterator<Item = TileData>,
    ) -> impl Iterator<Item = Point<f64, Logical>> {
        let active_idx = self.active_tile_idx;
        let active_pos = self.tile_offset(active_idx);
        let offsets = self
            .tile_offsets_iter(data)
            .enumerate()
            .filter_map(move |(idx, pos)| (idx != active_idx).then_some(pos));
        iter::once(active_pos).chain(offsets)
    }

    pub fn tiles(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        zip(&self.tiles, offsets)
    }

    fn tiles_mut(&mut self) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_iter(self.data.iter().copied());
        zip(&mut self.tiles, offsets)
    }

    fn tiles_in_render_order(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> + '_ {
        let offsets = self.tile_offsets_in_render_order(self.data.iter().copied());

        let (first, rest) = self.tiles.split_at(self.active_tile_idx);
        let (active, rest) = rest.split_at(1);

        let active = active.iter().map(|tile| (tile, true));

        let rest_visible = self.display_mode != ColumnDisplay::Tabbed;
        let rest = first.iter().chain(rest);
        let rest = rest.map(move |tile| (tile, rest_visible));

        let tiles = active.chain(rest);
        zip(tiles, offsets).map(|((tile, visible), pos)| (tile, pos, visible))
    }

    fn tiles_in_render_order_mut(
        &mut self,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.tile_offsets_in_render_order(self.data.iter().copied());

        let (first, rest) = self.tiles.split_at_mut(self.active_tile_idx);
        let (active, rest) = rest.split_at_mut(1);

        let tiles = active.iter_mut().chain(first).chain(rest);
        zip(tiles, offsets)
    }

    fn tab_indicator_area(&self) -> Rectangle<f64, Logical> {
        // We'd like to use the active tile's animated size for the tab indicator, however we need
        // to be mindful of the case where the active tile is smaller than some other tile in the
        // column. The column assumes the size of the largest tile.
        //
        // We expect users to mainly resize tabbed columns by width, so matching the animated size
        // is more important here. Besides, we always try to resize all windows in a column to the
        // same width when possible, and also the animation for going into tabbed mode doesn't move
        // tiles horizontally as much.
        //
        // For height though, it's a different story. First, users probably aren't resizing a
        // tabbed column by height. Second, we don't match windows by height, so it's easy to have
        // a smaller active tile than the rest of the column, e.g. by adding a fixed-size dialog.
        // Then, switching to that dialog and back should ideally keep the tab indicator position
        // fixed. Third, the animation for making a column tabbed moves tiles vertically, and using
        // the active tile's animated size in this case only works for the topmost tile, and looks
        // broken otherwise.
        let mut max_height = 0.;
        for tile in &self.tiles {
            max_height = f64::max(max_height, tile.tile_size().h);
        }

        let tile = &self.tiles[self.active_tile_idx];
        let area_size = Size::from((tile.animated_tile_size().w, max_height));

        Rectangle::new(self.tiles_origin(), area_size)
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        for tile in &mut self.tiles {
            if tile.window().id() == id {
                tile.start_open_animation();

                // Animate the appearance of the tab indicator.
                if self.display_mode == ColumnDisplay::Tabbed
                    && self.sizing_mode().is_normal()
                    && self.tiles.len() == 1
                    && !self.tab_indicator.config().hide_when_single_tab
                {
                    self.tab_indicator.start_open_animation(
                        self.clock.clone(),
                        self.options.animations.window_open.anim,
                    );
                }

                return true;
            }
        }

        false
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        assert!(!self.tiles.is_empty(), "columns can't be empty");
        assert!(self.active_tile_idx < self.tiles.len());
        assert_eq!(self.tiles.len(), self.data.len());

        if !self.pending_sizing_mode().is_normal() {
            assert!(self.tiles.len() == 1 || self.display_mode == ColumnDisplay::Tabbed);
        }

        if let Some(idx) = self.preset_width_idx {
            assert!(idx < self.options.layout.preset_column_widths.len());
        }

        let is_tabbed = self.display_mode == ColumnDisplay::Tabbed;

        if let Some(group) = &self.group {
            use super::floating_group::rects_overlap;

            let slots = self.tiles.len() + usize::from(group.reserved.is_some());
            assert!(
                (2..=MAX_GROUP_MEMBERS).contains(&slots),
                "tiled groups have 2..={MAX_GROUP_MEMBERS} members (reserved cell included)"
            );
            assert!(!self.tiles.is_empty());
            assert_eq!(group.cells.len(), self.tiles.len());
            assert!(!is_tabbed, "tiled groups are never tabbed");
            assert!(self.pending_sizing_mode().is_normal());
            let all = group.all_cells();
            let union = union_all(&all);
            assert!(union.loc.x.abs() < 0.01 && union.loc.y.abs() < 0.01);
            let max = self.group_cell_limits().max;
            assert!(
                union.size.h <= max.h + 0.5,
                "group cells must fit the band height ({} > {})",
                union.size.h,
                max.h
            );
            assert!(
                union.size.w <= max.w + 0.5,
                "group cells must fit the band width ({} > {})",
                union.size.w,
                max.w
            );
            for (i, a) in all.iter().enumerate() {
                for b in &all[i + 1..] {
                    assert!(!rects_overlap(*a, *b), "group cells must not overlap");
                }
            }
            for (tile, data) in zip(&self.tiles, &self.data) {
                assert!(Rc::ptr_eq(&self.options, &tile.options));
                assert_eq!(self.clock, tile.clock);
                assert_eq!(self.scale, tile.scale());
                assert_eq!(self.view_size, tile.view_size());
                tile.verify_invariants();
                let mut data2 = *data;
                data2.update(tile);
                assert_eq!(data, &data2, "tile data must be up to date");
            }
            return;
        }

        let tile_count = self.tiles.len();
        if tile_count == 1 {
            if let WindowHeight::Auto { weight } = self.data[0].height {
                assert_eq!(
                    weight, 1.,
                    "auto height weight must reset to 1 for a single window"
                );
            }
        }

        let working_size = self.working_area.size;
        let extra_size = self.extra_size();
        let gaps = self.options.layout.gaps;

        let mut found_fixed = false;
        let mut total_height = 0.;
        let mut total_min_height = 0.;
        for (tile, data) in zip(&self.tiles, &self.data) {
            assert!(Rc::ptr_eq(&self.options, &tile.options));
            assert_eq!(self.clock, tile.clock);
            assert_eq!(self.scale, tile.scale());
            assert_eq!(
                self.pending_sizing_mode(),
                tile.window().pending_sizing_mode()
            );
            assert_eq!(self.view_size, tile.view_size());
            tile.verify_invariants();

            let mut data2 = *data;
            data2.update(tile);
            assert_eq!(data, &data2, "tile data must be up to date");

            if matches!(data.height, WindowHeight::Fixed(_)) {
                assert!(
                    !found_fixed,
                    "there can only be one fixed-height window in a column"
                );
                found_fixed = true;
            }

            if let WindowHeight::Preset(idx) = data.height {
                assert!(self.options.layout.preset_window_heights.len() > idx);
            }

            let requested_size = tile.window().requested_size().unwrap();
            let requested_tile_height =
                tile.tile_height_for_window_height(f64::from(requested_size.h));
            let min_tile_height = f64::max(1., tile.min_size_nonfullscreen().h);

            if !is_tabbed
                && self.pending_sizing_mode().is_normal()
                && self.scale.round() == self.scale
                && working_size.h.round() == working_size.h
                && gaps.round() == gaps
            {
                let total_height = requested_tile_height + gaps * 2. + extra_size.h;
                let total_min_height = min_tile_height + gaps * 2. + extra_size.h;
                let max_height = f64::max(total_min_height, working_size.h);
                assert!(
                    total_height <= max_height,
                    "each tile in a column mustn't go beyond working area height \
                     (tile height {total_height} > max height {max_height})"
                );
            }

            total_height += requested_tile_height;
            total_min_height += min_tile_height;
        }

        if !is_tabbed
            && tile_count > 1
            && self.scale.round() == self.scale
            && working_size.h.round() == working_size.h
            && gaps.round() == gaps
        {
            total_height += gaps * (tile_count + 1) as f64 + extra_size.h;
            total_min_height += gaps * (tile_count + 1) as f64 + extra_size.h;
            let max_height = f64::max(total_min_height, working_size.h);
            assert!(
                total_height <= max_height,
                "multiple tiles in a column mustn't go beyond working area height \
                 (total height {total_height} > max height {max_height})"
            );
        }
    }
}

fn compute_new_view_offset(
    cur_x: f64,
    view_width: f64,
    new_col_x: f64,
    new_col_width: f64,
    gaps: f64,
) -> f64 {
    // If the column is wider than the view, always left-align it.
    if view_width <= new_col_width {
        return 0.;
    }

    // Compute the padding in case it needs to be smaller due to large tile width.
    let padding = ((view_width - new_col_width) / 2.).clamp(0., gaps);

    // Compute the desired new X with padding.
    let new_x = new_col_x - padding;
    let new_right_x = new_col_x + new_col_width + padding;

    // If the column is already fully visible, leave the view as is.
    if cur_x <= new_x && new_right_x <= cur_x + view_width {
        return -(new_col_x - cur_x);
    }

    // Otherwise, prefer the alignment that results in less motion from the current position.
    let dist_to_left = (cur_x - new_x).abs();
    let dist_to_right = ((cur_x + view_width) - new_right_x).abs();
    if dist_to_left <= dist_to_right {
        -padding
    } else {
        -(view_width - padding - new_col_width)
    }
}

/// Working area used by tiled columns: exclusive zone (`parent_area`) plus struts.
pub(super) fn compute_working_area(
    parent_area: Rectangle<f64, Logical>,
    scale: f64,
    struts: Struts,
) -> Rectangle<f64, Logical> {
    let mut working_area = parent_area;

    // Add struts.
    working_area.size.w = f64::max(0., working_area.size.w - struts.left.0 - struts.right.0);
    working_area.loc.x += struts.left.0;

    working_area.size.h = f64::max(0., working_area.size.h - struts.top.0 - struts.bottom.0);
    working_area.loc.y += struts.top.0;

    // Round location to start at a physical pixel.
    let loc = working_area
        .loc
        .to_physical_precise_ceil(scale)
        .to_logical(scale);

    let mut size_diff = (loc - working_area.loc).to_size();
    size_diff.w = f64::min(working_area.size.w, size_diff.w);
    size_diff.h = f64::min(working_area.size.h, size_diff.h);

    working_area.size -= size_diff;
    working_area.loc = loc;

    working_area
}

fn compute_toplevel_bounds(
    border_config: niri_config::Border,
    working_area_size: Size<f64, Logical>,
    extra_size: Size<f64, Logical>,
    gaps: f64,
) -> Size<i32, Logical> {
    let mut border = 0.;
    if !border_config.off {
        border = border_config.width * 2.;
    }

    Size::from((
        f64::max(working_area_size.w - gaps * 2. - extra_size.w - border, 1.),
        f64::max(working_area_size.h - gaps * 2. - extra_size.h - border, 1.),
    ))
    .to_i32_floor()
}

fn cancel_resize_for_column<W: LayoutElement>(
    interactive_resize: &mut Option<InteractiveResize<W>>,
    column: &mut Column<W>,
) {
    if let Some(resize) = interactive_resize {
        if column.contains(&resize.window) {
            *interactive_resize = None;
        }
    }

    for tile in &mut column.tiles {
        tile.window_mut().cancel_interactive_resize();
    }
}

fn resolve_preset_size(
    preset: PresetSize,
    options: &Options,
    view_size: f64,
    extra_size: f64,
) -> ResolvedSize {
    match preset {
        PresetSize::Proportion(proportion) => ResolvedSize::Tile(
            (view_size - options.layout.gaps) * proportion - options.layout.gaps - extra_size,
        ),
        PresetSize::Fixed(width) => ResolvedSize::Window(f64::from(width)),
    }
}

#[cfg(test)]
mod tests {
    use niri_config::FloatOrInt;

    use super::*;
    use crate::utils::round_logical_in_physical;

    #[test]
    fn working_area_starts_at_physical_pixel() {
        let struts = Struts {
            left: FloatOrInt(0.5),
            right: FloatOrInt(1.),
            top: FloatOrInt(0.75),
            bottom: FloatOrInt(1.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        let area = compute_working_area(parent_area, 1., struts);

        assert_eq!(round_logical_in_physical(1., area.loc.x), area.loc.x);
        assert_eq!(round_logical_in_physical(1., area.loc.y), area.loc.y);
    }

    #[test]
    fn large_fractional_strut() {
        let struts = Struts {
            left: FloatOrInt(0.),
            right: FloatOrInt(0.),
            top: FloatOrInt(50000.5),
            bottom: FloatOrInt(0.),
        };

        let parent_area = Rectangle::from_size(Size::from((1280., 720.)));
        compute_working_area(parent_area, 1., struts);
    }
}
