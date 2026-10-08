use std::cell::{Cell, OnceCell, RefCell};

use niri_config::{
    utils::{Flag, MergeWith as _},
    workspace::WorkspaceName,
    CenterFocusedColumn, FloatOrInt, OutputName, Struts, TabIndicatorLength, TabIndicatorPosition,
    WorkspaceReference,
};
use proptest::prelude::*;
use proptest_derive::Arbitrary;
use smithay::{
    output::{Mode, PhysicalProperties, Subpixel},
    utils::Rectangle,
};

use super::*;

mod animations;
mod fullscreen;

impl<W: LayoutElement> Default for Layout<W> {
    fn default() -> Self {
        Self::with_options(Clock::with_time(Duration::ZERO), Default::default())
    }
}

#[derive(Debug)]
struct TestWindowInner {
    id: usize,
    parent_id: Cell<Option<usize>>,
    bbox: Cell<Rectangle<i32, Logical>>,
    initial_bbox: Rectangle<i32, Logical>,
    requested_size: Cell<Option<Size<i32, Logical>>>,
    // Emulates the window ignoring the compositor-provided size.
    forced_size: Cell<Option<Size<i32, Logical>>>,
    min_size: Size<i32, Logical>,
    max_size: Size<i32, Logical>,
    pending_sizing_mode: Cell<SizingMode>,
    pending_activated: Cell<bool>,
    sizing_mode: Cell<SizingMode>,
    is_windowed_fullscreen: Cell<bool>,
    is_pending_windowed_fullscreen: Cell<bool>,
    animate_next_configure: Cell<bool>,
    animation_snapshot: RefCell<Option<LayoutElementRenderSnapshot>>,
    rules: ResolvedWindowRules,
}

#[derive(Debug, Clone)]
struct TestWindow(Rc<TestWindowInner>);

#[derive(Debug, Clone, Arbitrary)]
struct TestWindowParams {
    #[proptest(strategy = "1..=5usize")]
    id: usize,
    #[proptest(strategy = "arbitrary_parent_id()")]
    parent_id: Option<usize>,
    is_floating: bool,
    #[proptest(strategy = "arbitrary_bbox()")]
    bbox: Rectangle<i32, Logical>,
    #[proptest(strategy = "arbitrary_min_max_size()")]
    min_max_size: (Size<i32, Logical>, Size<i32, Logical>),
    #[proptest(strategy = "prop::option::of(arbitrary_rules())")]
    rules: Option<ResolvedWindowRules>,
}

impl TestWindowParams {
    pub fn new(id: usize) -> Self {
        Self {
            id,
            parent_id: None,
            is_floating: false,
            bbox: Rectangle::from_size(Size::from((100, 200))),
            min_max_size: Default::default(),
            rules: None,
        }
    }
}

impl TestWindow {
    fn new(params: TestWindowParams) -> Self {
        Self(Rc::new(TestWindowInner {
            id: params.id,
            parent_id: Cell::new(params.parent_id),
            bbox: Cell::new(params.bbox),
            initial_bbox: params.bbox,
            requested_size: Cell::new(None),
            forced_size: Cell::new(None),
            min_size: params.min_max_size.0,
            max_size: params.min_max_size.1,
            pending_sizing_mode: Cell::new(SizingMode::Normal),
            pending_activated: Cell::new(false),
            sizing_mode: Cell::new(SizingMode::Normal),
            is_windowed_fullscreen: Cell::new(false),
            is_pending_windowed_fullscreen: Cell::new(false),
            animate_next_configure: Cell::new(false),
            animation_snapshot: RefCell::new(None),
            rules: params.rules.unwrap_or_default(),
        }))
    }

    fn communicate(&self) -> bool {
        let mut changed = false;

        let size = self.0.forced_size.get().or(self.0.requested_size.get());
        if let Some(size) = size {
            assert!(size.w >= 0);
            assert!(size.h >= 0);

            let mut new_bbox = self.0.initial_bbox;
            if size.w != 0 {
                new_bbox.size.w = size.w;
            }
            if size.h != 0 {
                new_bbox.size.h = size.h;
            }

            if self.0.bbox.get() != new_bbox {
                if self.0.animate_next_configure.get() {
                    self.0.animation_snapshot.replace(Some(RenderSnapshot {
                        contents: Vec::new(),
                        contents_with_blocked_out_bg: None,
                        blocked_out_contents: Vec::new(),
                        block_out_from: None,
                        size: self.0.bbox.get().size.to_f64(),
                        texture: OnceCell::new(),
                        texture_with_blocked_out_bg: Default::default(),
                        blocked_out_texture: OnceCell::new(),
                    }));
                }

                self.0.bbox.set(new_bbox);
                changed = true;
            }
        }

        self.0.animate_next_configure.set(false);

        if self.0.sizing_mode.get() != self.0.pending_sizing_mode.get() {
            self.0.sizing_mode.set(self.0.pending_sizing_mode.get());
            changed = true;
        }

        if self.0.is_windowed_fullscreen.get() != self.0.is_pending_windowed_fullscreen.get() {
            self.0
                .is_windowed_fullscreen
                .set(self.0.is_pending_windowed_fullscreen.get());
            changed = true;
        }

        changed
    }
}

impl LayoutElement for TestWindow {
    type Id = usize;

    fn id(&self) -> &Self::Id {
        &self.0.id
    }

    fn size(&self) -> Size<i32, Logical> {
        self.0.bbox.get().size
    }

    fn buf_loc(&self) -> Point<i32, Logical> {
        (0, 0).into()
    }

    fn is_in_input_region(&self, _point: Point<f64, Logical>) -> bool {
        false
    }

    fn request_size(
        &mut self,
        size: Size<i32, Logical>,
        mode: SizingMode,
        _animate: bool,
        _transaction: Option<Transaction>,
    ) {
        if self.0.requested_size.get() != Some(size) {
            self.0.requested_size.set(Some(size));
            self.0.animate_next_configure.set(true);
        }

        self.0.pending_sizing_mode.set(mode);

        if mode.is_fullscreen() {
            self.0.is_pending_windowed_fullscreen.set(false);
        }
    }

    fn min_size(&self) -> Size<i32, Logical> {
        self.0.min_size
    }

    fn max_size(&self) -> Size<i32, Logical> {
        self.0.max_size
    }

    fn is_wl_surface(&self, _wl_surface: &WlSurface) -> bool {
        false
    }

    fn set_preferred_scale_transform(&self, _scale: output::Scale, _transform: Transform) {}

    fn has_ssd(&self) -> bool {
        false
    }

    fn output_enter(&self, _output: &Output) {}

    fn output_leave(&self, _output: &Output) {}

    fn set_offscreen_data(&self, _data: Option<OffscreenData>) {}

    fn set_activated(&mut self, active: bool) {
        self.0.pending_activated.set(active);
    }

    fn set_bounds(&self, _bounds: Size<i32, Logical>) {}

    fn is_ignoring_opacity_window_rule(&self) -> bool {
        false
    }

    fn configure_intent(&self) -> ConfigureIntent {
        ConfigureIntent::CanSend
    }

    fn send_pending_configure(&mut self) {}

    fn set_active_in_column(&mut self, _active: bool) {}

    fn set_floating(&mut self, _floating: bool) {}

    fn sizing_mode(&self) -> SizingMode {
        self.0.sizing_mode.get()
    }

    fn pending_sizing_mode(&self) -> SizingMode {
        self.0.pending_sizing_mode.get()
    }

    fn requested_size(&self) -> Option<Size<i32, Logical>> {
        self.0.requested_size.get()
    }

    fn is_windowed_fullscreen(&self) -> bool {
        self.0.is_windowed_fullscreen.get()
    }

    fn is_pending_windowed_fullscreen(&self) -> bool {
        self.0.is_pending_windowed_fullscreen.get()
    }

    fn request_windowed_fullscreen(&mut self, value: bool) {
        self.0.is_pending_windowed_fullscreen.set(value);
    }

    fn is_child_of(&self, parent: &Self) -> bool {
        self.0.parent_id.get() == Some(parent.0.id)
    }

    fn refresh(&self) {}

    fn rules(&self) -> &ResolvedWindowRules {
        &self.0.rules
    }

    fn take_animation_snapshot(&mut self) -> Option<LayoutElementRenderSnapshot> {
        self.0.animation_snapshot.take()
    }

    fn set_interactive_resize(&mut self, _data: Option<InteractiveResizeData>) {}

    fn cancel_interactive_resize(&mut self) {}

    fn on_commit(&mut self, _serial: Serial) {}

    fn interactive_resize_data(&self) -> Option<InteractiveResizeData> {
        None
    }

    fn is_urgent(&self) -> bool {
        false
    }
}

fn arbitrary_size() -> impl Strategy<Value = Size<i32, Logical>> {
    any::<(u16, u16)>().prop_map(|(w, h)| Size::from((w.max(1).into(), h.max(1).into())))
}

fn arbitrary_bbox() -> impl Strategy<Value = Rectangle<i32, Logical>> {
    any::<(i16, i16, u16, u16)>().prop_map(|(x, y, w, h)| {
        let loc: Point<i32, _> = Point::from((x.into(), y.into()));
        let size: Size<i32, _> = Size::from((w.max(1).into(), h.max(1).into()));
        Rectangle::new(loc, size)
    })
}

fn arbitrary_size_change() -> impl Strategy<Value = SizeChange> {
    prop_oneof![
        (0..).prop_map(SizeChange::SetFixed),
        (0f64..).prop_map(SizeChange::SetProportion),
        any::<i32>().prop_map(SizeChange::AdjustFixed),
        any::<f64>().prop_map(SizeChange::AdjustProportion),
        // Interactive resize can have negative values here.
        Just(SizeChange::SetFixed(-100)),
    ]
}

fn arbitrary_position_change() -> impl Strategy<Value = PositionChange> {
    prop_oneof![
        (-1000f64..1000f64).prop_map(PositionChange::SetFixed),
        any::<f64>().prop_map(PositionChange::SetProportion),
        (-1000f64..1000f64).prop_map(PositionChange::AdjustFixed),
        any::<f64>().prop_map(PositionChange::AdjustProportion),
        any::<f64>().prop_map(PositionChange::SetFixed),
        any::<f64>().prop_map(PositionChange::AdjustFixed),
    ]
}

fn arbitrary_min_max() -> impl Strategy<Value = (i32, i32)> {
    prop_oneof![
        Just((0, 0)),
        (1..65536).prop_map(|n| (n, n)),
        (1..65536).prop_map(|min| (min, 0)),
        (1..).prop_map(|max| (0, max)),
        (1..65536, 1..).prop_map(|(min, max): (i32, i32)| (min, max.max(min))),
    ]
}

fn arbitrary_min_max_size() -> impl Strategy<Value = (Size<i32, Logical>, Size<i32, Logical>)> {
    prop_oneof![
        5 => (arbitrary_min_max(), arbitrary_min_max()).prop_map(
            |((min_w, max_w), (min_h, max_h))| {
                let min_size = Size::from((min_w, min_h));
                let max_size = Size::from((max_w, max_h));
                (min_size, max_size)
            },
        ),
        1 => arbitrary_min_max().prop_map(|(w, h)| {
            let size = Size::from((w, h));
            (size, size)
        }),
    ]
}

prop_compose! {
    fn arbitrary_rules()(
        focus_ring in arbitrary_focus_ring(),
        border in arbitrary_border(),
    ) -> ResolvedWindowRules {
        ResolvedWindowRules {
            focus_ring,
            border,
            ..ResolvedWindowRules::default()
        }
    }
}

fn arbitrary_view_offset_gesture_delta() -> impl Strategy<Value = f64> {
    prop_oneof![(-10f64..10f64), (-50000f64..50000f64),]
}

fn arbitrary_resize_edge() -> impl Strategy<Value = ResizeEdge> {
    prop_oneof![
        Just(ResizeEdge::RIGHT),
        Just(ResizeEdge::BOTTOM),
        Just(ResizeEdge::LEFT),
        Just(ResizeEdge::TOP),
        Just(ResizeEdge::BOTTOM_RIGHT),
        Just(ResizeEdge::BOTTOM_LEFT),
        Just(ResizeEdge::TOP_RIGHT),
        Just(ResizeEdge::TOP_LEFT),
        Just(ResizeEdge::empty()),
    ]
}

fn arbitrary_scale() -> impl Strategy<Value = f64> {
    prop_oneof![Just(1.), Just(1.5), Just(2.),]
}

fn arbitrary_msec_delta() -> impl Strategy<Value = i32> {
    prop_oneof![
        1 => Just(-1000),
        2 => Just(-10),
        1 => Just(0),
        2 => Just(10),
        6 => Just(1000),
    ]
}

fn arbitrary_parent_id() -> impl Strategy<Value = Option<usize>> {
    prop_oneof![
        5 => Just(None),
        1 => prop::option::of(1..=5usize),
    ]
}

fn arbitrary_scroll_direction() -> impl Strategy<Value = ScrollDirection> {
    prop_oneof![Just(ScrollDirection::Left), Just(ScrollDirection::Right)]
}

fn arbitrary_column_display() -> impl Strategy<Value = ColumnDisplay> {
    prop_oneof![Just(ColumnDisplay::Normal), Just(ColumnDisplay::Tabbed)]
}

#[derive(Debug, Clone, Arbitrary)]
enum Op {
    AddOutput(#[proptest(strategy = "1..=5usize")] usize),
    AddScaledOutput {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "arbitrary_scale()")]
        scale: f64,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    RemoveOutput(#[proptest(strategy = "1..=5usize")] usize),
    FocusOutput(#[proptest(strategy = "1..=5usize")] usize),
    UpdateOutputLayoutConfig {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    AddNamedWorkspace {
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
        #[proptest(strategy = "prop::option::of(1..=5usize)")]
        output_name: Option<usize>,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    UnnameWorkspace {
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
    },
    UpdateWorkspaceLayoutConfig {
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
        #[proptest(strategy = "prop::option::of(arbitrary_layout_part().prop_map(Box::new))")]
        layout_config: Option<Box<niri_config::LayoutPart>>,
    },
    AddWindow {
        params: TestWindowParams,
    },
    AddWindowNextTo {
        params: TestWindowParams,
        #[proptest(strategy = "1..=5usize")]
        next_to_id: usize,
    },
    AddWindowToNamedWorkspace {
        params: TestWindowParams,
        #[proptest(strategy = "1..=5usize")]
        ws_name: usize,
    },
    CloseWindow(#[proptest(strategy = "1..=5usize")] usize),
    FullscreenWindow(#[proptest(strategy = "1..=5usize")] usize),
    SetFullscreenWindow {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        is_fullscreen: bool,
    },
    ToggleWindowedFullscreen(#[proptest(strategy = "1..=5usize")] usize),
    FocusColumnLeft,
    FocusColumnRight,
    FocusColumnFirst,
    FocusColumnLast,
    FocusColumnRightOrFirst,
    FocusColumnLeftOrLast,
    FocusColumn(#[proptest(strategy = "1..=5usize")] usize),
    FocusWindowOrMonitorUp(#[proptest(strategy = "1..=2u8")] u8),
    FocusWindowOrMonitorDown(#[proptest(strategy = "1..=2u8")] u8),
    FocusColumnOrMonitorLeft(#[proptest(strategy = "1..=2u8")] u8),
    FocusColumnOrMonitorRight(#[proptest(strategy = "1..=2u8")] u8),
    FocusWindowDown,
    FocusWindowUp,
    FocusWindowDownOrColumnLeft,
    FocusWindowDownOrColumnRight,
    FocusWindowUpOrColumnLeft,
    FocusWindowUpOrColumnRight,
    FocusWindowOrWorkspaceDown,
    FocusWindowOrWorkspaceUp,
    FocusWindow(#[proptest(strategy = "1..=5usize")] usize),
    FocusWindowInColumn(#[proptest(strategy = "1..=5u8")] u8),
    FocusWindowTop,
    FocusWindowBottom,
    FocusWindowDownOrTop,
    FocusWindowUpOrBottom,
    MoveColumnLeft,
    MoveColumnRight,
    MoveColumnToFirst,
    MoveColumnToLast,
    MoveColumnLeftOrToMonitorLeft(#[proptest(strategy = "1..=2u8")] u8),
    MoveColumnRightOrToMonitorRight(#[proptest(strategy = "1..=2u8")] u8),
    MoveColumnToIndex(#[proptest(strategy = "1..=5usize")] usize),
    MoveWindowDown,
    MoveWindowUp,
    MoveWindowDownOrToWorkspaceDown,
    MoveWindowUpOrToWorkspaceUp,
    ConsumeOrExpelWindowLeft {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    ConsumeOrExpelWindowRight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    ConsumeWindowIntoColumn,
    ExpelWindowFromColumn,
    SwapWindowInDirection(#[proptest(strategy = "arbitrary_scroll_direction()")] ScrollDirection),
    ToggleColumnTabbedDisplay,
    SetColumnDisplay(#[proptest(strategy = "arbitrary_column_display()")] ColumnDisplay),
    CenterColumn,
    CenterWindow {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    CenterVisibleColumns,
    FocusWorkspaceDown,
    FocusWorkspaceUp,
    FocusWorkspace(#[proptest(strategy = "0..=4usize")] usize),
    FocusWorkspaceAutoBackAndForth(#[proptest(strategy = "0..=4usize")] usize),
    FocusWorkspacePrevious,
    MoveWindowToWorkspaceDown(bool),
    MoveWindowToWorkspaceUp(bool),
    MoveWindowToWorkspace {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        window_id: Option<usize>,
        #[proptest(strategy = "0..=4usize")]
        workspace_idx: usize,
    },
    MoveColumnToWorkspaceDown(bool),
    MoveColumnToWorkspaceUp(bool),
    MoveColumnToWorkspace(#[proptest(strategy = "0..=4usize")] usize, bool),
    MoveWorkspaceDown,
    MoveWorkspaceUp,
    MoveWorkspaceToIndex {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
        #[proptest(strategy = "0..=4usize")]
        target_idx: usize,
    },
    MoveWorkspaceToMonitor {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
        #[proptest(strategy = "0..=5usize")]
        output_id: usize,
    },
    SetWorkspaceName {
        #[proptest(strategy = "1..=5usize")]
        new_ws_name: usize,
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
    },
    UnsetWorkspaceName {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        ws_name: Option<usize>,
    },
    MoveWindowToOutput {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        window_id: Option<usize>,
        #[proptest(strategy = "1..=5usize")]
        output_id: usize,
        #[proptest(strategy = "proptest::option::of(0..=4usize)")]
        target_ws_idx: Option<usize>,
    },
    MoveColumnToOutput {
        #[proptest(strategy = "1..=5usize")]
        output_id: usize,
        #[proptest(strategy = "proptest::option::of(0..=4usize)")]
        target_ws_idx: Option<usize>,
        activate: bool,
    },
    SwitchPresetColumnWidth,
    SwitchPresetColumnWidthBack,
    SwitchPresetWindowWidth {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SwitchPresetWindowWidthBack {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SwitchPresetWindowHeight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SwitchPresetWindowHeightBack {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    MaximizeColumn,
    MaximizeWindowToEdges {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SetColumnWidth(#[proptest(strategy = "arbitrary_size_change()")] SizeChange),
    SetWindowWidth {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        #[proptest(strategy = "arbitrary_size_change()")]
        change: SizeChange,
    },
    SetWindowHeight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        #[proptest(strategy = "arbitrary_size_change()")]
        change: SizeChange,
    },
    ResetWindowHeight {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    ExpandColumnToAvailableWidth,
    ToggleWindowFloating {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
    },
    SetWindowFloating {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        floating: bool,
    },
    MinimizeWindow(#[proptest(strategy = "1..=5usize")] usize),
    UnminimizeWindow(#[proptest(strategy = "1..=5usize")] usize),
    ToggleShowDesktop,
    FocusFloating,
    FocusTiling,
    SwitchFocusFloatingTiling,
    MoveFloatingWindow {
        #[proptest(strategy = "proptest::option::of(1..=5usize)")]
        id: Option<usize>,
        #[proptest(strategy = "arbitrary_position_change()")]
        x: PositionChange,
        #[proptest(strategy = "arbitrary_position_change()")]
        y: PositionChange,
        animate: bool,
    },
    SetParent {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "prop::option::of(1..=5usize)")]
        new_parent_id: Option<usize>,
    },
    SetForcedSize {
        #[proptest(strategy = "1..=5usize")]
        id: usize,
        #[proptest(strategy = "proptest::option::of(arbitrary_size())")]
        size: Option<Size<i32, Logical>>,
    },
    Communicate(#[proptest(strategy = "1..=5usize")] usize),
    Refresh {
        is_active: bool,
    },
    AdvanceAnimations {
        #[proptest(strategy = "arbitrary_msec_delta()")]
        msec_delta: i32,
    },
    CompleteAnimations,
    MoveWorkspaceToOutput(#[proptest(strategy = "1..=5usize")] usize),
    ViewOffsetGestureBegin {
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "proptest::option::of(0..=4usize)")]
        workspace_idx: Option<usize>,
        is_touchpad: bool,
    },
    ViewOffsetGestureUpdate {
        #[proptest(strategy = "arbitrary_view_offset_gesture_delta()")]
        delta: f64,
        timestamp: Duration,
        is_touchpad: bool,
    },
    ViewOffsetGestureEnd {
        is_touchpad: Option<bool>,
    },
    WorkspaceSwitchGestureBegin {
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        is_touchpad: bool,
    },
    WorkspaceSwitchGestureUpdate {
        #[proptest(strategy = "-400f64..400f64")]
        delta: f64,
        timestamp: Duration,
        is_touchpad: bool,
    },
    WorkspaceSwitchGestureEnd {
        is_touchpad: Option<bool>,
    },
    OverviewGestureBegin,
    OverviewGestureUpdate {
        #[proptest(strategy = "-400f64..400f64")]
        delta: f64,
        timestamp: Duration,
    },
    OverviewGestureEnd,
    InteractiveMoveBegin {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        px: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        py: f64,
    },
    InteractiveMoveUpdate {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        dx: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        dy: f64,
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        px: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        py: f64,
    },
    InteractiveMoveEnd {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
    },
    DndUpdate {
        #[proptest(strategy = "1..=5usize")]
        output_idx: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        px: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        py: f64,
    },
    DndEnd,
    InteractiveResizeBegin {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "arbitrary_resize_edge()")]
        edges: ResizeEdge,
    },
    InteractiveResizeUpdate {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
        #[proptest(strategy = "-20000f64..20000f64")]
        dx: f64,
        #[proptest(strategy = "-20000f64..20000f64")]
        dy: f64,
    },
    InteractiveResizeEnd {
        #[proptest(strategy = "1..=5usize")]
        window: usize,
    },
    ToggleOverview,
    UpdateConfig {
        #[proptest(strategy = "arbitrary_layout_part().prop_map(Box::new)")]
        layout_config: Box<niri_config::LayoutPart>,
    },
}

impl Op {
    fn apply(self, layout: &mut Layout<TestWindow>) {
        match self {
            Op::AddOutput(id) => {
                let name = format!("output{id}");
                if layout.outputs().any(|o| o.name() == name) {
                    return;
                }

                let output = Output::new(
                    name.clone(),
                    PhysicalProperties {
                        size: Size::from((1280, 720)),
                        subpixel: Subpixel::Unknown,
                        make: String::new(),
                        model: String::new(),
                        serial_number: String::new(),
                    },
                );
                output.change_current_state(
                    Some(Mode {
                        size: Size::from((1280, 720)),
                        refresh: 60000,
                    }),
                    None,
                    None,
                    None,
                );
                output.user_data().insert_if_missing(|| OutputName {
                    connector: name,
                    make: None,
                    model: None,
                    serial: None,
                });
                layout.add_output(output.clone(), None);
            }
            Op::AddScaledOutput {
                id,
                scale,
                layout_config,
            } => {
                let name = format!("output{id}");
                if layout.outputs().any(|o| o.name() == name) {
                    return;
                }

                let output = Output::new(
                    name.clone(),
                    PhysicalProperties {
                        size: Size::from((1280, 720)),
                        subpixel: Subpixel::Unknown,
                        make: String::new(),
                        model: String::new(),
                        serial_number: String::new(),
                    },
                );
                output.change_current_state(
                    Some(Mode {
                        size: Size::from((1280, 720)),
                        refresh: 60000,
                    }),
                    None,
                    Some(smithay::output::Scale::Fractional(scale)),
                    None,
                );
                output.user_data().insert_if_missing(|| OutputName {
                    connector: name,
                    make: None,
                    model: None,
                    serial: None,
                });
                layout.add_output(output.clone(), layout_config.map(|x| *x));
            }
            Op::RemoveOutput(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.remove_output(&output);
            }
            Op::FocusOutput(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_output(&output);
            }
            Op::UpdateOutputLayoutConfig { id, layout_config } => {
                let name = format!("output{id}");
                let Some(mon) = layout.monitors_mut().find(|m| m.output_name() == &name) else {
                    return;
                };

                mon.update_layout_config(layout_config.map(|x| *x));
            }
            Op::AddNamedWorkspace {
                ws_name,
                output_name,
                layout_config,
            } => {
                layout.ensure_named_workspace(&WorkspaceConfig {
                    name: WorkspaceName(format!("ws{ws_name}")),
                    open_on_output: output_name.map(|name| format!("output{name}")),
                    layout: layout_config.map(|x| niri_config::WorkspaceLayoutPart(*x)),
                });
            }
            Op::UnnameWorkspace { ws_name } => {
                layout.unname_workspace(&format!("ws{ws_name}"));
            }
            Op::UpdateWorkspaceLayoutConfig {
                ws_name,
                layout_config,
            } => {
                let ws_name = format!("ws{ws_name}");
                let Some(ws) = layout
                    .workspaces_mut()
                    .find(|ws| ws.name() == Some(&ws_name))
                else {
                    return;
                };

                ws.update_layout_config(layout_config.map(|x| *x));
            }
            Op::SetWorkspaceName {
                new_ws_name,
                ws_name,
            } => {
                let ws_ref =
                    ws_name.map(|ws_name| WorkspaceReference::Name(format!("ws{ws_name}")));
                layout.set_workspace_name(format!("ws{new_ws_name}"), ws_ref);
            }
            Op::UnsetWorkspaceName { ws_name } => {
                let ws_ref =
                    ws_name.map(|ws_name| WorkspaceReference::Name(format!("ws{ws_name}")));
                layout.unset_workspace_name(ws_ref);
            }
            Op::AddWindow { mut params } => {
                if layout.has_window(&params.id) {
                    return;
                }
                if let Some(parent_id) = params.parent_id {
                    if parent_id_causes_loop(layout, params.id, parent_id) {
                        params.parent_id = None;
                    }
                }

                let is_floating = params.is_floating;
                let win = TestWindow::new(params);
                layout.add_window(
                    win,
                    AddWindowTarget::Auto,
                    None,
                    None,
                    false,
                    is_floating,
                    ActivateWindow::default(),
                );
            }
            Op::AddWindowNextTo {
                mut params,
                next_to_id,
            } => {
                let mut found_next_to = false;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    let win_id = move_.tile.window().0.id;
                    if win_id == params.id {
                        return;
                    }
                    if win_id == next_to_id {
                        found_next_to = true;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == params.id {
                                        return;
                                    }

                                    if win.0.id == next_to_id {
                                        found_next_to = true;
                                    }
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == params.id {
                                    return;
                                }

                                if win.0.id == next_to_id {
                                    found_next_to = true;
                                }
                            }
                        }
                    }
                }

                if !found_next_to {
                    return;
                }

                if let Some(parent_id) = params.parent_id {
                    if parent_id_causes_loop(layout, params.id, parent_id) {
                        params.parent_id = None;
                    }
                }

                let is_floating = params.is_floating;
                let win = TestWindow::new(params);
                layout.add_window(
                    win,
                    AddWindowTarget::NextTo(&next_to_id),
                    None,
                    None,
                    false,
                    is_floating,
                    ActivateWindow::default(),
                );
            }
            Op::AddWindowToNamedWorkspace {
                mut params,
                ws_name,
            } => {
                let ws_name = format!("ws{ws_name}");
                let mut ws_id = None;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    if move_.tile.window().0.id == params.id {
                        return;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == params.id {
                                        return;
                                    }
                                }

                                if ws
                                    .name
                                    .as_ref()
                                    .is_some_and(|name| name.eq_ignore_ascii_case(&ws_name))
                                {
                                    ws_id = Some(ws.id());
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == params.id {
                                    return;
                                }
                            }

                            if ws
                                .name
                                .as_ref()
                                .is_some_and(|name| name.eq_ignore_ascii_case(&ws_name))
                            {
                                ws_id = Some(ws.id());
                            }
                        }
                    }
                }

                let Some(ws_id) = ws_id else {
                    return;
                };

                if let Some(parent_id) = params.parent_id {
                    if parent_id_causes_loop(layout, params.id, parent_id) {
                        params.parent_id = None;
                    }
                }

                let is_floating = params.is_floating;
                let win = TestWindow::new(params);
                layout.add_window(
                    win,
                    AddWindowTarget::Workspace(ws_id),
                    None,
                    None,
                    false,
                    is_floating,
                    ActivateWindow::default(),
                );
            }
            Op::CloseWindow(id) => {
                layout.remove_window(&id, Transaction::new());
            }
            Op::FullscreenWindow(id) => {
                if !layout.has_window(&id) {
                    return;
                }
                layout.toggle_fullscreen(&id);
            }
            Op::SetFullscreenWindow {
                window,
                is_fullscreen,
            } => {
                if !layout.has_window(&window) {
                    return;
                }
                layout.set_fullscreen(&window, is_fullscreen);
            }
            Op::ToggleWindowedFullscreen(id) => {
                if !layout.has_window(&id) {
                    return;
                }
                layout.toggle_windowed_fullscreen(&id);
            }
            Op::FocusColumnLeft => layout.focus_left(),
            Op::FocusColumnRight => layout.focus_right(),
            Op::FocusColumnFirst => layout.focus_column_first(),
            Op::FocusColumnLast => layout.focus_column_last(),
            Op::FocusColumnRightOrFirst => layout.focus_column_right_or_first(),
            Op::FocusColumnLeftOrLast => layout.focus_column_left_or_last(),
            Op::FocusColumn(index) => layout.focus_column(index),
            Op::FocusWindowOrMonitorUp(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_window_up_or_output(&output);
            }
            Op::FocusWindowOrMonitorDown(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_window_down_or_output(&output);
            }
            Op::FocusColumnOrMonitorLeft(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_column_left_or_output(&output);
            }
            Op::FocusColumnOrMonitorRight(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.focus_column_right_or_output(&output);
            }
            Op::FocusWindowDown => layout.focus_down(),
            Op::FocusWindowUp => layout.focus_up(),
            Op::FocusWindowDownOrColumnLeft => layout.focus_down_or_left(),
            Op::FocusWindowDownOrColumnRight => layout.focus_down_or_right(),
            Op::FocusWindowUpOrColumnLeft => layout.focus_up_or_left(),
            Op::FocusWindowUpOrColumnRight => layout.focus_up_or_right(),
            Op::FocusWindowOrWorkspaceDown => layout.focus_window_or_workspace_down(),
            Op::FocusWindowOrWorkspaceUp => layout.focus_window_or_workspace_up(),
            Op::FocusWindow(id) => layout.activate_window(&id),
            Op::FocusWindowInColumn(index) => layout.focus_window_in_column(index),
            Op::FocusWindowTop => layout.focus_window_top(),
            Op::FocusWindowBottom => layout.focus_window_bottom(),
            Op::FocusWindowDownOrTop => layout.focus_window_down_or_top(),
            Op::FocusWindowUpOrBottom => layout.focus_window_up_or_bottom(),
            Op::MoveColumnLeft => layout.move_left(),
            Op::MoveColumnRight => layout.move_right(),
            Op::MoveColumnToFirst => layout.move_column_to_first(),
            Op::MoveColumnToLast => layout.move_column_to_last(),
            Op::MoveColumnLeftOrToMonitorLeft(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_column_left_or_to_output(&output);
            }
            Op::MoveColumnRightOrToMonitorRight(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_column_right_or_to_output(&output);
            }
            Op::MoveColumnToIndex(index) => layout.move_column_to_index(index),
            Op::MoveWindowDown => layout.move_down(),
            Op::MoveWindowUp => layout.move_up(),
            Op::MoveWindowDownOrToWorkspaceDown => layout.move_down_or_to_workspace_down(),
            Op::MoveWindowUpOrToWorkspaceUp => layout.move_up_or_to_workspace_up(),
            Op::ConsumeOrExpelWindowLeft { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.consume_or_expel_window_left(id.as_ref());
            }
            Op::ConsumeOrExpelWindowRight { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.consume_or_expel_window_right(id.as_ref());
            }
            Op::ConsumeWindowIntoColumn => layout.consume_into_column(),
            Op::ExpelWindowFromColumn => layout.expel_from_column(),
            Op::SwapWindowInDirection(direction) => layout.swap_window_in_direction(direction),
            Op::ToggleColumnTabbedDisplay => layout.toggle_column_tabbed_display(),
            Op::SetColumnDisplay(display) => layout.set_column_display(display),
            Op::CenterColumn => layout.center_column(),
            Op::CenterWindow { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.center_window(id.as_ref());
            }
            Op::CenterVisibleColumns => layout.center_visible_columns(),
            Op::FocusWorkspaceDown => layout.switch_workspace_down(),
            Op::FocusWorkspaceUp => layout.switch_workspace_up(),
            Op::FocusWorkspace(idx) => layout.switch_workspace(idx),
            Op::FocusWorkspaceAutoBackAndForth(idx) => {
                layout.switch_workspace_auto_back_and_forth(idx)
            }
            Op::FocusWorkspacePrevious => layout.switch_workspace_previous(),
            Op::MoveWindowToWorkspaceDown(focus) => layout.move_to_workspace_down(focus),
            Op::MoveWindowToWorkspaceUp(focus) => layout.move_to_workspace_up(focus),
            Op::MoveWindowToWorkspace {
                window_id,
                workspace_idx,
            } => {
                let window_id = window_id.filter(|id| layout.has_window(id));
                layout.move_to_workspace(window_id.as_ref(), workspace_idx, ActivateWindow::Smart);
            }
            Op::MoveColumnToWorkspaceDown(focus) => layout.move_column_to_workspace_down(focus),
            Op::MoveColumnToWorkspaceUp(focus) => layout.move_column_to_workspace_up(focus),
            Op::MoveColumnToWorkspace(idx, focus) => layout.move_column_to_workspace(idx, focus),
            Op::MoveWindowToOutput {
                window_id,
                output_id: id,
                target_ws_idx,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                let mon = layout.monitor_for_output(&output).unwrap();

                let window_id = window_id.filter(|id| layout.has_window(id));
                let target_ws_idx = target_ws_idx.filter(|idx| mon.workspaces.len() > *idx);
                layout.move_to_output(
                    window_id.as_ref(),
                    &output,
                    target_ws_idx,
                    ActivateWindow::Smart,
                );
            }
            Op::MoveColumnToOutput {
                output_id: id,
                target_ws_idx,
                activate,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_column_to_output(&output, target_ws_idx, activate);
            }
            Op::MoveWorkspaceDown => layout.move_workspace_down(),
            Op::MoveWorkspaceUp => layout.move_workspace_up(),
            Op::MoveWorkspaceToIndex {
                ws_name: Some(ws_name),
                target_idx,
            } => {
                let MonitorSet::Normal { monitors, .. } = &mut layout.monitor_set else {
                    return;
                };

                let Some((old_idx, old_output)) = monitors.iter().find_map(|monitor| {
                    monitor
                        .workspaces
                        .iter()
                        .enumerate()
                        .find_map(|(i, ws)| {
                            if ws.name == Some(format!("ws{ws_name}")) {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .map(|i| (i, monitor.output.clone()))
                }) else {
                    return;
                };

                layout.move_workspace_to_idx(Some((Some(old_output), old_idx)), target_idx)
            }
            Op::MoveWorkspaceToIndex {
                ws_name: None,
                target_idx,
            } => layout.move_workspace_to_idx(None, target_idx),
            Op::MoveWorkspaceToMonitor {
                ws_name: None,
                output_id: id,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.move_workspace_to_output(&output);
            }
            Op::MoveWorkspaceToMonitor {
                ws_name: Some(ws_name),
                output_id: id,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                let MonitorSet::Normal { monitors, .. } = &mut layout.monitor_set else {
                    return;
                };

                let Some((old_idx, old_output)) = monitors.iter().find_map(|monitor| {
                    monitor
                        .workspaces
                        .iter()
                        .enumerate()
                        .find_map(|(i, ws)| {
                            if ws.name == Some(format!("ws{ws_name}")) {
                                Some(i)
                            } else {
                                None
                            }
                        })
                        .map(|i| (i, monitor.output.clone()))
                }) else {
                    return;
                };

                layout.move_workspace_to_output_by_id(old_idx, Some(old_output), &output);
            }
            Op::SwitchPresetColumnWidth => layout.toggle_width(true),
            Op::SwitchPresetColumnWidthBack => layout.toggle_width(false),
            Op::SwitchPresetWindowWidth { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_width(id.as_ref(), true);
            }
            Op::SwitchPresetWindowWidthBack { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_width(id.as_ref(), false);
            }
            Op::SwitchPresetWindowHeight { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_height(id.as_ref(), true);
            }
            Op::SwitchPresetWindowHeightBack { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_height(id.as_ref(), false);
            }
            Op::MaximizeColumn => layout.toggle_full_width(),
            Op::MaximizeWindowToEdges { id } => {
                let id = id.or_else(|| layout.focus().map(|win| *win.id()));
                let Some(id) = id else {
                    return;
                };
                if !layout.has_window(&id) {
                    return;
                }
                layout.toggle_maximized(&id);
            }
            Op::SetColumnWidth(change) => layout.set_column_width(change),
            Op::SetWindowWidth { id, change } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.set_window_width(id.as_ref(), change);
            }
            Op::SetWindowHeight { id, change } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.set_window_height(id.as_ref(), change);
            }
            Op::ResetWindowHeight { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.reset_window_height(id.as_ref());
            }
            Op::ExpandColumnToAvailableWidth => layout.expand_column_to_available_width(),
            Op::ToggleWindowFloating { id } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.toggle_window_floating(id.as_ref());
            }
            Op::SetWindowFloating { id, floating } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.set_window_floating(id.as_ref(), floating);
            }
            Op::MinimizeWindow(id) => {
                layout.minimize_window(&id);
            }
            Op::UnminimizeWindow(id) => {
                layout.unminimize_window(&id);
            }
            Op::ToggleShowDesktop => layout.toggle_show_desktop(),
            Op::FocusFloating => {
                layout.focus_floating();
            }
            Op::FocusTiling => {
                layout.focus_tiling();
            }
            Op::SwitchFocusFloatingTiling => {
                layout.switch_focus_floating_tiling();
            }
            Op::MoveFloatingWindow { id, x, y, animate } => {
                let id = id.filter(|id| layout.has_window(id));
                layout.move_floating_window(id.as_ref(), x, y, animate);
            }
            Op::SetParent {
                id,
                mut new_parent_id,
            } => {
                if !layout.has_window(&id) {
                    return;
                }

                if let Some(parent_id) = new_parent_id {
                    if parent_id_causes_loop(layout, id, parent_id) {
                        new_parent_id = None;
                    }
                }

                let mut update = false;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    if move_.tile.window().0.id == id {
                        move_.tile.window().0.parent_id.set(new_parent_id);
                        update = true;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        'outer: for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == id {
                                        win.0.parent_id.set(new_parent_id);
                                        update = true;
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        'outer: for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == id {
                                    win.0.parent_id.set(new_parent_id);
                                    update = true;
                                    break 'outer;
                                }
                            }
                        }
                    }
                }

                if update {
                    if let Some(new_parent_id) = new_parent_id {
                        layout.descendants_added(&new_parent_id);
                    }
                }
            }
            Op::SetForcedSize { id, size } => {
                for (_mon, win) in layout.windows() {
                    if win.0.id == id {
                        win.0.forced_size.set(size);
                        return;
                    }
                }
            }
            Op::Communicate(id) => {
                let mut update = false;

                if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
                    if move_.tile.window().0.id == id {
                        if move_.tile.window().communicate() {
                            update = true;
                        }

                        if update {
                            // FIXME: serial.
                            layout.update_window(&id, None);
                        }
                        return;
                    }
                }

                match &mut layout.monitor_set {
                    MonitorSet::Normal { monitors, .. } => {
                        'outer: for mon in monitors {
                            for ws in &mut mon.workspaces {
                                for win in ws.windows() {
                                    if win.0.id == id {
                                        if win.communicate() {
                                            update = true;
                                        }
                                        break 'outer;
                                    }
                                }
                            }
                        }
                    }
                    MonitorSet::NoOutputs { workspaces, .. } => {
                        'outer: for ws in workspaces {
                            for win in ws.windows() {
                                if win.0.id == id {
                                    if win.communicate() {
                                        update = true;
                                    }
                                    break 'outer;
                                }
                            }
                        }
                    }
                }

                if update {
                    // FIXME: serial.
                    layout.update_window(&id, None);
                }
            }
            Op::Refresh { is_active } => {
                layout.refresh(is_active);
            }
            Op::AdvanceAnimations { msec_delta } => {
                let mut now = layout.clock.now_unadjusted();
                if msec_delta >= 0 {
                    now = now.saturating_add(Duration::from_millis(msec_delta as u64));
                } else {
                    now = now.saturating_sub(Duration::from_millis(-msec_delta as u64));
                }
                layout.clock.set_unadjusted(now);
                layout.advance_animations();
            }
            Op::CompleteAnimations => {
                layout.clock.set_complete_instantly(true);
                layout.advance_animations();
                layout.clock.set_complete_instantly(false);
            }
            Op::MoveWorkspaceToOutput(id) => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.move_workspace_to_output(&output);
            }
            Op::ViewOffsetGestureBegin {
                output_idx: id,
                workspace_idx,
                is_touchpad: normalize,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.view_offset_gesture_begin(&output, workspace_idx, normalize);
            }
            Op::ViewOffsetGestureUpdate {
                delta,
                timestamp,
                is_touchpad,
            } => {
                layout.view_offset_gesture_update(delta, timestamp, is_touchpad);
            }
            Op::ViewOffsetGestureEnd { is_touchpad } => {
                layout.view_offset_gesture_end(is_touchpad);
            }
            Op::WorkspaceSwitchGestureBegin {
                output_idx: id,
                is_touchpad,
            } => {
                let name = format!("output{id}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };

                layout.workspace_switch_gesture_begin(&output, is_touchpad);
            }
            Op::WorkspaceSwitchGestureUpdate {
                delta,
                timestamp,
                is_touchpad,
            } => {
                layout.workspace_switch_gesture_update(delta, timestamp, is_touchpad);
            }
            Op::WorkspaceSwitchGestureEnd { is_touchpad } => {
                layout.workspace_switch_gesture_end(is_touchpad);
            }
            Op::OverviewGestureBegin => {
                layout.overview_gesture_begin();
            }
            Op::OverviewGestureUpdate { delta, timestamp } => {
                layout.overview_gesture_update(delta, timestamp);
            }
            Op::OverviewGestureEnd => {
                layout.overview_gesture_end();
            }
            Op::InteractiveMoveBegin {
                window,
                output_idx,
                px,
                py,
            } => {
                let name = format!("output{output_idx}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.interactive_move_begin(window, &output, Point::from((px, py)));
            }
            Op::InteractiveMoveUpdate {
                window,
                dx,
                dy,
                output_idx,
                px,
                py,
            } => {
                let name = format!("output{output_idx}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.interactive_move_update(
                    &window,
                    Point::from((dx, dy)),
                    output,
                    Point::from((px, py)),
                );
            }
            Op::InteractiveMoveEnd { window } => {
                layout.interactive_move_end(&window);
            }
            Op::DndUpdate { output_idx, px, py } => {
                let name = format!("output{output_idx}");
                let Some(output) = layout.outputs().find(|o| o.name() == name).cloned() else {
                    return;
                };
                layout.dnd_update(output, Point::from((px, py)));
            }
            Op::DndEnd => {
                layout.dnd_end();
            }
            Op::InteractiveResizeBegin { window, edges } => {
                layout.interactive_resize_begin(window, edges);
            }
            Op::InteractiveResizeUpdate { window, dx, dy } => {
                layout.interactive_resize_update(&window, Point::from((dx, dy)));
            }
            Op::InteractiveResizeEnd { window } => {
                layout.interactive_resize_end(&window);
            }
            Op::ToggleOverview => {
                layout.toggle_overview();
            }
            Op::UpdateConfig { layout_config } => {
                let options = Options {
                    layout: niri_config::Layout::from_part(&layout_config),
                    ..Default::default()
                };

                layout.update_options(options);
            }
        }
    }
}

#[track_caller]
fn check_ops_on_layout(layout: &mut Layout<TestWindow>, ops: impl IntoIterator<Item = Op>) {
    for op in ops {
        op.apply(layout);
        layout.verify_invariants();
    }
}

#[track_caller]
fn check_ops(ops: impl IntoIterator<Item = Op>) -> Layout<TestWindow> {
    let mut layout = Layout::default();
    check_ops_on_layout(&mut layout, ops);
    layout
}

#[track_caller]
fn check_ops_with_options(
    options: Options,
    ops: impl IntoIterator<Item = Op>,
) -> Layout<TestWindow> {
    let mut layout = Layout::with_options(Clock::with_time(Duration::ZERO), options);
    check_ops_on_layout(&mut layout, ops);
    layout
}

#[test]
fn operations_dont_panic() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        eprintln!("ignoring slow test");
        return;
    }

    let every_op = [
        Op::AddOutput(0),
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::RemoveOutput(0),
        Op::RemoveOutput(1),
        Op::RemoveOutput(2),
        Op::FocusOutput(0),
        Op::FocusOutput(1),
        Op::FocusOutput(2),
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(1),
            layout_config: None,
        },
        Op::UnnameWorkspace { ws_name: 1 },
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(2),
            next_to_id: 1,
        },
        Op::AddWindowToNamedWorkspace {
            params: TestWindowParams::new(3),
            ws_name: 1,
        },
        Op::CloseWindow(0),
        Op::CloseWindow(1),
        Op::CloseWindow(2),
        Op::FullscreenWindow(1),
        Op::FullscreenWindow(2),
        Op::FullscreenWindow(3),
        Op::MaximizeWindowToEdges { id: Some(1) },
        Op::MaximizeWindowToEdges { id: Some(2) },
        Op::MaximizeWindowToEdges { id: Some(3) },
        Op::FocusColumnLeft,
        Op::FocusColumnRight,
        Op::FocusColumnRightOrFirst,
        Op::FocusColumnLeftOrLast,
        Op::FocusWindowOrMonitorUp(0),
        Op::FocusWindowOrMonitorDown(1),
        Op::FocusColumnOrMonitorLeft(0),
        Op::FocusColumnOrMonitorRight(1),
        Op::FocusWindowUp,
        Op::FocusWindowUpOrColumnLeft,
        Op::FocusWindowUpOrColumnRight,
        Op::FocusWindowOrWorkspaceUp,
        Op::FocusWindowDown,
        Op::FocusWindowDownOrColumnLeft,
        Op::FocusWindowDownOrColumnRight,
        Op::FocusWindowOrWorkspaceDown,
        Op::MoveColumnLeft,
        Op::MoveColumnRight,
        Op::MoveColumnLeftOrToMonitorLeft(0),
        Op::MoveColumnRightOrToMonitorRight(1),
        Op::ConsumeWindowIntoColumn,
        Op::ExpelWindowFromColumn,
        Op::CenterColumn,
        Op::FocusWorkspaceDown,
        Op::FocusWorkspaceUp,
        Op::FocusWorkspace(1),
        Op::FocusWorkspace(2),
        Op::MoveWindowToWorkspaceDown(true),
        Op::MoveWindowToWorkspaceUp(true),
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 1,
        },
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 2,
        },
        Op::MoveColumnToWorkspaceDown(true),
        Op::MoveColumnToWorkspaceUp(true),
        Op::MoveColumnToWorkspace(1, true),
        Op::MoveColumnToWorkspace(2, true),
        Op::MoveWindowDown,
        Op::MoveWindowDownOrToWorkspaceDown,
        Op::MoveWindowUp,
        Op::MoveWindowUpOrToWorkspaceUp,
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::ConsumeOrExpelWindowRight { id: None },
        Op::MoveWorkspaceToOutput(1),
        Op::ToggleColumnTabbedDisplay,
    ];

    for third in &every_op {
        for second in &every_op {
            for first in &every_op {
                // eprintln!("{first:?}, {second:?}, {third:?}");

                let mut layout = Layout::default();
                first.clone().apply(&mut layout);
                layout.verify_invariants();
                second.clone().apply(&mut layout);
                layout.verify_invariants();
                third.clone().apply(&mut layout);
                layout.verify_invariants();
            }
        }
    }
}

#[test]
fn operations_from_starting_state_dont_panic() {
    if std::env::var_os("RUN_SLOW_TESTS").is_none() {
        eprintln!("ignoring slow test");
        return;
    }

    // Running every op from an empty state doesn't get us to all the interesting states. So,
    // also run it from a manually-created starting state with more things going on to exercise
    // more code paths.
    let setup_ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MoveWindowToWorkspaceDown(true),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusColumnLeft,
        Op::ConsumeWindowIntoColumn,
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(5),
        },
        Op::MoveWindowToOutput {
            window_id: None,
            output_id: 2,
            target_ws_idx: None,
        },
        Op::FocusOutput(1),
        Op::Communicate(1),
        Op::Communicate(2),
        Op::Communicate(3),
        Op::Communicate(4),
        Op::Communicate(5),
    ];

    let every_op = [
        Op::AddOutput(0),
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::RemoveOutput(0),
        Op::RemoveOutput(1),
        Op::RemoveOutput(2),
        Op::FocusOutput(0),
        Op::FocusOutput(1),
        Op::FocusOutput(2),
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(1),
            layout_config: None,
        },
        Op::UnnameWorkspace { ws_name: 1 },
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(6),
            next_to_id: 0,
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(7),
            next_to_id: 1,
        },
        Op::AddWindowToNamedWorkspace {
            params: TestWindowParams::new(5),
            ws_name: 1,
        },
        Op::CloseWindow(0),
        Op::CloseWindow(1),
        Op::CloseWindow(2),
        Op::FullscreenWindow(1),
        Op::FullscreenWindow(2),
        Op::FullscreenWindow(3),
        Op::MaximizeWindowToEdges { id: Some(1) },
        Op::MaximizeWindowToEdges { id: Some(2) },
        Op::MaximizeWindowToEdges { id: Some(3) },
        Op::SetFullscreenWindow {
            window: 1,
            is_fullscreen: false,
        },
        Op::SetFullscreenWindow {
            window: 1,
            is_fullscreen: true,
        },
        Op::SetFullscreenWindow {
            window: 2,
            is_fullscreen: false,
        },
        Op::SetFullscreenWindow {
            window: 2,
            is_fullscreen: true,
        },
        Op::FocusColumnLeft,
        Op::FocusColumnRight,
        Op::FocusColumnRightOrFirst,
        Op::FocusColumnLeftOrLast,
        Op::FocusWindowOrMonitorUp(0),
        Op::FocusWindowOrMonitorDown(1),
        Op::FocusColumnOrMonitorLeft(0),
        Op::FocusColumnOrMonitorRight(1),
        Op::FocusWindowUp,
        Op::FocusWindowUpOrColumnLeft,
        Op::FocusWindowUpOrColumnRight,
        Op::FocusWindowOrWorkspaceUp,
        Op::FocusWindowDown,
        Op::FocusWindowDownOrColumnLeft,
        Op::FocusWindowDownOrColumnRight,
        Op::FocusWindowOrWorkspaceDown,
        Op::MoveColumnLeft,
        Op::MoveColumnRight,
        Op::MoveColumnLeftOrToMonitorLeft(0),
        Op::MoveColumnRightOrToMonitorRight(1),
        Op::ConsumeWindowIntoColumn,
        Op::ExpelWindowFromColumn,
        Op::CenterColumn,
        Op::FocusWorkspaceDown,
        Op::FocusWorkspaceUp,
        Op::FocusWorkspace(1),
        Op::FocusWorkspace(2),
        Op::FocusWorkspace(3),
        Op::MoveWindowToWorkspaceDown(true),
        Op::MoveWindowToWorkspaceUp(true),
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 1,
        },
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 2,
        },
        Op::MoveWindowToWorkspace {
            window_id: None,
            workspace_idx: 3,
        },
        Op::MoveColumnToWorkspaceDown(true),
        Op::MoveColumnToWorkspaceUp(true),
        Op::MoveColumnToWorkspace(1, true),
        Op::MoveColumnToWorkspace(2, true),
        Op::MoveColumnToWorkspace(3, true),
        Op::MoveWindowDown,
        Op::MoveWindowDownOrToWorkspaceDown,
        Op::MoveWindowUp,
        Op::MoveWindowUpOrToWorkspaceUp,
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::ConsumeOrExpelWindowRight { id: None },
        Op::ToggleColumnTabbedDisplay,
    ];

    for third in &every_op {
        for second in &every_op {
            for first in &every_op {
                // eprintln!("{first:?}, {second:?}, {third:?}");

                let mut layout = Layout::default();
                for op in &setup_ops {
                    op.clone().apply(&mut layout);
                }

                let mut layout = Layout::default();
                first.clone().apply(&mut layout);
                layout.verify_invariants();
                second.clone().apply(&mut layout);
                layout.verify_invariants();
                third.clone().apply(&mut layout);
                layout.verify_invariants();
            }
        }
    }
}

#[test]
fn primary_active_workspace_idx_not_updated_on_output_add() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(2),
        Op::FocusWorkspace(3),
        Op::AddOutput(2),
    ];

    check_ops(ops);
}

#[test]
fn window_closed_on_previous_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::CloseWindow(0),
    ];

    check_ops(ops);
}

#[test]
fn removing_output_must_keep_empty_focus_on_primary() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    // The workspace from the removed output was inserted at position 0, so the active workspace
    // must change to 1 to keep the focus on the empty workspace.
    assert_eq!(monitors[0].active_workspace_idx, 1);
}

#[test]
fn move_to_workspace_by_idx_does_not_leave_empty_workspaces() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(1),
        Op::MoveWindowToWorkspace {
            window_id: Some(0),
            workspace_idx: 2,
        },
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert!(monitors[0].workspaces[1].has_windows());
}

#[test]
fn empty_workspaces_dont_move_back_to_original_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspace(1),
        Op::CloseWindow(1),
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn named_workspaces_dont_update_original_output_on_adding_window() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        // Adding a window updates the original output for unnamed workspaces.
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        // Connecting the previous output should move the named workspace back since its
        // original output wasn't updated.
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output1");
}

#[test]
fn workspaces_update_original_output_on_moving_to_same_output() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceToOutput(2),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output2");
}

#[test]
fn workspaces_update_original_output_on_moving_to_same_monitor() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 1,
            ws_name: None,
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceToMonitor {
            ws_name: Some(1),
            output_id: 2,
        },
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);
    let (mon, _, ws) = layout
        .workspaces()
        .find(|(_, _, ws)| ws.name().is_some())
        .unwrap();
    assert!(ws.name().is_some()); // Sanity check.
    let mon = mon.unwrap();
    assert_eq!(mon.output_name(), "output2");
}

#[test]
fn large_negative_height_change() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::AdjustProportion(-1e129),
        },
    ];

    let mut options = Options::default();
    options.layout.border.off = false;
    options.layout.border.width = 1.;

    check_ops_with_options(options, ops);
}

#[test]
fn large_max_size() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                min_max_size: (Size::from((0, 0)), Size::from((i32::MAX, i32::MAX))),
                ..TestWindowParams::new(1)
            },
        },
    ];

    let mut options = Options::default();
    options.layout.border.off = false;
    options.layout.border.width = 1.;

    check_ops_with_options(options, ops);
}

#[test]
fn workspace_cleanup_during_switch() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::CloseWindow(1),
    ];

    check_ops(ops);
}

#[test]
fn workspace_transfer_during_switch() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::FocusWorkspaceDown,
        Op::FocusWorkspaceDown,
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn workspace_transfer_during_switch_from_last() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(2),
        Op::RemoveOutput(1),
        Op::FocusWorkspaceUp,
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn workspace_transfer_during_switch_gets_cleaned_up() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(2),
        Op::MoveColumnToWorkspaceDown(true),
        Op::MoveColumnToWorkspaceDown(true),
        Op::AddOutput(1),
    ];

    check_ops(ops);
}

#[test]
fn move_workspace_to_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::FocusOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::MoveWorkspaceToOutput(2),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal {
        monitors,
        active_monitor_idx,
        ..
    } = layout.monitor_set
    else {
        unreachable!()
    };

    assert_eq!(active_monitor_idx, 1);
    assert_eq!(monitors[0].workspaces.len(), 1);
    assert!(!monitors[0].workspaces[0].has_windows());
    assert_eq!(monitors[1].active_workspace_idx, 0);
    assert_eq!(monitors[1].workspaces.len(), 2);
    assert!(monitors[1].workspaces[0].has_windows());
}

#[test]
fn open_right_of_on_different_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 1,
        },
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let mon = monitors.into_iter().next().unwrap();
    assert_eq!(
        mon.active_workspace_idx, 1,
        "the second workspace must remain active"
    );
    assert_eq!(
        mon.workspaces[0].scrolling().active_column_idx(),
        1,
        "the new window must become active"
    );
}

#[test]
// empty_workspace_above_first = true
fn open_right_of_on_different_workspace_ewaf() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 1,
        },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let layout = check_ops_with_options(options, ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    let mon = monitors.into_iter().next().unwrap();
    assert_eq!(
        mon.active_workspace_idx, 2,
        "the second workspace must remain active"
    );
    assert_eq!(
        mon.workspaces[1].scrolling().active_column_idx(),
        1,
        "the new window must become active"
    );
}

#[test]
fn removing_all_outputs_preserves_empty_named_workspaces() {
    let ops = [
        Op::AddOutput(1),
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: None,
            layout_config: None,
        },
        Op::AddNamedWorkspace {
            ws_name: 2,
            output_name: None,
            layout_config: None,
        },
        Op::RemoveOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::NoOutputs { workspaces } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(workspaces.len(), 2);
}

#[test]
fn config_change_updates_cached_sizes() {
    let mut config = Config::default();
    let border = &mut config.layout.border;
    border.off = false;
    border.width = 2.;

    let mut layout = Layout::new(Clock::default(), &config);

    Op::AddWindow {
        params: TestWindowParams {
            bbox: Rectangle::from_size(Size::from((1280, 200))),
            ..TestWindowParams::new(1)
        },
    }
    .apply(&mut layout);

    config.layout.border.width = 4.;
    layout.update_config(&config);

    layout.verify_invariants();
}

#[test]
fn preset_height_change_removes_preset() {
    let mut config = Config::default();
    config.layout.preset_window_heights = vec![PresetSize::Fixed(1), PresetSize::Fixed(2)];

    let mut layout = Layout::new(Clock::default(), &config);

    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SwitchPresetWindowHeight { id: None },
        Op::SwitchPresetWindowHeight { id: None },
    ];
    for op in ops {
        op.apply(&mut layout);
    }

    // Leave only one.
    config.layout.preset_window_heights = vec![PresetSize::Fixed(1)];

    layout.update_config(&config);

    layout.verify_invariants();
}

#[test]
fn set_window_height_recomputes_to_auto() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        Op::FocusWindowUp,
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
    ];

    check_ops(ops);
}

#[test]
fn one_window_in_column_becomes_weight_1() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(100),
        },
        Op::Communicate(2),
        Op::FocusWindowUp,
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(200),
        },
        Op::Communicate(1),
        Op::CloseWindow(0),
        Op::CloseWindow(1),
    ];

    check_ops(ops);
}

#[test]
fn fixed_height_takes_max_non_auto_into_account() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SetWindowHeight {
            id: Some(0),
            change: SizeChange::SetFixed(704),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    let options = Options {
        layout: niri_config::Layout {
            border: niri_config::Border {
                off: false,
                width: 4.,
                ..Default::default()
            },
            gaps: 0.,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn start_interactive_move_then_remove_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::CloseWindow(0),
    ];

    check_ops(ops);
}

/// Layout side of `MoveGrab::click_to_drop` (title bar and Super+M1).
fn apply_click_to_drop_pickup(layout: &mut Layout<TestWindow>, window: usize) {
    layout.interactive_move_click_to_drop(&window);
    layout.verify_invariants();
}

fn window_is_floating(layout: &Layout<TestWindow>, id: usize) -> bool {
    if let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move {
        if *move_.tile.window().id() == id {
            return move_.is_floating;
        }
    }
    layout.workspaces().any(|(_, _, ws)| ws.is_floating(&id))
}

#[test]
fn click_to_drop_click_without_drag_stays_tiled() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
    ]);
    apply_click_to_drop_pickup(&mut layout, 0);
    layout.interactive_move_end(&0);
    layout.verify_invariants();
    assert!(
        !window_is_floating(&layout, 0),
        "click-to-drop without a held drag must stay tiled"
    );
}

#[test]
fn click_to_drop_held_drag_release_floats() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
    ]);
    apply_click_to_drop_pickup(&mut layout, 0);
    check_ops_on_layout(
        &mut layout,
        [Op::InteractiveMoveUpdate {
            window: 0,
            dx: 20.,
            dy: 0.,
            output_idx: 1,
            px: 70.,
            py: 50.,
        }],
    );
    assert!(
        matches!(
            layout.interactive_move,
            Some(InteractiveMoveState::Moving(_))
        ),
        "instant click-to-drop pickup must detach on motion"
    );
    assert!(
        !window_is_floating(&layout, 0),
        "pickup stays tiled until held-drag release"
    );
    layout.set_window_floating(Some(&0), true);
    layout.interactive_move_end(&0);
    layout.verify_invariants();
    assert!(
        window_is_floating(&layout, 0),
        "held-drag release must float the window on the strip"
    );
}

#[test]
fn click_to_drop_retile_floating_then_click_stays_tiled() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(0)
            },
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
    ]);
    assert!(window_is_floating(&layout, 0));
    apply_click_to_drop_pickup(&mut layout, 0);
    assert!(
        window_is_floating(&layout, 0),
        "click-to-drop press must not retile before the click/drag decision"
    );
    layout.interactive_move_click_released(&0);
    assert!(
        !window_is_floating(&layout, 0),
        "click release must retile a floating window"
    );
    layout.interactive_move_end(&0);
    layout.verify_invariants();
    assert!(
        !window_is_floating(&layout, 0),
        "click without drag must keep the retiled placement"
    );
}

#[test]
fn click_to_drop_retile_floating_then_held_drag_floats() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(0)
            },
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
    ]);
    apply_click_to_drop_pickup(&mut layout, 0);
    layout.interactive_move_held_drag(&0);
    check_ops_on_layout(
        &mut layout,
        [Op::InteractiveMoveUpdate {
            window: 0,
            dx: 20.,
            dy: 0.,
            output_idx: 1,
            px: 70.,
            py: 50.,
        }],
    );
    assert!(
        window_is_floating(&layout, 0),
        "held drag of a floating window never passes through tiling"
    );
    layout.interactive_move_end(&0);
    layout.verify_invariants();
    assert!(
        window_is_floating(&layout, 0),
        "held-drag release must return the window to floating"
    );
}

#[test]
fn interactive_move_floating_does_not_open_tiled_drop_ui() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(2)
            },
        },
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 200.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 300.,
            dy: 0.,
            output_idx: 1,
            px: 500.,
            py: 360.,
        },
    ];

    let layout = check_ops(ops);
    assert!(
        window_is_floating(&layout, 2),
        "already-floating pickup must stay a floating drag"
    );
    assert!(
        layout
            .workspaces()
            .all(|(_, _, ws)| ws.live_reflow_gap().is_none()),
        "item 4: floating drag must not set tiled live_reflow_gap / drop_dest"
    );
}

#[test]
fn interactive_move_float_mid_drag_clears_tiled_drop_ui() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 200.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 300.,
            dy: 0.,
            output_idx: 1,
            px: 500.,
            py: 360.,
        },
    ];

    let mut layout = check_ops(ops);
    assert!(
        layout
            .workspaces()
            .any(|(_, _, ws)| ws.live_reflow_gap().is_some()),
        "tiled drag still opens live-reflow dest"
    );

    layout.set_window_floating(Some(&1), true);
    layout.verify_invariants();
    assert!(window_is_floating(&layout, 1));
    assert!(
        layout
            .workspaces()
            .all(|(_, _, ws)| ws.live_reflow_gap().is_none()),
        "item 4: held_drag / float mid-drag must drop tiled insert-hint / squeeze / gaps"
    );
}

#[test]
fn interactive_move_live_reflow_opens_sideways_column_gap() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 200.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 300.,
            dy: 0.,
            output_idx: 1,
            px: 500.,
            py: 360.,
        },
    ];

    let mut layout = check_ops(ops);
    assert!(
        matches!(
            layout.interactive_move,
            Some(InteractiveMoveState::Moving(_))
        ),
        "window must have been picked up"
    );
    assert_eq!(
        layout.workspaces().next().unwrap().2.windows().count(),
        2,
        "moved window is detached from the workspace"
    );
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        matches!(
            gap.map(|g| g.position),
            Some(super::monitor::InsertPosition::NewColumn(_))
        ),
        "window-center in leftover must open a NewColumn gap, got {gap:?}"
    );

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert!(layout
        .workspaces()
        .all(|(_, _, ws)| ws.live_reflow_gap().is_none()));
    assert_eq!(layout.windows().count(), 3);
}

#[test]
fn interactive_move_live_reflow_opens_in_column_gap() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusColumnLeft,
        Op::ConsumeWindowIntoColumn,
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 50.,
            py: 360.,
        },
        // Pickup, then stay over the remaining stacked tile in the top band.
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 260.,
            dy: -280.,
            output_idx: 1,
            px: 80.,
            py: 80.,
        },
    ];

    let mut layout = check_ops(ops);
    assert!(
        matches!(
            layout.interactive_move,
            Some(InteractiveMoveState::Moving(_))
        ),
        "window must have been picked up"
    );
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        matches!(
            gap.map(|g| g.position),
            Some(super::monitor::InsertPosition::InColumn(..))
        ),
        "stacking over a column must open a vertical live-reflow gap, got {gap:?}"
    );

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert!(layout
        .workspaces()
        .all(|(_, _, ws)| ws.live_reflow_gap().is_none()));
}

#[test]
fn interactive_move_live_reflow_cancel_restores_origin() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 400.,
            dy: 0.,
            output_idx: 1,
            px: 450.,
            py: 50.,
        },
    ];

    let mut layout = check_ops(ops);
    layout.mark_interactive_move_cancelled();
    layout.interactive_move_end(&0);
    layout.verify_invariants();
    assert!(layout
        .workspaces()
        .all(|(_, _, ws)| ws.live_reflow_gap().is_none()));
    assert_eq!(layout.windows().count(), 2);
}

#[test]
fn interactive_move_live_reflow_can_be_disabled() {
    let options = Options {
        layout: niri_config::Layout {
            interactive_move: niri_config::InteractiveMove {
                live_reflow: false,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 300.,
            dy: 0.,
            output_idx: 1,
            px: 350.,
            py: 50.,
        },
    ];

    let layout = check_ops_with_options(options, ops);
    assert!(matches!(
        layout.interactive_move,
        Some(InteractiveMoveState::Moving(_))
    ));
    assert!(
        layout
            .workspaces()
            .all(|(_, _, ws)| ws.live_reflow_gap().is_none()),
        "live-reflow off must not reserve a placeholder gap"
    );
}

fn remaining_tiled_column_gap(layout: &Layout<TestWindow>) -> f64 {
    let ws = layout.workspaces().next().unwrap().2;
    let mut xs: Vec<(f64, f64)> = ws
        .scrolling()
        .tiles_with_render_positions()
        .map(|(tile, pos, _)| (pos.x, tile.tile_size().w))
        .collect();
    xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    assert!(
        xs.len() >= 2,
        "need two remaining tiled columns to measure a gap, got {}",
        xs.len()
    );
    xs[1].0 - (xs[0].0 + xs[0].1)
}

#[test]
fn interactive_move_tiled_drag_keeps_extra_column_gap() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 200.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 300.,
            dy: 0.,
            output_idx: 1,
            px: 500.,
            py: 360.,
        },
        Op::CompleteAnimations,
    ];

    let layout = check_ops(ops);
    let ws = layout.workspaces().next().unwrap().2;
    assert!(
        ws.scrolling().is_drag_spread(),
        "tiled drag must keep extra column gap (drag_spread)"
    );
    let expected = ws.scrolling().options().layout.gaps + super::scrolling::DRAG_COL_GAP_EXTRA;
    let gap = remaining_tiled_column_gap(&layout);
    assert!(
        (gap - expected).abs() < 1.,
        "tiled drag extra gap should be {expected}, got {gap}"
    );
}

#[test]
fn interactive_move_floating_drag_does_not_spread_column_gap() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(2)
            },
        },
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 300.,
            dy: 0.,
            output_idx: 1,
            px: 350.,
            py: 50.,
        },
        Op::CompleteAnimations,
    ];

    let layout = check_ops(ops);
    let ws = layout.workspaces().next().unwrap().2;
    assert!(
        !ws.scrolling().is_drag_spread(),
        "floating drag must not open extra column gaps"
    );
    let expected = ws.scrolling().options().layout.gaps;
    let gap = remaining_tiled_column_gap(&layout);
    assert!(
        (gap - expected).abs() < 1.,
        "floating drag must keep idle column gap {expected}, got {gap}"
    );
}

#[test]
fn maximize_column_does_not_shrink_wider_than_viewport() {
    // Regression: toggle_full_width / set_full_width_gapped used to set
    // is_full_width on columns already Fixed > 1 viewport, which resolves to
    // Proportion(1.) and spontaneously shrinks the window.
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SetColumnWidth(SizeChange::SetFixed(2000)),
    ];
    let mut layout = check_ops(ops);
    let before = layout
        .windows()
        .find(|(_, w)| *w.id() == 0)
        .unwrap()
        .1
        .requested_size()
        .unwrap()
        .w;
    assert!(
        before > 1200,
        "precondition: column should be wider than one viewport, got {before}"
    );

    check_ops_on_layout(&mut layout, [Op::MaximizeColumn]);
    let after_toggle = layout
        .windows()
        .find(|(_, w)| *w.id() == 0)
        .unwrap()
        .1
        .requested_size()
        .unwrap()
        .w;
    assert_eq!(
        after_toggle, before,
        "maximize-column must not shrink a >1-viewport column"
    );

    layout.set_full_width_gapped(true);
    let after_gapped = layout
        .windows()
        .find(|(_, w)| *w.id() == 0)
        .unwrap()
        .1
        .requested_size()
        .unwrap()
        .w;
    assert_eq!(
        after_gapped, before,
        "set_full_width_gapped must not shrink a >1-viewport column"
    );
}

#[test]
fn tiled_drag_edge_grows_strip_toward_cap() {
    // Hold the pointer at the right screen edge during a tiled drag: once the
    // view hits the content bound, virtual strip padding grows toward the
    // 4-viewport cap. Floating drags never set drag_spread so they skip this.
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 600.,
            py: 360.,
        },
        // Cross the move threshold (256px) so we enter Moving + drag_spread.
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 300.,
            dy: 0.,
            output_idx: 1,
            px: 900.,
            py: 360.,
        },
        // Physical right edge of the 1280-wide test output (trigger zone = 30px).
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 0.,
            dy: 0.,
            output_idx: 1,
            px: 1275.,
            py: 360.,
        },
    ];
    let mut layout = check_ops(ops);
    assert!(
        matches!(
            layout.interactive_move,
            Some(InteractiveMoveState::Moving(_))
        ),
        "precondition: interactive move must be Moving"
    );
    assert!(
        layout
            .workspaces()
            .next()
            .unwrap()
            .2
            .scrolling()
            .is_drag_spread(),
        "precondition: tiled drag_spread"
    );
    assert_eq!(
        layout
            .workspaces()
            .next()
            .unwrap()
            .2
            .scrolling()
            .drag_edge_extra(),
        (0., 0.)
    );

    // Debounce is zero while drag_spread; hold at the edge for several frames.
    for _ in 0..40 {
        check_ops_on_layout(
            &mut layout,
            [
                Op::InteractiveMoveUpdate {
                    window: 1,
                    dx: 0.,
                    dy: 0.,
                    output_idx: 1,
                    px: 1275.,
                    py: 360.,
                },
                Op::AdvanceAnimations { msec_delta: 16 },
            ],
        );
    }

    let (left, right) = layout
        .workspaces()
        .next()
        .unwrap()
        .2
        .scrolling()
        .drag_edge_extra();
    assert!(
        right > 10.,
        "right edge hold during tiled drag must grow strip padding, got left={left} right={right}"
    );
    assert!(
        left.abs() < 1.,
        "right-edge grow must not add left padding, got {left}"
    );

    check_ops_on_layout(&mut layout, [Op::InteractiveMoveEnd { window: 1 }]);
    let (left, right) = layout
        .workspaces()
        .next()
        .unwrap()
        .2
        .scrolling()
        .drag_edge_extra();
    assert_eq!(
        (left, right),
        (0., 0.),
        "release must clear edge-grow virtual padding"
    );
}

#[test]
fn interactive_move_gutter_does_not_expand_to_window_width() {
    let options = Options {
        layout: niri_config::Layout {
            interactive_move: niri_config::InteractiveMove {
                live_reflow: true,
                gutter_expand_ms: 500,
            },
            ..Default::default()
        },
        ..Default::default()
    };
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: {
                let mut params = TestWindowParams::new(0);
                params.bbox = Rectangle::from_size(Size::from((400, 300)));
                params
            },
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 500.,
            dy: 0.,
            output_idx: 1,
            px: 550.,
            py: 50.,
        },
        Op::AdvanceAnimations { msec_delta: 2000 },
        Op::CompleteAnimations,
    ];

    let mut layout = check_ops_with_options(options, ops);
    let ws = layout.workspaces().next().unwrap().2;
    let dest = ws.live_reflow_gap();
    assert!(
        dest.is_some_and(|g| !g.origin
            && g.squeeze.is_none()
            && matches!(g.position, super::monitor::InsertPosition::NewColumn(_))),
        "gutter hover remains a drop dest, got {dest:?}"
    );
    assert!(
        dest.unwrap().width
            > ws.scrolling().options().layout.gaps + super::scrolling::DRAG_COL_GAP_EXTRA + 50.,
        "dest still stores dragged width for drop, got {:?}",
        dest.map(|g| g.width)
    );
    assert!(
        ws.scrolling().live_reflow_layout_gap().is_none(),
        "gutter-expand-ms must not open a window-width layout hole, got {:?}",
        ws.scrolling().live_reflow_layout_gap()
    );

    layout.interactive_move_end(&0);
    layout.verify_invariants();
}

#[test]
fn interactive_move_gutter_hint_matches_column_gap() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: {
                let mut params = TestWindowParams::new(0);
                params.bbox = Rectangle::from_size(Size::from((400, 300)));
                params
            },
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 500.,
            dy: 0.,
            output_idx: 1,
            px: 550.,
            py: 50.,
        },
        Op::CompleteAnimations,
    ];

    let layout = check_ops(ops);
    let ws = layout.workspaces().next().unwrap().2;
    let dest = ws
        .live_reflow_gap()
        .expect("leftover gutter must be a drop dest");
    assert!(
        !dest.origin
            && dest.squeeze.is_none()
            && matches!(dest.position, super::monitor::InsertPosition::NewColumn(_)),
        "expected leftover NewColumn dest, got {dest:?}"
    );
    let hint = ws
        .insert_hint_area(dest.position)
        .expect("gutter dest must show an insert hint");
    let expected = ws.scrolling().options().layout.gaps + super::scrolling::DRAG_COL_GAP_EXTRA;
    assert!(
        (hint.size.w - expected).abs() < 1.,
        "hint width must match the enlarged gutter {expected}, got {} (dragged dest width {})",
        hint.size.w,
        dest.width
    );
    assert!(
        hint.size.w + 50. < dest.width,
        "hint must not be the dragged window width"
    );
}

#[test]
fn interactive_move_keeps_origin_hole_until_other_column() {
    let wide = |id| {
        let mut params = TestWindowParams::new(id);
        params.bbox = Rectangle::from_size(Size::from((600, 400)));
        params
    };
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow { params: wide(0) },
        Op::AddWindow { params: wide(1) },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 750.,
            py: 360.,
        },
        // Pickup the right column; stay in the trailing dragged-width origin gap.
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 260.,
            dy: 0.,
            output_idx: 1,
            px: 1010.,
            py: 360.,
        },
    ];

    let mut layout = check_ops(ops);
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        gap.is_some_and(|g| !g.origin
            && g.squeeze.is_none()
            && matches!(g.position, super::monitor::InsertPosition::NewColumn(_)))
            || gap.is_none(),
        "pickup must close the origin hole; trailing dest is overlay only, got {gap:?}"
    );
    assert_eq!(
        layout.workspaces().next().unwrap().2.windows().count(),
        1,
        "moved window is detached; the other stays put"
    );

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert!(layout
        .workspaces()
        .all(|(_, _, ws)| ws.live_reflow_gap().is_none()));
    assert_eq!(
        layout.windows().count(),
        2,
        "drop without a dest split must restore origin"
    );
}

#[test]
fn interactive_move_commits_when_crossing_other_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 150.,
            py: 50.,
        },
        // Detach first (threshold is 256 px), still to the right of the remaining column.
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 260.,
            dy: 0.,
            output_idx: 1,
            px: 410.,
            py: 50.,
        },
        // Then pass over the other column so it splits and the origin hole can close.
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: -360.,
            dy: 0.,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
    ];

    let mut layout = check_ops(ops);
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        gap.is_some_and(|g| !g.origin),
        "crossing a different column must open a dest split, got {gap:?}"
    );

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert!(layout
        .workspaces()
        .all(|(_, _, ws)| ws.live_reflow_gap().is_none()));
    assert_eq!(layout.windows().count(), 2);
}

#[test]
fn interactive_move_gap_with_space_opens_new_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 50.,
            py: 50.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 500.,
            dy: 0.,
            output_idx: 1,
            px: 550.,
            py: 50.,
        },
    ];

    let mut layout = check_ops(ops);
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        gap.is_some_and(|g| !g.origin
            && g.squeeze.is_none()
            && matches!(g.position, super::monitor::InsertPosition::NewColumn(_))),
        "gutter with leftover must open a new column, got {gap:?}"
    );

    layout.interactive_move_end(&0);
    layout.verify_invariants();
    assert_eq!(layout.windows().count(), 2);
}

#[test]
fn interactive_move_over_window_splits_sideways() {
    let wide = |id| {
        let mut params = TestWindowParams::new(id);
        params.bbox = Rectangle::from_size(Size::from((600, 400)));
        params
    };
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow { params: wide(0) },
        Op::AddWindow { params: wide(1) },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 750.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 260.,
            dy: 0.,
            output_idx: 1,
            px: 1010.,
            py: 360.,
        },
        // Center of the remaining 600px column (outside the 80px edge band and stack band).
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: -710.,
            dy: 0.,
            output_idx: 1,
            px: 300.,
            py: 360.,
        },
    ];

    let mut layout = check_ops(ops);
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        gap.is_some_and(|g| !g.origin && g.squeeze.is_some()),
        "passing over a window must split it sideways, got {gap:?}"
    );

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert_eq!(layout.windows().count(), 2);
}

#[test]
fn interactive_move_cancel_over_split_restores() {
    let wide = |id| {
        let mut params = TestWindowParams::new(id);
        params.bbox = Rectangle::from_size(Size::from((600, 400)));
        params
    };
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow { params: wide(0) },
        Op::AddWindow { params: wide(1) },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 750.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 260.,
            dy: 0.,
            output_idx: 1,
            px: 1010.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: -710.,
            dy: 0.,
            output_idx: 1,
            px: 300.,
            py: 360.,
        },
    ];

    let mut layout = check_ops(ops);
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        gap.is_some_and(|g| !g.origin && g.squeeze.is_some()),
        "must be hovering a split dest, got {gap:?}"
    );

    layout.mark_interactive_move_cancelled();
    layout.interactive_move_end(&1);
    layout.verify_invariants();
    let ws = layout.workspaces().next().unwrap().2;
    assert_eq!(ws.windows().count(), 2);
    let widths: Vec<f64> = ws
        .tiles_with_render_positions()
        .map(|(tile, _, _)| tile.tile_size().w)
        .collect();
    assert!(
        widths.iter().all(|w| *w > 400.),
        "cancel over a split dest must restore full columns, got {widths:?}"
    );
}

#[test]
fn interactive_move_over_window_splits_left() {
    let wide = |id| {
        let mut params = TestWindowParams::new(id);
        params.bbox = Rectangle::from_size(Size::from((600, 400)));
        params
    };
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow { params: wide(0) },
        Op::AddWindow { params: wide(1) },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 750.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 260.,
            dy: 0.,
            output_idx: 1,
            px: 1010.,
            py: 360.,
        },
        // Left half of the remaining column (cursor dest, outside the stack band).
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: -860.,
            dy: 0.,
            output_idx: 1,
            px: 150.,
            py: 360.,
        },
    ];

    let mut layout = check_ops(ops);
    let gap = layout
        .workspaces()
        .find_map(|(_, _, ws)| ws.live_reflow_gap());
    assert!(
        gap.is_some_and(|g| !g.origin
            && g.squeeze.is_some_and(|(col, _)| {
                matches!(g.position, super::monitor::InsertPosition::NewColumn(idx) if idx <= col)
            })),
        "left half of a window must split to its left, got {gap:?}"
    );

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert_eq!(layout.windows().count(), 2);
}

#[test]
fn drop_zone_guides_match_real_stack_and_split_sites() {
    let wide = |id| {
        let mut params = TestWindowParams::new(id);
        params.bbox = Rectangle::from_size(Size::from((600, 400)));
        params
    };
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow { params: wide(0) },
        Op::AddWindow { params: wide(1) },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 750.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 260.,
            dy: 0.,
            output_idx: 1,
            px: 1010.,
            py: 360.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: -860.,
            dy: 0.,
            output_idx: 1,
            px: 150.,
            py: 360.,
        },
    ];

    let layout = check_ops(ops);
    let (gap, rects) = layout
        .workspaces()
        .find_map(|(_, _, ws)| {
            let gap = ws.live_reflow_gap()?;
            let col = gap.overlay_column_idx()?;
            Some((gap, ws.drop_zone_guide_rects(col)))
        })
        .expect("hovered column should have overlay dest and zone marks");
    assert!(
        gap.squeeze.is_some(),
        "middle of the column is a side split"
    );
    assert_eq!(
        rects.len(),
        4,
        "stack bands plus left/right split sites, got {rects:?}"
    );
    let top = rects[0].size.h;
    let bottom = rects[1].size.h;
    let mid = rects[2].size.h;
    assert!(
        (top - bottom).abs() < 2.,
        "top/bottom stack bands should match, {top} vs {bottom}"
    );
    assert!(
        mid > top,
        "middle split sites must be taller than the 20% stack band, mid={mid} top={top}"
    );
}

#[test]
fn alone_window_fills_column_height_no_hole() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        // Even after an explicit fixed height, a lone tiled window must fill the column.
        Op::SetWindowHeight {
            id: Some(0),
            change: SizeChange::SetFixed(100),
        },
    ];

    let layout = check_ops(ops);
    let win = layout.windows().next().unwrap().1;
    let h = win.requested_size().unwrap().h;
    assert!(
        h > 400,
        "alone window must fill column height, got requested h={h}"
    );
}

#[test]
fn resize_edges_under_always_returns_a_corner() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
    ];
    let layout = check_ops(ops);
    let output = layout
        .outputs()
        .find(|o| o.name() == "output1")
        .cloned()
        .unwrap();

    // Probe a grid across the output; every hit on the window must yield a corner.
    let mut hits = 0;
    for x in (40..600).step_by(40) {
        for y in (40..600).step_by(40) {
            let pos = Point::from((f64::from(x), f64::from(y)));
            if let Some(edges) = layout.resize_edges_under(&output, pos) {
                hits += 1;
                assert!(
                    edges.intersects(ResizeEdge::LEFT_RIGHT)
                        && edges.intersects(ResizeEdge::TOP_BOTTOM),
                    "expected a corner at ({x},{y}), got {edges:?}"
                );
            }
        }
    }
    assert!(hits > 0, "must hit the window at least once");
}

#[test]
fn interactive_move_onto_empty_output() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::AddOutput(2),
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 2,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_onto_empty_output_ewaf() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::AddOutput(2),
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 2,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn interactive_move_onto_last_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::InteractiveMoveBegin {
            window: 0,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveUpdate {
            window: 0,
            dx: 1000.,
            dy: 0.,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::FocusWorkspaceDown,
        Op::AdvanceAnimations { msec_delta: 1000 },
        Op::InteractiveMoveEnd { window: 0 },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_onto_first_empty_workspace() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::InteractiveMoveBegin {
            window: 1,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::InteractiveMoveUpdate {
            window: 1,
            dx: 1000.,
            dy: 0.,
            output_idx: 1,
            px: 0.,
            py: 0.,
        },
        Op::FocusWorkspaceUp,
        Op::AdvanceAnimations { msec_delta: 1000 },
        Op::InteractiveMoveEnd { window: 1 },
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn output_active_workspace_is_preserved() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 1);
}

#[test]
fn output_active_workspace_is_preserved_with_other_outputs() {
    let ops = [
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::RemoveOutput(1),
        Op::AddOutput(1),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[1].active_workspace_idx, 1);
}

#[test]
fn named_workspace_to_output() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: None,
            layout_config: None,
        },
        Op::AddOutput(1),
        Op::MoveWorkspaceToOutput(1),
        Op::FocusWorkspaceUp,
    ];
    check_ops(ops);
}

#[test]
// empty_workspace_above_first = true
fn named_workspace_to_output_ewaf() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(2),
            layout_config: None,
        },
        Op::AddOutput(1),
        Op::AddOutput(2),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn move_window_to_empty_workspace_above_first() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MoveWorkspaceUp,
        Op::MoveWorkspaceDown,
        Op::FocusWorkspaceUp,
        Op::MoveWorkspaceDown,
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn move_window_to_different_output() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::AddOutput(2),
        Op::MoveWorkspaceToOutput(2),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn close_window_empty_ws_above_first() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddOutput(1),
        Op::CloseWindow(1),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn add_and_remove_output() {
    let ops = [
        Op::AddOutput(2),
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::RemoveOutput(2),
    ];
    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn switch_ewaf_on() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let mut layout = check_ops(ops);
    layout.update_options(Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    });
    layout.verify_invariants();
}

#[test]
fn switch_ewaf_off() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut layout = check_ops_with_options(options, ops);
    layout.update_options(Options::default());
    layout.verify_invariants();
}

#[test]
fn interactive_move_drop_on_other_output_during_animation() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::InteractiveMoveBegin {
            window: 3,
            output_idx: 3,
            px: 0.0,
            py: 0.0,
        },
        Op::FocusWorkspaceDown,
        Op::AddOutput(4),
        Op::InteractiveMoveUpdate {
            window: 3,
            dx: 0.0,
            dy: 8300.68619826683,
            output_idx: 4,
            px: 0.0,
            py: 0.0,
        },
        Op::RemoveOutput(4),
        Op::InteractiveMoveEnd { window: 3 },
    ];
    check_ops(ops);
}

#[test]
fn add_window_next_to_only_interactively_moved_without_outputs() {
    let ops = [
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(1),
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::RemoveOutput(1),
        // We have no outputs, and the only existing window is interactively moved, meaning there
        // are no workspaces either.
        Op::AddWindowNextTo {
            params: TestWindowParams::new(3),
            next_to_id: 2,
        },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_toggle_floating_ends_dnd_gesture() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::Refresh { is_active: false },
        Op::ToggleWindowFloating { id: None },
        Op::InteractiveMoveEnd { window: 2 },
    ];

    check_ops(ops);
}

#[test]
fn interactive_move_from_workspace_with_layout_config() {
    let ops = [
        Op::AddNamedWorkspace {
            ws_name: 1,
            output_name: Some(2),
            layout_config: Some(Box::new(niri_config::LayoutPart {
                border: Some(niri_config::BorderRule {
                    on: true,
                    ..Default::default()
                }),
                ..Default::default()
            })),
        },
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::InteractiveMoveBegin {
            window: 2,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 3586.692842955048,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        // Now remove and add the output. It will have the same workspace.
        Op::RemoveOutput(1),
        Op::AddOutput(1),
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 0.0,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
        // Now move onto a different workspace.
        Op::FocusWorkspaceDown,
        Op::CompleteAnimations,
        Op::InteractiveMoveUpdate {
            window: 2,
            dx: 0.0,
            dy: 0.0,
            output_idx: 1,
            px: 0.0,
            py: 0.0,
        },
    ];

    check_ops(ops);
}

#[test]
fn set_width_fixed_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::SetColumnWidth(SizeChange::SetFixed(-100)),
    ];
    check_ops(ops);
}

#[test]
fn set_height_fixed_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::SetWindowHeight {
            id: None,
            change: SizeChange::SetFixed(-100),
        },
    ];
    check_ops(ops);
}

#[test]
fn interactive_resize_to_negative() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
        Op::InteractiveResizeBegin {
            window: 3,
            edges: ResizeEdge::BOTTOM_RIGHT,
        },
        Op::InteractiveResizeUpdate {
            window: 3,
            dx: -10000.,
            dy: -10000.,
        },
    ];
    check_ops(ops);
}

#[test]
fn windows_on_other_workspaces_remain_activated() {
    let ops = [
        Op::AddOutput(3),
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::FocusWorkspaceDown,
        Op::Refresh { is_active: true },
    ];

    let layout = check_ops(ops);
    let (_, win) = layout.windows().next().unwrap();
    assert!(win.0.pending_activated.get());
}

#[test]
fn stacking_add_parent_brings_up_child() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(1),
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
    ];

    check_ops(ops);
}

#[test]
fn stacking_add_parent_brings_up_descendants() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(2),
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(0),
                ..TestWindowParams::new(1)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(2)
            },
        },
    ];

    check_ops(ops);
}

#[test]
fn stacking_activate_brings_up_descendants() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(0),
                ..TestWindowParams::new(1)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                parent_id: Some(1),
                ..TestWindowParams::new(2)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(3)
            },
        },
        Op::FocusWindow(0),
    ];

    check_ops(ops);
}

#[test]
fn stacking_set_parent_brings_up_child() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(0)
            },
        },
        Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                ..TestWindowParams::new(1)
            },
        },
        Op::SetParent {
            id: 0,
            new_parent_id: Some(1),
        },
    ];

    check_ops(ops);
}

#[test]
fn move_window_to_workspace_with_different_active_output() {
    let ops = [
        Op::AddOutput(0),
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusOutput(1),
        Op::MoveWindowToWorkspace {
            window_id: Some(0),
            workspace_idx: 2,
        },
    ];

    check_ops(ops);
}

#[test]
fn set_first_workspace_name() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    check_ops(ops);
}

#[test]
fn set_first_workspace_name_ewaf() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    let options = Options {
        layout: niri_config::Layout {
            empty_workspace_above_first: true,
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn set_last_workspace_name() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
    ];

    check_ops(ops);
}

#[test]
fn move_workspace_to_same_monitor_doesnt_reorder() {
    let ops = [
        Op::AddOutput(0),
        Op::SetWorkspaceName {
            new_ws_name: 0,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::FocusWorkspaceDown,
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::MoveWorkspaceToMonitor {
            ws_name: Some(0),
            output_id: 0,
        },
    ];

    let layout = check_ops(ops);
    let counts: Vec<_> = layout
        .workspaces()
        .map(|(_, _, ws)| ws.windows().count())
        .collect();
    assert_eq!(counts, &[1, 2, 0]);
}

#[test]
fn removing_window_above_preserves_focused_window() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::FocusColumnFirst,
        Op::ConsumeWindowIntoColumn,
        Op::ConsumeWindowIntoColumn,
        Op::FocusWindowDown,
        Op::CloseWindow(0),
    ];

    let layout = check_ops(ops);
    let win = layout.focus().unwrap();
    assert_eq!(win.0.id, 1);
}

#[test]
fn preset_column_width_fixed_correct_with_border() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SwitchPresetColumnWidth,
    ];

    let options = Options {
        layout: niri_config::Layout {
            preset_column_widths: vec![PresetSize::Fixed(500)],
            ..Default::default()
        },
        ..Default::default()
    };
    let mut layout = check_ops_with_options(options, ops);

    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 500);

    // Add border.
    let options = Options {
        layout: niri_config::Layout {
            preset_column_widths: vec![PresetSize::Fixed(500)],
            border: niri_config::Border {
                off: false,
                width: 5.,
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    layout.update_options(options);

    // With border, the window gets less size.
    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 490);

    // However, preset fixed width will still work correctly.
    layout.toggle_width(true);
    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 500);
}

#[test]
fn preset_column_width_reset_after_set_width() {
    let ops = [
        Op::AddOutput(0),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::SwitchPresetColumnWidth,
        Op::SetWindowWidth {
            id: None,
            change: SizeChange::AdjustFixed(-10),
        },
        Op::SwitchPresetColumnWidth,
    ];

    let options = Options {
        layout: niri_config::Layout {
            preset_column_widths: vec![PresetSize::Fixed(500), PresetSize::Fixed(1000)],
            ..Default::default()
        },
        ..Default::default()
    };
    let layout = check_ops_with_options(options, ops);
    let win = layout.windows().next().unwrap().1;
    assert_eq!(win.requested_size().unwrap().w, 500);
}

#[test]
fn move_column_to_workspace_unfocused_with_multiple_monitors() {
    let ops = [
        Op::AddOutput(1),
        Op::SetWorkspaceName {
            new_ws_name: 101,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::SetWorkspaceName {
            new_ws_name: 102,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddOutput(2),
        Op::FocusOutput(2),
        Op::SetWorkspaceName {
            new_ws_name: 201,
            ws_name: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::AddWindow {
            params: TestWindowParams::new(4),
        },
        Op::MoveColumnToOutput {
            output_id: 1,
            target_ws_idx: Some(0),
            activate: false,
        },
        Op::FocusOutput(1),
    ];

    let layout = check_ops(ops);

    assert_eq!(layout.active_workspace().unwrap().name().unwrap(), "ws102");

    for (mon, win) in layout.windows() {
        let mon = mon.unwrap();
        let ws = mon
            .workspaces
            .iter()
            .find(|w| w.has_window(win.id()))
            .unwrap();

        assert_eq!(
            ws.name().unwrap(),
            match win.id() {
                1 | 4 => "ws101",
                2 => "ws102",
                3 => "ws201",
                _ => unreachable!(),
            }
        );
    }
}

#[test]
fn move_column_to_workspace_down_focus_false_on_floating_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::MoveColumnToWorkspaceDown(false),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 0);
}

#[test]
fn move_column_to_workspace_focus_false_on_floating_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: None },
        Op::MoveColumnToWorkspace(1, false),
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = layout.monitor_set else {
        unreachable!()
    };

    assert_eq!(monitors[0].active_workspace_idx, 0);
}

#[test]
fn restore_to_floating_persists_across_fullscreen_maximize() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        // Maximize then fullscreen.
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        // Unfullscreen.
        Op::FullscreenWindow(1),
    ];

    let mut layout = check_ops(ops);

    // Unfullscreening should return the window to the maximized state.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_some());

    let ops = [
        // Unmaximize.
        Op::MaximizeWindowToEdges { id: None },
    ];
    check_ops_on_layout(&mut layout, ops);

    // Unmaximize should return the window back to floating.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_none());
}

#[test]
fn unmaximize_during_fullscreen_does_not_float() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: None },
        // Maximize then fullscreen.
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        // Unmaximize.
        Op::MaximizeWindowToEdges { id: None },
    ];

    let mut layout = check_ops(ops);

    // Unmaximize shouldn't have changed the window state since it's fullscreen.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_some());

    let ops = [
        // Unfullscreen.
        Op::FullscreenWindow(1),
    ];
    check_ops_on_layout(&mut layout, ops);

    // Unfullscreen should return the window back to floating.
    let scrolling = layout.active_workspace().unwrap().scrolling();
    assert!(scrolling.tiles().next().is_none());
}

#[test]
fn move_column_to_workspace_maximize_and_fullscreen() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        Op::MoveColumnToWorkspaceDown(true),
        Op::FullscreenWindow(1),
    ];

    let layout = check_ops(ops);
    let (_, win) = layout.windows().next().unwrap();

    // Unfullscreening should return to maximized because the window was maximized before.
    assert_eq!(win.pending_sizing_mode(), SizingMode::Maximized);
}

#[test]
fn move_window_to_workspace_maximize_and_fullscreen() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MaximizeWindowToEdges { id: None },
        Op::FullscreenWindow(1),
        Op::MoveWindowToWorkspaceDown(true),
        Op::FullscreenWindow(1),
    ];

    let layout = check_ops(ops);
    let (_, win) = layout.windows().next().unwrap();

    // Unfullscreening should return to maximized because the window was maximized before.
    //
    // FIXME: it currently doesn't because windows themselves can only be either fullscreen or
    // maximized. So when a window is fullscreen, whether it is also maximized or not is stored in
    // the column. MoveWindowToWorkspace removes the window from the column and this information is
    // forgotten.
    assert_eq!(win.pending_sizing_mode(), SizingMode::Normal);
}

#[test]
fn tabs_with_different_border() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams {
                rules: Some(ResolvedWindowRules {
                    border: niri_config::BorderRule {
                        on: true,
                        ..Default::default()
                    },
                    ..ResolvedWindowRules::default()
                }),
                ..TestWindowParams::new(2)
            },
        },
        Op::SwitchPresetWindowHeight { id: None },
        Op::ToggleColumnTabbedDisplay,
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    let options = Options {
        layout: niri_config::Layout {
            struts: Struts {
                left: FloatOrInt(0.),
                right: FloatOrInt(0.),
                top: FloatOrInt(20000.),
                bottom: FloatOrInt(0.),
            },
            ..Default::default()
        },
        ..Default::default()
    };
    check_ops_with_options(options, ops);
}

#[test]
fn expel_pending_left_from_fullscreen_tabbed_column() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FullscreenWindow(1),
        Op::Communicate(1),
        // 1 is now fullscreen, view_offset_to_restore is set.
        Op::ToggleColumnTabbedDisplay,
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ConsumeOrExpelWindowLeft { id: Some(2) },
        // 2 is consumed into a fullscreen column, fullscreen is requested but not applied.
        //
        // Now, get it back out while keeping it focused.
        //
        // Importantly, we expel it *left*, which results in adding a new column with the exact
        // same active_column_idx.
        Op::FocusWindow(2),
        Op::ConsumeOrExpelWindowLeft { id: None },
    ];

    check_ops(ops);
}

#[test]
fn workspace_render_geo_at_fractional_scale() {
    let ops = [
        Op::AddScaledOutput {
            id: 1,
            scale: 1.1,
            layout_config: None,
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::FocusWorkspaceDown,
        Op::CompleteAnimations,
    ];

    let layout = check_ops(ops);

    let MonitorSet::Normal { monitors, .. } = &layout.monitor_set else {
        unreachable!()
    };

    let mon = &monitors[0];
    let mut iter = mon.workspaces_with_render_geo();
    let (_ws, geo) = iter.next().unwrap();
    assert!(
        iter.next().is_none(),
        "animations are completed, only one workspace should be visible"
    );
    assert_eq!(
        geo.loc.y, 0.,
        "active workspace must be at y = 0 exactly, \
         otherwise a pointer against the screen edge at y = 0 won't hit it"
    );
}

fn parent_id_causes_loop(layout: &Layout<TestWindow>, id: usize, mut parent_id: usize) -> bool {
    if parent_id == id {
        return true;
    }

    'outer: loop {
        for (_, win) in layout.windows() {
            if win.0.id == parent_id {
                match win.0.parent_id.get() {
                    Some(new_parent_id) => {
                        if new_parent_id == id {
                            // Found a loop.
                            return true;
                        }

                        parent_id = new_parent_id;
                        continue 'outer;
                    }
                    // Reached window with no parent.
                    None => return false,
                }
            }
        }

        // Parent is not in the layout.
        return false;
    }
}

fn arbitrary_spacing() -> impl Strategy<Value = f64> {
    // Give equal weight to:
    // - 0: the element is disabled
    // - 4: some reasonable value
    // - random value, likely unreasonably big
    prop_oneof![Just(0.), Just(4.), ((1.)..=65535.)]
}

fn arbitrary_spacing_neg() -> impl Strategy<Value = f64> {
    // Give equal weight to:
    // - 0: the element is disabled
    // - 4: some reasonable value
    // - -4: some reasonable negative value
    // - random value, likely unreasonably big
    prop_oneof![Just(0.), Just(4.), Just(-4.), ((1.)..=65535.)]
}

fn arbitrary_struts() -> impl Strategy<Value = Struts> {
    (
        arbitrary_spacing_neg(),
        arbitrary_spacing_neg(),
        arbitrary_spacing_neg(),
        arbitrary_spacing_neg(),
    )
        .prop_map(|(left, right, top, bottom)| Struts {
            left: FloatOrInt(left),
            right: FloatOrInt(right),
            top: FloatOrInt(top),
            bottom: FloatOrInt(bottom),
        })
}

fn arbitrary_center_focused_column() -> impl Strategy<Value = CenterFocusedColumn> {
    prop_oneof![
        Just(CenterFocusedColumn::Never),
        Just(CenterFocusedColumn::OnOverflow),
        Just(CenterFocusedColumn::Always),
    ]
}

fn arbitrary_tab_indicator_position() -> impl Strategy<Value = TabIndicatorPosition> {
    prop_oneof![
        Just(TabIndicatorPosition::Left),
        Just(TabIndicatorPosition::Right),
        Just(TabIndicatorPosition::Top),
        Just(TabIndicatorPosition::Bottom),
    ]
}

prop_compose! {
    fn arbitrary_focus_ring()(
        off in any::<bool>(),
        width in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
    ) -> niri_config::BorderRule {
        niri_config::BorderRule {
            off,
            on: !off,
            width,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_border()(
        off in any::<bool>(),
        width in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
    ) -> niri_config::BorderRule {
        niri_config::BorderRule {
            off,
            on: !off,
            width,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_shadow()(
        off in any::<bool>(),
        softness in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
    ) -> niri_config::ShadowRule {
        niri_config::ShadowRule {
            off,
            on: !off,
            softness,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_tab_indicator()(
        off in any::<bool>(),
        hide_when_single_tab in prop::option::of(any::<bool>().prop_map(Flag)),
        place_within_column in prop::option::of(any::<bool>().prop_map(Flag)),
        width in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
        gap in prop::option::of(arbitrary_spacing_neg().prop_map(FloatOrInt)),
        length in prop::option::of((0f64..2f64)
            .prop_map(|x| TabIndicatorLength { total_proportion: Some(x) })),
        position in prop::option::of(arbitrary_tab_indicator_position()),
    ) -> niri_config::TabIndicatorPart {
        niri_config::TabIndicatorPart {
            off,
            on: !off,
            hide_when_single_tab,
            place_within_column,
            width,
            gap,
            length,
            position,
            ..Default::default()
        }
    }
}

prop_compose! {
    fn arbitrary_layout_part()(
        gaps in prop::option::of(arbitrary_spacing().prop_map(FloatOrInt)),
        struts in prop::option::of(arbitrary_struts()),
        focus_ring in prop::option::of(arbitrary_focus_ring()),
        border in prop::option::of(arbitrary_border()),
        shadow in prop::option::of(arbitrary_shadow()),
        tab_indicator in prop::option::of(arbitrary_tab_indicator()),
        center_focused_column in prop::option::of(arbitrary_center_focused_column()),
        always_center_single_column in prop::option::of(any::<bool>().prop_map(Flag)),
        empty_workspace_above_first in prop::option::of(any::<bool>().prop_map(Flag)),
    ) -> niri_config::LayoutPart {
        niri_config::LayoutPart {
            gaps,
            struts,
            center_focused_column,
            always_center_single_column,
            empty_workspace_above_first,
            focus_ring,
            border,
            shadow,
            tab_indicator,
            ..Default::default()
        }
    }
}

#[test]
fn floating_full_viewport_matches_tiled_band_and_fills_view() {
    let mut options = Options::default();
    options.layout.gaps = 12.;
    options.layout.struts.bottom = FloatOrInt(48.);

    let layout = check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
            Op::ToggleWindowFloating { id: Some(2) },
            Op::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(1000.),
                y: PositionChange::SetFixed(40.),
                animate: false,
            },
            Op::MaximizeColumn,
        ],
    );

    let band = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .working_area();
    // Yellow has no border and no reserved title bar (it only shows on top-edge hover).
    let expected_win_h = band.size.h.floor().max(1.) as i32;
    let expected_win_w = band.size.w.floor().max(1.) as i32;

    let floating_size = layout
        .windows()
        .find(|(_, w)| *w.id() == 2)
        .unwrap()
        .1
        .requested_size()
        .unwrap();
    assert_eq!(
        floating_size.h, expected_win_h,
        "floating FullViewport must fill the working-area band (no leftover gaps)"
    );
    assert_eq!(
        floating_size.w, expected_win_w,
        "floating FullViewport must span the working-area width"
    );

    let ws = layout.active_workspace().unwrap();
    let (_tile, float_pos) = ws
        .floating()
        .tiles_with_offsets()
        .find(|(t, _)| *t.window().id() == 2)
        .unwrap();
    assert!(
        (float_pos.y - band.loc.y).abs() < 1.,
        "floating Y must sit on the working-area origin, got {} expected {}",
        float_pos.y,
        band.loc.y
    );
    assert!(
        (float_pos.x - band.loc.x).abs() < 1.,
        "floating FullViewport must start at the working-area left, got x={} expected {}",
        float_pos.x,
        band.loc.x
    );
}

fn requested_size_of(layout: &Layout<TestWindow>, id: usize) -> smithay::utils::Size<i32, smithay::utils::Logical> {
    layout
        .windows()
        .find(|(_, w)| *w.id() == id)
        .unwrap()
        .1
        .requested_size()
        .unwrap()
}

#[test]
fn tiled_window_detaches_to_floating_as_one_to_one_square() {
    let layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::ToggleWindowFloating { id: Some(2) },
    ]);
    let ws = layout.active_workspace().unwrap();
    let side = ws.floating_square_side();
    let chrome = crate::layout::window_chrome::WindowChrome::extra_height();

    // Frame is `side` x `side`: 1px border on each side, title bar on top.
    let size = requested_size_of(&layout, 2);
    assert_eq!(size.w, side as i32 - 2);
    assert_eq!(size.h, side as i32 - 2 - chrome as i32);
    // A bit under a quarter of the screen.
    let band = ws.scrolling().working_area();
    assert!(side < band.size.w / 2. && side < band.size.h / 2.);
    assert!(side > band.size.h * 0.4);
}

#[test]
fn floating_lime_ball_sizes_to_square_and_leaves_yellow() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: Some(1) },
        Op::MaximizeColumn,
    ]);
    let band = layout.active_workspace().unwrap().scrolling().working_area();
    assert_eq!(requested_size_of(&layout, 1).w, band.size.w as i32);

    assert!(layout.size_floating_square(Some(&1)));
    let side = layout.active_workspace().unwrap().floating_square_side();
    let size = requested_size_of(&layout, 1);
    assert_eq!(size.w, side as i32 - 2);
    assert_eq!(size.h, side as i32 - 2 - 26);
}

#[test]
fn yellow_title_bar_reveals_only_at_top_edge() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: Some(1) },
    ]);
    let probe = |layout: &Layout<TestWindow>, y: f64| {
        let ws = layout.active_workspace().unwrap();
        let (tile, _) = ws.floating().tiles_with_offsets().next().unwrap();
        tile.wants_chrome_reveal(Point::from((1., y)))
    };
    // Plain floating: the bar is always reserved, never an overlay.
    assert!(!probe(&layout, 0.5));

    layout.toggle_full_width_gapped();
    assert!(probe(&layout, 0.5));
    assert!(!probe(&layout, 12.));
}

#[test]
fn yellow_has_no_frame_edges_and_resize_leaves_yellow() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: Some(1) },
    ]);
    layout.toggle_full_width_gapped();
    {
        let ws = layout.active_workspace().unwrap();
        let (tile, _) = ws.floating().tiles_with_offsets().next().unwrap();
        assert!(tile.is_floating_work_area());
        // Nothing to grab at the top edge of a yellow window.
        assert!(tile.frame_edges_at(Point::from((100., -4.)), 12.).is_empty());
    }

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 1,
                edges: ResizeEdge::RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 1,
                dx: -200.,
                dy: 0.,
            },
            Op::InteractiveResizeEnd { window: 1 },
        ],
    );
    let ws = layout.active_workspace().unwrap();
    let (tile, _) = ws.floating().tiles_with_offsets().next().unwrap();
    assert!(!tile.is_floating_work_area());
    assert!(!tile.wants_chrome_reveal(Point::from((1., 0.5))));
}

#[test]
fn floating_fullscreen_stays_floating_and_fills_output() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: Some(1) },
        Op::FullscreenWindow(1),
    ]);
    let (is_floating, in_scrolling, view) = {
        let ws = layout.active_workspace().unwrap();
        (
            ws.floating().has_window(&1),
            ws.scrolling().columns().any(|c| c.contains(&1)),
            ws.view_size(),
        )
    };
    assert!(
        is_floating,
        "orange fullscreen on a floating window must stay floating"
    );
    assert!(!in_scrolling);
    let requested = layout
        .windows()
        .find(|(_, w)| *w.id() == 1)
        .unwrap()
        .1
        .requested_size()
        .unwrap();
    assert_eq!(requested.w, view.w.round() as i32);
    assert_eq!(requested.h, view.h.round() as i32);
    let pending = layout
        .windows()
        .find(|(_, w)| *w.id() == 1)
        .unwrap()
        .1
        .pending_sizing_mode();
    assert!(
        pending.is_fullscreen(),
        "orange fullscreen must set the real fullscreen state so overlay bars hide"
    );
    assert!(
        layout.active_workspace().unwrap().render_above_top_layer(),
        "floating orange fullscreen must cover the overlay bars like tiled fullscreen"
    );

    check_ops_on_layout(&mut layout, [Op::FullscreenWindow(1)]);
    let pending = layout
        .windows()
        .find(|(_, w)| *w.id() == 1)
        .unwrap()
        .1
        .pending_sizing_mode();
    assert!(
        pending.is_normal(),
        "orange / F11 / Esc must leave floating fullscreen"
    );
    assert!(!layout.active_workspace().unwrap().render_above_top_layer());
}

fn floating_title_bar_band_layout() -> Layout<TestWindow> {
    let mut options = Options::default();
    options.layout.struts.top = FloatOrInt(24.);
    options.layout.struts.bottom = FloatOrInt(48.);
    check_ops_with_options(
        options,
        [
            Op::AddOutput(1),
            Op::AddWindow {
                params: TestWindowParams::new(1),
            },
            Op::ToggleWindowFloating { id: Some(1) },
        ],
    )
}

#[test]
fn floating_title_bar_cannot_go_above_tiled_band() {
    let mut layout = floating_title_bar_band_layout();
    check_ops_on_layout(
        &mut layout,
        [Op::MoveFloatingWindow {
            id: Some(1),
            x: PositionChange::SetFixed(40.),
            y: PositionChange::SetFixed(-2000.),
            animate: false,
        }],
    );

    let ws = layout.active_workspace().unwrap();
    let band = ws.scrolling().working_area();
    let chrome = crate::layout::window_chrome::WindowChrome::extra_height();
    let (_, pos) = ws
        .floating()
        .tiles_with_offsets()
        .find(|(t, _)| *t.window().id() == 1)
        .unwrap();

    assert!(
        pos.y + 1e-6 >= band.loc.y,
        "title bar must not go above band.top: pos.y={} band.top={}",
        pos.y,
        band.loc.y
    );
    assert!(
        (pos.y - band.loc.y).abs() < 1.,
        "extreme negative Y should sit on band.top, got {} expected {}",
        pos.y,
        band.loc.y
    );
    assert!(
        pos.y + chrome <= band.loc.y + band.size.h + 1e-6,
        "title bar must remain inside the tiled band"
    );
    assert!(
        (pos.x - 40.).abs() < 1.,
        "Y clamp must not change strip-X, got {}",
        pos.x
    );
}

#[test]
fn floating_title_bar_cannot_go_below_tiled_band() {
    let mut layout = floating_title_bar_band_layout();
    check_ops_on_layout(
        &mut layout,
        [Op::MoveFloatingWindow {
            id: Some(1),
            x: PositionChange::SetFixed(40.),
            y: PositionChange::SetFixed(8000.),
            animate: false,
        }],
    );

    let ws = layout.active_workspace().unwrap();
    let band = ws.scrolling().working_area();
    let chrome = crate::layout::window_chrome::WindowChrome::extra_height();
    let (tile, pos) = ws
        .floating()
        .tiles_with_offsets()
        .find(|(t, _)| *t.window().id() == 1)
        .unwrap();
    let tile_h = tile.tile_size().h;
    let title_bar_bottom = pos.y + chrome;
    let band_bottom = band.loc.y + band.size.h;
    let clip_y = ws.bottom_clip_y();
    assert!(clip_y < band_bottom);

    assert!(
        title_bar_bottom <= clip_y + 1e-6,
        "title bar must not go below the clip line: title_bar_bottom={title_bar_bottom} \
         clip_y={clip_y}"
    );
    assert!(
        (title_bar_bottom - clip_y).abs() < 1.,
        "extreme positive Y should rest the title bar on the clip line, got {title_bar_bottom} \
         expected {clip_y}"
    );
    assert!(
        pos.y + tile_h > band_bottom + 1.,
        "window body may extend into the bottom strut/rail"
    );
    assert!(
        (pos.x - 40.).abs() < 1.,
        "Y clamp must not change strip-X, got {}",
        pos.x
    );
}

#[test]
fn floating_drop_clamp_keeps_title_bar_in_tiled_band() {
    let layout = floating_title_bar_band_layout();
    let ws = layout.active_workspace().unwrap();
    let band = ws.scrolling().working_area();
    let chrome = crate::layout::window_chrome::WindowChrome::extra_height();
    let (tile, _) = ws
        .floating()
        .tiles_with_offsets()
        .find(|(t, _)| *t.window().id() == 1)
        .unwrap();
    let size = tile.tile_size();
    let wa = ws.working_area();
    let strip_x = wa.size.w * 2.5;
    let pinned_x = wa.loc.x + wa.size.w - size.w;

    let up = ws
        .floating()
        .clamp_within_working_area(Point::from((strip_x, -2000.)), size);
    assert!(
        up.y + 1e-6 >= band.loc.y,
        "drop clamp must not place title bar above band.top: y={} band.top={}",
        up.y,
        band.loc.y
    );
    assert!(
        (up.y - band.loc.y).abs() < 1.,
        "drop clamp top should sit on band.top, got {} expected {}",
        up.y,
        band.loc.y
    );
    assert!(
        (up.x - pinned_x).abs() < 1.,
        "drop clamp must keep the window inside the viewport, got {} expected {pinned_x}",
        up.x
    );

    let down = ws
        .floating()
        .clamp_within_working_area(Point::from((strip_x, 8000.)), size);
    let clip_y = ws.bottom_clip_y();
    assert!(
        down.y + chrome <= clip_y + 1e-6,
        "drop clamp must not place title bar below the clip line: title_bar_bottom={} \
         clip_y={clip_y}",
        down.y + chrome
    );
    assert!(
        ((down.y + chrome) - clip_y).abs() < 1.,
        "drop clamp bottom should rest the title bar on the clip line"
    );
    assert!(
        (down.x - pinned_x).abs() < 1.,
        "drop clamp must keep the window inside the viewport, got {} expected {pinned_x}",
        down.x
    );
}

fn assert_interactive_move_clipped_not_glued(layout: &Layout<TestWindow>, want_floating: bool) {
    let output = layout.outputs().next().cloned().unwrap();
    let Some(InteractiveMoveState::Moving(move_)) = &layout.interactive_move else {
        panic!("expected interactive move to have picked up the window");
    };
    assert_eq!(
        move_.is_floating, want_floating,
        "drag must keep tiled vs floating pickup"
    );

    let ws = layout.active_workspace().unwrap();
    let wa = ws.scrolling().working_area();
    let gaps = ws.scrolling().options().layout.gaps;
    let rail = tile::bottom_rail_y(wa, gaps);
    assert_eq!(rail, ws.bottom_clip_y());
    let tile_pos = move_.tile_render_location(1.);
    let tile_size = move_.tile.tile_size();
    assert!(
        tile_pos.y + tile_size.h > rail,
        "window must overlap the rail band so clip applies (not a move-up): \
         bottom={} rail={rail}",
        tile_pos.y + tile_size.h
    );

    let clip = tile::bottom_rail_clip_bounds(wa, gaps, 1.);
    let clip_bottom = clip.loc.y.saturating_add(clip.size.h);
    let expected = Point::<f64, Logical>::from((0., wa.loc.y + wa.size.h - gaps))
        .to_physical_precise_round::<_, i32>(1.)
        .y;
    assert_eq!(
        clip_bottom, expected,
        "clip rect must end at the bottom of tiled windows (working-area bottom minus gaps)"
    );

    let x = tile_pos.x + tile_size.w / 2.;
    assert!(
        layout
            .interactive_moved_window_under(&output, Point::from((x, rail + 8.)))
            .is_none(),
        "hits in the rail band must miss the dragged window"
    );
    if tile_pos.y < rail - 2. {
        let y_vis = (tile_pos.y + 8.).min(rail - 4.);
        assert!(
            layout
                .interactive_moved_window_under(&output, Point::from((x, y_vis)))
                .is_some(),
            "visible part above the rail must still hit"
        );
    }
}

/// Grab the window by its title bar and drag it down so its body overlaps the rail band.
fn drag_window_into_rail_band(floating: bool) -> Layout<TestWindow> {
    let mut options = Options::default();
    options.layout.gaps = 12.;
    options.layout.struts.bottom = FloatOrInt(48.);
    let mut ops = vec![
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
    ];
    if floating {
        ops.push(Op::ToggleWindowFloating { id: Some(1) });
    }
    ops.push(Op::CompleteAnimations);
    let mut layout = check_ops_with_options(options, ops);

    let (_, tile_pos, _) = layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .find(|(t, _, _)| *t.window().id() == 1)
        .unwrap();
    let grab = tile_pos + Point::from((20., 10.));
    let drop = Point::<f64, Logical>::from((grab.x, 600.));
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveMoveBegin {
                window: 1,
                output_idx: 1,
                px: grab.x,
                py: grab.y,
            },
            Op::InteractiveMoveUpdate {
                window: 1,
                dx: 0.,
                dy: drop.y - grab.y,
                output_idx: 1,
                px: drop.x,
                py: drop.y,
            },
            Op::CompleteAnimations,
        ],
    );
    layout
}

#[test]
fn bottom_clip_line_matches_tiled_window_bottom() {
    let mut options = Options::default();
    options.layout.gaps = 12.;
    options.layout.struts.bottom = FloatOrInt(48.);
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::Communicate(1),
        Op::CompleteAnimations,
    ];
    let layout = check_ops_with_options(options, ops);

    let ws = layout.active_workspace().unwrap();
    let wa = ws.scrolling().working_area();
    let clip_y = ws.bottom_clip_y();
    assert_eq!(clip_y, wa.loc.y + wa.size.h - 12.);

    let (tile, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(t, _, _)| *t.window().id() == 1)
        .unwrap();
    let bottom = pos.y + tile.tile_size().h;
    assert!(
        (bottom - clip_y).abs() < 1e-6,
        "tiled window bottom {bottom} must sit on the clip line {clip_y}"
    );
}

#[test]
fn interactive_move_tiled_is_clipped_at_rail_not_moved_up() {
    let layout = drag_window_into_rail_band(false);
    assert_interactive_move_clipped_not_glued(&layout, false);
}

#[test]
fn interactive_move_floating_is_clipped_at_rail_not_moved_up() {
    let layout = drag_window_into_rail_band(true);
    assert_interactive_move_clipped_not_glued(&layout, true);
}

#[test]
fn settled_floating_body_over_rail_is_hit_clipped() {
    let mut layout = floating_title_bar_band_layout();
    check_ops_on_layout(
        &mut layout,
        [
            Op::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(40.),
                y: PositionChange::SetFixed(8000.),
                animate: false,
            },
            Op::CompleteAnimations,
        ],
    );

    let output = layout.outputs().next().cloned().unwrap();
    let ws = layout.active_workspace().unwrap();
    let rail = ws.bottom_clip_y();
    let view_x = ws.scrolling().view_pos();
    let (tile, pos) = ws
        .floating()
        .tiles_with_offsets()
        .find(|(t, _)| *t.window().id() == 1)
        .unwrap();
    let x = pos.x - view_x + tile.tile_size().w / 2.;
    assert!(
        pos.y + tile.tile_size().h > rail + 1.,
        "body is allowed to overlap the rail; clipping must hide those pixels"
    );
    assert!(
        layout
            .window_under(&output, Point::from((x, rail + 8.)))
            .is_none(),
        "settled window must not receive hits in the rail band"
    );
    let y_bar = pos.y + crate::layout::window_chrome::WindowChrome::extra_height() / 2.;
    assert!(y_bar < rail);
    assert!(
        layout
            .window_under(&output, Point::from((x, y_bar)))
            .is_some(),
        "title bar above the rail must still hit"
    );
}

#[test]
fn maximized_tiled_window_hits_below_clip_line() {
    let mut options = Options::default();
    options.layout.gaps = 12.;
    options.layout.struts.bottom = FloatOrInt(48.);
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::MaximizeWindowToEdges { id: Some(1) },
        Op::Communicate(1),
        Op::CompleteAnimations,
    ];
    let layout = check_ops_with_options(options, ops);

    let output = layout.outputs().next().cloned().unwrap();
    let ws = layout.active_workspace().unwrap();
    let wa = ws.scrolling().working_area();
    let wa_bottom = wa.loc.y + wa.size.h;
    let clip_y = ws.bottom_clip_y();
    let (tile, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(t, _, _)| *t.window().id() == 1)
        .unwrap();
    assert!(
        pos.y + tile.tile_size().h >= wa_bottom - 1e-6,
        "maximized tile must reach the working-area bottom"
    );

    let x = pos.x + tile.tile_size().w / 2.;
    assert!(
        layout
            .window_under(&output, Point::from((x, (clip_y + wa_bottom) / 2.)))
            .is_some(),
        "maximized tiled window must hit between the clip line and the working-area bottom"
    );
    assert!(
        layout
            .window_under(&output, Point::from((x, wa_bottom + 1.)))
            .is_none(),
        "hits below the working-area bottom must miss tiled windows"
    );
}

use crate::layout::floating_group::FloatingSplitSide;
use crate::layout::group_chrome::{GROUP_BAR_W, GROUP_BOTTOM_H};
use crate::layout::window_chrome::WindowChrome;

const GROUP_GAP: f64 = 16.;
/// Width a group's chrome adds left of the cells: side bar and gap.
const GROUP_LEFT: f64 = GROUP_BAR_W + GROUP_GAP;
/// Height a group's chrome adds below the cells: gap and bottom bar.
const GROUP_BELOW: f64 = GROUP_GAP + GROUP_BOTTOM_H;

/// Floating window 2 (`target`) at (100, 100); 200x100 floating windows 1, 3, 4, 5 parked
/// on the right as drag sources.
fn floating_group_layout(target: (i32, i32)) -> Layout<TestWindow> {
    let mut options = Options::default();
    options.layout.gaps = GROUP_GAP;
    let mut ops = vec![Op::AddOutput(1)];
    for (id, size) in [
        (2, target),
        (1, (200, 100)),
        (3, (200, 100)),
        (4, (200, 100)),
        (5, (200, 100)),
    ] {
        ops.push(Op::AddWindow {
            params: TestWindowParams {
                is_floating: true,
                bbox: Rectangle::from_size(Size::from(size)),
                ..TestWindowParams::new(id)
            },
        });
    }
    for id in 1..=5 {
        ops.push(Op::Communicate(id));
    }
    for (id, x, y) in [
        (2, 100., 100.),
        (1, 700., 50.),
        (3, 700., 250.),
        (4, 700., 450.),
        (5, 1000., 50.),
    ] {
        ops.push(Op::MoveFloatingWindow {
            id: Some(id),
            x: PositionChange::SetFixed(x),
            y: PositionChange::SetFixed(y),
            animate: false,
        });
    }
    ops.push(Op::CompleteAnimations);
    check_ops_with_options(options, ops)
}

fn floating_pair_layout() -> Layout<TestWindow> {
    floating_group_layout((400, 300))
}

/// Floating tile rect in strip coordinates.
fn floating_rect(layout: &Layout<TestWindow>, id: usize) -> Rectangle<f64, Logical> {
    let ws = layout.active_workspace().unwrap();
    let (tile, pos) = ws
        .floating()
        .tiles_with_offsets()
        .find(|(t, _)| *t.window().id() == id)
        .unwrap();
    Rectangle::new(pos, tile.tile_size())
}

fn group_mates(layout: &Layout<TestWindow>, id: usize) -> Vec<usize> {
    let ws = layout.active_workspace().unwrap();
    let mut mates = ws.floating().group_mates(&id);
    mates.sort();
    mates
}

fn floating_partner(layout: &Layout<TestWindow>, id: usize) -> Option<usize> {
    let mates = group_mates(layout, id);
    assert!(mates.len() <= 1, "expected a pair: {mates:?}");
    mates.first().copied()
}

fn group_cell(layout: &Layout<TestWindow>, id: usize) -> Rectangle<f64, Logical> {
    let ws = layout.active_workspace().unwrap();
    ws.floating()
        .group_cell(&id)
        .expect("window must be grouped")
}

#[track_caller]
fn assert_rect_near(a: Rectangle<f64, Logical>, b: Rectangle<f64, Logical>, what: &str) {
    let ok = (a.loc.x - b.loc.x).abs() < 1.5
        && (a.loc.y - b.loc.y).abs() < 1.5
        && (a.size.w - b.size.w).abs() < 1.5
        && (a.size.h - b.size.h).abs() < 1.5;
    assert!(ok, "{what}: {a:?} != {b:?}");
}

fn rect_f(x: f64, y: f64, w: f64, h: f64) -> Rectangle<f64, Logical> {
    Rectangle::new(Point::from((x, y)), Size::from((w, h)))
}

/// Grab floating window `id` near its top-left and move the pointer to `to` (output coords).
fn drag_floating_to(layout: &mut Layout<TestWindow>, id: usize, to: Point<f64, Logical>) {
    let view_x = layout.active_workspace().unwrap().scroll_view_pos();
    let rect = floating_rect(layout, id);
    let grab = Point::<f64, Logical>::from((rect.loc.x - view_x + 20., rect.loc.y + 10.));
    check_ops_on_layout(
        layout,
        [
            Op::InteractiveMoveBegin {
                window: id,
                output_idx: 1,
                px: grab.x,
                py: grab.y,
            },
            Op::InteractiveMoveUpdate {
                window: id,
                dx: to.x - grab.x,
                dy: to.y - grab.y,
                output_idx: 1,
                px: to.x,
                py: to.y,
            },
        ],
    );
}

fn settle_floating_pair(layout: &mut Layout<TestWindow>) {
    let mut ops: Vec<Op> = (1..=5).map(Op::Communicate).collect();
    ops.push(Op::CompleteAnimations);
    check_ops_on_layout(layout, ops);
}

fn drop_floating_at(layout: &mut Layout<TestWindow>, id: usize, to: Point<f64, Logical>) {
    drag_floating_to(layout, id, to);
    layout.interactive_move_end(&id);
    layout.verify_invariants();
    settle_floating_pair(layout);
}

fn current_join(layout: &Layout<TestWindow>) -> Option<FloatingJoinTarget<usize>> {
    layout
        .interactive_move
        .as_ref()
        .unwrap()
        .moving()
        .unwrap()
        .floating_join
        .clone()
}

/// Strip point → output point at the current view position.
fn view_point(layout: &Layout<TestWindow>, x: f64, y: f64) -> Point<f64, Logical> {
    let view_x = layout.active_workspace().unwrap().scroll_view_pos();
    Point::from((x - view_x, y))
}

fn right_half_of(
    layout: &Layout<TestWindow>,
    rect: Rectangle<f64, Logical>,
) -> Point<f64, Logical> {
    view_point(
        layout,
        rect.loc.x + rect.size.w * 0.75,
        rect.loc.y + rect.size.h * 0.5,
    )
}

/// Pair: 1 dropped on the right half of 2. Returns the cells box: 2's rect before the join,
/// shifted right by the group bar.
fn form_pair(layout: &mut Layout<TestWindow>) -> Rectangle<f64, Logical> {
    let b0 = floating_rect(layout, 2);
    let to = right_half_of(layout, b0);
    drop_floating_at(layout, 1, to);
    assert_eq!(floating_partner(layout, 1), Some(2));
    let mut cells_box = b0;
    cells_box.loc.x += GROUP_LEFT;
    cells_box
}

/// Pair, then 3 at the bottom of the left side (2 splits, 1 keeps full height).
fn form_three_split_left(layout: &mut Layout<TestWindow>) -> Rectangle<f64, Logical> {
    let b0 = form_pair(layout);
    let left = group_cell(layout, 2);
    let to = view_point(layout, left.loc.x + 20., left.loc.y + left.size.h - 8.);
    drag_floating_to(layout, 3, to);
    let join = current_join(layout).expect("bottom of the left side is a zone");
    assert_eq!(join.window, 2);
    assert_eq!(join.op, FloatingJoinOp::Split(FloatingSplitSide::Bottom));
    layout.interactive_move_end(&3);
    layout.verify_invariants();
    settle_floating_pair(layout);
    b0
}

/// 2x2: 2 TL, 1 TR, 3 BL, 4 BR.
fn form_two_by_two(layout: &mut Layout<TestWindow>) -> Rectangle<f64, Logical> {
    let b0 = form_three_split_left(layout);
    let right = group_cell(layout, 1);
    let to = view_point(
        layout,
        right.loc.x + right.size.w - 20.,
        right.loc.y + right.size.h - 8.,
    );
    drag_floating_to(layout, 4, to);
    let join = current_join(layout).expect("bottom of the right side is a zone");
    assert_eq!(join.window, 1);
    assert_eq!(join.op, FloatingJoinOp::Split(FloatingSplitSide::Bottom));
    layout.interactive_move_end(&4);
    layout.verify_invariants();
    settle_floating_pair(layout);
    b0
}

#[test]
fn floating_drop_on_floating_right_half_forms_side_by_side_group() {
    let mut layout = floating_pair_layout();
    let b0 = floating_rect(&layout, 2);
    let to = right_half_of(&layout, b0);
    drag_floating_to(&mut layout, 1, to);

    let join = current_join(&layout).expect("hovering B must set a join target");
    assert_eq!(join.window, 2);
    assert_eq!(join.op, FloatingJoinOp::Split(FloatingSplitSide::Right));

    let output = layout.outputs().next().cloned().unwrap();
    layout.update_render_elements(Some(&output));
    let mon = layout.monitor_for_output(&output).unwrap();
    assert!(mon.insert_hint.is_none(), "no tiled insert hint");
    let preview = mon
        .floating_join_preview
        .as_ref()
        .expect("preview over the target's right half");
    let view_x = layout.active_workspace().unwrap().scroll_view_pos();
    let half_x = b0.loc.x - view_x + (b0.size.w + GROUP_GAP) / 2. + GROUP_LEFT;
    assert!((preview.hint.loc.x - half_x).abs() < 1.5, "{preview:?}");
    assert!((preview.hint.loc.y - b0.loc.y).abs() < 1.5, "{preview:?}");
    assert!((preview.hint.size.h - b0.size.h).abs() < 1.5, "{preview:?}");

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    settle_floating_pair(&mut layout);

    assert!(window_is_floating(&layout, 1));
    assert!(window_is_floating(&layout, 2));
    assert_eq!(floating_partner(&layout, 1), Some(2));
    assert_eq!(floating_partner(&layout, 2), Some(1));

    let a = floating_rect(&layout, 1);
    let b = floating_rect(&layout, 2);
    assert!(
        (b.loc.x - (b0.loc.x + GROUP_LEFT)).abs() < 1.5,
        "B keeps the left edge, right of the bar: {b:?} {b0:?}"
    );
    assert!(
        (a.loc.x + a.size.w - (b0.loc.x + b0.size.w + GROUP_LEFT)).abs() < 1.5,
        "A ends at B's original right edge plus the bar: {a:?} {b0:?}"
    );
    let gb = layout
        .active_workspace()
        .unwrap()
        .floating_group_box(&1)
        .unwrap();
    assert_rect_near(
        gb,
        rect_f(
            b0.loc.x,
            b0.loc.y,
            b0.size.w + GROUP_LEFT,
            b0.size.h + GROUP_BELOW,
        ),
        "group box includes the bar and the frame",
    );
    assert!(
        (a.loc.x - (b.loc.x + b.size.w + GROUP_GAP)).abs() < 1.5,
        "A sits right of B with a gap: {a:?} {b:?}"
    );
    for r in [a, b] {
        assert!((r.loc.y - b0.loc.y).abs() < 1.5, "{r:?} {b0:?}");
        assert!((r.size.h - b0.size.h).abs() < 1.5, "{r:?} {b0:?}");
    }

    check_ops_on_layout(
        &mut layout,
        [Op::MoveFloatingWindow {
            id: Some(1),
            x: PositionChange::AdjustFixed(-50.),
            y: PositionChange::AdjustFixed(20.),
            animate: false,
        }],
    );
    let a2 = floating_rect(&layout, 1);
    let b2 = floating_rect(&layout, 2);
    assert!((a2.loc.x - (a.loc.x - 50.)).abs() < 0.01);
    assert!(
        (b2.loc.x - (b.loc.x - 50.)).abs() < 0.01,
        "moving A moves B"
    );
    assert!(
        (b2.loc.y - (b.loc.y + 20.)).abs() < 0.01,
        "moving A moves B"
    );
}

#[test]
fn floating_drop_away_from_floating_windows_does_not_group() {
    let mut layout = floating_pair_layout();
    drag_floating_to(&mut layout, 1, Point::from((1000., 600.)));

    assert!(current_join(&layout).is_none());
    let output = layout.outputs().next().cloned().unwrap();
    layout.update_render_elements(Some(&output));
    let mon = layout.monitor_for_output(&output).unwrap();
    assert!(mon.floating_join_preview.is_none(), "no preview off-target");
    assert!(mon.insert_hint.is_none());

    layout.interactive_move_end(&1);
    layout.verify_invariants();
    settle_floating_pair(&mut layout);

    assert!(window_is_floating(&layout, 1));
    assert_eq!(floating_partner(&layout, 1), None);
    assert_eq!(floating_partner(&layout, 2), None);
}

#[test]
fn floating_drag_cancel_over_floating_does_not_group() {
    let mut layout = floating_pair_layout();
    let b0 = floating_rect(&layout, 2);
    let to = right_half_of(&layout, b0);
    drag_floating_to(&mut layout, 1, to);
    assert!(current_join(&layout).is_some());

    layout.mark_interactive_move_cancelled();
    layout.interactive_move_end(&1);
    layout.verify_invariants();
    settle_floating_pair(&mut layout);

    assert!(window_is_floating(&layout, 1));
    assert!(window_is_floating(&layout, 2));
    assert_eq!(floating_partner(&layout, 1), None);
    assert_eq!(floating_partner(&layout, 2), None);
    let b = floating_rect(&layout, 2);
    assert_eq!(b.size, b0.size, "cancel leaves B untouched");
}

#[test]
fn floating_pickup_of_group_member_ungroups() {
    let mut layout = floating_pair_layout();
    form_pair(&mut layout);

    drag_floating_to(&mut layout, 1, Point::from((1000., 600.)));
    assert_eq!(floating_partner(&layout, 2), None);
    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert_eq!(floating_partner(&layout, 1), None);
}

#[test]
fn floating_group_third_via_bottom_of_one_side_splits_that_side() {
    let mut layout = floating_group_layout((500, 400));
    let b0 = form_three_split_left(&mut layout);

    assert_eq!(group_mates(&layout, 2), vec![1, 3]);
    let half_w = (b0.size.w - GROUP_GAP) / 2.;
    let half_h = (b0.size.h - GROUP_GAP) / 2.;
    let x = b0.loc.x;
    let y = b0.loc.y;
    let tl = rect_f(x, y, half_w, half_h);
    let bl = rect_f(x, y + half_h + GROUP_GAP, half_w, half_h);
    let r = rect_f(x + half_w + GROUP_GAP, y, half_w, b0.size.h);
    assert_rect_near(
        group_cell(&layout, 2),
        tl,
        "2 stays on top of the left side",
    );
    assert_rect_near(group_cell(&layout, 3), bl, "3 goes below 2");
    assert_rect_near(group_cell(&layout, 1), r, "1 keeps the full height");
    assert_rect_near(floating_rect(&layout, 2), tl, "2 tile");
    assert_rect_near(floating_rect(&layout, 3), bl, "3 tile");
    assert_rect_near(floating_rect(&layout, 1), r, "1 tile");
}

#[test]
fn floating_group_third_via_bottom_center_takes_full_width_strip() {
    let mut layout = floating_group_layout((500, 400));
    let b0 = form_pair(&mut layout);

    let to = view_point(
        &layout,
        b0.loc.x + b0.size.w / 2.,
        b0.loc.y + b0.size.h - 5.,
    );
    drag_floating_to(&mut layout, 3, to);
    let join = current_join(&layout).expect("bottom center is a zone");
    assert_eq!(join.op, FloatingJoinOp::BottomStrip);
    let half_w = (b0.size.w - GROUP_GAP) / 2.;
    let half_h = (b0.size.h - GROUP_GAP) / 2.;
    let strip = rect_f(b0.loc.x, b0.loc.y + half_h + GROUP_GAP, b0.size.w, half_h);
    let view_x = layout.active_workspace().unwrap().scroll_view_pos();
    let mut strip_view = strip;
    strip_view.loc.x -= view_x;
    assert_rect_near(join.preview, strip_view, "preview is the strip cell");

    layout.interactive_move_end(&3);
    layout.verify_invariants();
    settle_floating_pair(&mut layout);

    assert_eq!(group_mates(&layout, 3), vec![1, 2]);
    assert_rect_near(group_cell(&layout, 3), strip, "3 takes the bottom strip");
    assert_rect_near(
        group_cell(&layout, 2),
        rect_f(b0.loc.x, b0.loc.y, half_w, half_h),
        "2 above, left",
    );
    assert_rect_near(
        group_cell(&layout, 1),
        rect_f(b0.loc.x + half_w + GROUP_GAP, b0.loc.y, half_w, half_h),
        "1 above, right",
    );
    assert_rect_near(floating_rect(&layout, 3), strip, "3 tile");
}

#[test]
fn floating_group_fourth_makes_two_by_two_and_fifth_does_not_join() {
    let mut layout = floating_group_layout((500, 400));
    let b0 = form_two_by_two(&mut layout);

    let half_w = (b0.size.w - GROUP_GAP) / 2.;
    let half_h = (b0.size.h - GROUP_GAP) / 2.;
    let (x0, y0) = (b0.loc.x, b0.loc.y);
    let (x1, y1) = (x0 + half_w + GROUP_GAP, y0 + half_h + GROUP_GAP);
    for (id, cell) in [
        (2, rect_f(x0, y0, half_w, half_h)),
        (1, rect_f(x1, y0, half_w, half_h)),
        (3, rect_f(x0, y1, half_w, half_h)),
        (4, rect_f(x1, y1, half_w, half_h)),
    ] {
        assert_rect_near(group_cell(&layout, id), cell, &format!("cell of {id}"));
        assert_rect_near(floating_rect(&layout, id), cell, &format!("tile of {id}"));
    }
    assert_eq!(group_mates(&layout, 4), vec![1, 2, 3]);

    // A full group shows no zone and the drop floats where it lands.
    let to = view_point(&layout, x0 + half_w / 2., y0 + half_h / 2.);
    drag_floating_to(&mut layout, 5, to);
    // Let the pickup animation land so the drop happens under the pointer.
    check_ops_on_layout(&mut layout, [Op::CompleteAnimations]);
    assert!(current_join(&layout).is_none());
    let output = layout.outputs().next().cloned().unwrap();
    layout.update_render_elements(Some(&output));
    let mon = layout.monitor_for_output(&output).unwrap();
    assert!(mon.floating_join_preview.is_none());
    layout.interactive_move_end(&5);
    layout.verify_invariants();
    settle_floating_pair(&mut layout);

    assert!(group_mates(&layout, 5).is_empty());
    assert_eq!(group_mates(&layout, 4), vec![1, 2, 3]);
    let r5 = floating_rect(&layout, 5);
    let view_x = layout.active_workspace().unwrap().scroll_view_pos();
    assert!(
        (r5.loc.x - view_x - (to.x - 20.)).abs() < 1.5 && (r5.loc.y - (to.y - 10.)).abs() < 1.5,
        "5 floats where dropped: {r5:?} {to:?}"
    );
}

#[test]
fn floating_group_inner_resize_keeps_the_group_box() {
    let mut layout = floating_group_layout((500, 400));
    form_pair(&mut layout);
    let left0 = group_cell(&layout, 2);
    let right0 = group_cell(&layout, 1);
    let box0 = left0.merge(right0);

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 2,
                edges: ResizeEdge::RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 2,
                dx: 60.,
                dy: 0.,
            },
        ],
    );
    let mut left = left0;
    left.size.w += 60.;
    let mut right = right0;
    right.loc.x += 60.;
    right.size.w -= 60.;
    assert_rect_near(group_cell(&layout, 2), left, "2 grows");
    assert_rect_near(group_cell(&layout, 1), right, "1 gives the space");

    check_ops_on_layout(
        &mut layout,
        [Op::InteractiveResizeUpdate {
            window: 2,
            dx: -40.,
            dy: 0.,
        }],
    );
    let mut left = left0;
    left.size.w -= 40.;
    let mut right = right0;
    right.loc.x -= 40.;
    right.size.w += 40.;
    assert_rect_near(group_cell(&layout, 2), left, "2 shrinks");
    assert_rect_near(group_cell(&layout, 1), right, "1 takes the space");
    assert_rect_near(
        group_cell(&layout, 2).merge(group_cell(&layout, 1)),
        box0,
        "same box",
    );

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeUpdate {
                window: 2,
                dx: 60.,
                dy: 0.,
            },
            Op::InteractiveResizeEnd { window: 2 },
        ],
    );
    settle_floating_pair(&mut layout);
    let mut left = left0;
    left.size.w += 60.;
    let mut right = right0;
    right.loc.x += 60.;
    right.size.w -= 60.;
    assert_rect_near(floating_rect(&layout, 2), left, "2 tile grows");
    assert_rect_near(floating_rect(&layout, 1), right, "1 tile shrinks");

    // An edge on the group boundary scales the whole group.
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 1,
                edges: ResizeEdge::RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 1,
                dx: 80.,
                dy: 0.,
            },
            Op::InteractiveResizeEnd { window: 1 },
        ],
    );
    let bbox = group_cell(&layout, 2).merge(group_cell(&layout, 1));
    assert!((bbox.size.w - (box0.size.w + 80.)).abs() < 0.5, "{bbox:?}");
    assert!((bbox.loc.x - box0.loc.x).abs() < 0.5, "{bbox:?}");
    let ratio_before = left.size.w / right.size.w;
    let ratio_after = group_cell(&layout, 2).size.w / group_cell(&layout, 1).size.w;
    assert!((ratio_before - ratio_after).abs() < 0.05);
}

#[test]
fn group_member_half_keeps_the_box() {
    let mut layout = floating_group_layout((500, 400));
    form_pair(&mut layout);
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 2,
                edges: ResizeEdge::RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 2,
                dx: 100.,
                dy: 0.,
            },
            Op::InteractiveResizeEnd { window: 2 },
        ],
    );
    let box0 = group_cell(&layout, 2).merge(group_cell(&layout, 1));
    assert!(layout.group_member_half(&1));
    let l = group_cell(&layout, 2);
    let r = group_cell(&layout, 1);
    let half = (box0.size.w - GROUP_GAP) / 2.;
    assert!((r.size.w - half).abs() < 0.5, "{r:?}");
    assert!((l.size.w - half).abs() < 0.5, "{l:?}");
    assert_rect_near(l.merge(r), box0, "same box");
    assert!(!layout.group_member_half(&3));
}

#[test]
fn floating_group_leave_redistributes_and_dissolves() {
    let mut layout = floating_group_layout((500, 400));
    let b0 = form_two_by_two(&mut layout);
    let half_w = (b0.size.w - GROUP_GAP) / 2.;
    let half_h = (b0.size.h - GROUP_GAP) / 2.;
    let (x0, y0) = (b0.loc.x, b0.loc.y);
    let x1 = x0 + half_w + GROUP_GAP;
    let y1 = y0 + half_h + GROUP_GAP;

    // 4 → 3: 2 (top-left) leaves, 3 (bottom-left) grows up.
    drop_floating_at(&mut layout, 2, Point::from((1150., 300.)));
    assert!(group_mates(&layout, 2).is_empty());
    assert_eq!(group_mates(&layout, 3), vec![1, 4]);
    assert_rect_near(
        group_cell(&layout, 3),
        rect_f(x0, y0, half_w, b0.size.h),
        "3 grows up",
    );
    assert_rect_near(
        group_cell(&layout, 1),
        rect_f(x1, y0, half_w, half_h),
        "1 stays",
    );
    assert_rect_near(
        group_cell(&layout, 4),
        rect_f(x1, y1, half_w, half_h),
        "4 stays",
    );
    assert_rect_near(
        floating_rect(&layout, 3),
        rect_f(x0, y0, half_w, b0.size.h),
        "3 tile",
    );

    // 3 → 2: 4 (bottom-right) leaves, 1 grows down; side by side again.
    drop_floating_at(&mut layout, 4, Point::from((1100., 620.)));
    assert_eq!(group_mates(&layout, 3), vec![1]);
    assert_rect_near(
        group_cell(&layout, 1),
        rect_f(x1, y0, half_w, b0.size.h),
        "1 grows down",
    );
    assert_rect_near(
        group_cell(&layout, 3),
        rect_f(x0, y0, half_w, b0.size.h),
        "3 stays",
    );

    // 2 → 1: the group dissolves.
    // Away from the others (the group bar shifted cells, so the old spot now hits 4).
    drop_floating_at(&mut layout, 1, Point::from((50., 650.)));
    assert!(group_mates(&layout, 3).is_empty());
    assert!(group_mates(&layout, 1).is_empty());
    assert_rect_near(
        floating_rect(&layout, 3),
        rect_f(x0, y0, half_w, b0.size.h),
        "3 keeps its cell",
    );
}

#[test]
fn floating_group_three_to_two_lays_side_by_side() {
    let mut layout = floating_group_layout((500, 400));
    let b0 = form_pair(&mut layout);
    let to = view_point(
        &layout,
        b0.loc.x + b0.size.w / 2.,
        b0.loc.y + b0.size.h - 5.,
    );
    drop_floating_at(&mut layout, 3, to);
    assert_eq!(group_mates(&layout, 3), vec![1, 2]);

    // Top-left leaves: 1 and the strip are laid side by side in the old box.
    drop_floating_at(&mut layout, 2, Point::from((1150., 300.)));
    assert_eq!(group_mates(&layout, 3), vec![1]);
    let half_w = (b0.size.w - GROUP_GAP) / 2.;
    assert_rect_near(
        group_cell(&layout, 1),
        rect_f(b0.loc.x, b0.loc.y, half_w, b0.size.h),
        "1 left",
    );
    assert_rect_near(
        group_cell(&layout, 3),
        rect_f(b0.loc.x + half_w + GROUP_GAP, b0.loc.y, half_w, b0.size.h),
        "3 right",
    );
}

#[test]
fn floating_group_full_viewport_scales_group_and_keeps_camera() {
    let mut layout = floating_group_layout((500, 400));
    form_three_split_left(&mut layout);
    let before: Vec<_> = [2, 1, 3].map(|id| group_cell(&layout, id)).into();
    let view_before = layout.active_workspace().unwrap().scroll_view_pos();

    // 3 is active after its drop.
    check_ops_on_layout(&mut layout, [Op::MaximizeColumn]);
    settle_floating_pair(&mut layout);

    let ws = layout.active_workspace().unwrap();
    let band = ws.scrolling().working_area();
    let group_target = rect_f(
        band.loc.x + GROUP_GAP,
        band.loc.y + GROUP_GAP,
        band.size.w - 2. * GROUP_GAP,
        band.size.h - 2. * GROUP_GAP,
    );
    let target = rect_f(
        group_target.loc.x + GROUP_LEFT,
        band.loc.y + GROUP_GAP,
        band.size.w - 2. * GROUP_GAP - GROUP_LEFT,
        band.size.h - 2. * GROUP_GAP - GROUP_BELOW,
    );
    let after: Vec<_> = [2, 1, 3].map(|id| group_cell(&layout, id)).into();
    let bbox = after[1..].iter().fold(after[0], |acc, r| acc.merge(*r));
    assert_rect_near(bbox, target, "cells fill the tile band right of the bar");

    // Proportions: equal halves stay equal, gaps stay exact.
    assert!((after[0].size.w - after[1].size.w).abs() < 1.);
    assert!((after[0].size.h - after[2].size.h).abs() < 1.);
    assert!((after[1].loc.x - (after[0].loc.x + after[0].size.w) - GROUP_GAP).abs() < 1.);
    assert!((after[2].loc.y - (after[0].loc.y + after[0].size.h) - GROUP_GAP).abs() < 1.);
    let ratio_before = before[0].size.h / before[1].size.h;
    let ratio_after = after[0].size.h / after[1].size.h;
    assert!(
        (ratio_before - ratio_after).abs() < 0.02,
        "{ratio_before} {ratio_after}"
    );
    for id in [1, 2, 3] {
        assert_rect_near(
            floating_rect(&layout, id),
            group_cell(&layout, id),
            &format!("tile of {id}"),
        );
    }

    let view_x = ws.scroll_view_pos();
    assert!(
        (view_x - view_before).abs() < 1.,
        "floating groups are pinned to the viewport, the camera stays: {view_x} vs {view_before}"
    );
}

#[test]
fn floating_group_resize_clamps_to_max_and_min() {
    let mut layout = floating_group_layout((500, 400));
    form_pair(&mut layout);
    let limits = layout.active_workspace().unwrap().floating().cell_limits();

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 1,
                edges: ResizeEdge::BOTTOM_RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 1,
                dx: 5000.,
                dy: 5000.,
            },
        ],
    );
    let l = group_cell(&layout, 2);
    let r = group_cell(&layout, 1);
    let bbox = l.merge(r);
    assert!(
        (bbox.size.w - limits.max.w).abs() < 0.5,
        "{bbox:?} {limits:?}"
    );
    assert!(
        (bbox.size.h - limits.max.h).abs() < 0.5,
        "{bbox:?} {limits:?}"
    );
    check_ops_on_layout(&mut layout, [Op::InteractiveResizeEnd { window: 1 }]);

    // A narrow pair stops shrinking once members would go below their floors (or the
    // group min, whichever is larger relative to the current box).
    let mut layout = floating_group_layout((200, 300));
    form_pair(&mut layout);
    let before = group_cell(&layout, 2).merge(group_cell(&layout, 1));
    let left0 = group_cell(&layout, 2).size.w;
    let right0 = group_cell(&layout, 1).size.w;
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 1,
                edges: ResizeEdge::RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 1,
                dx: -1000.,
                dy: 0.,
            },
            Op::InteractiveResizeEnd { window: 1 },
        ],
    );
    let bbox = group_cell(&layout, 2).merge(group_cell(&layout, 1));
    let left = group_cell(&layout, 2).size.w;
    let right = group_cell(&layout, 1).size.w;
    assert!(bbox.size.w >= limits.min.w - 0.5, "{bbox:?} {limits:?}");
    assert!(
        bbox.size.w <= before.size.w + 0.5,
        "did not grow: {bbox:?} before={before:?}"
    );
    // Members do not shrink past their interactive floor (or past what they already were).
    assert!(left + 0.5 >= left0.min(96.), "{left} vs {left0}");
    assert!(right + 0.5 >= right0.min(96.), "{right} vs {right0}");
}

#[test]
fn floating_single_window_size_is_clamped_to_tile_band() {
    let mut layout = floating_group_layout((500, 400));
    let limits = layout.active_workspace().unwrap().floating().group_limits();

    check_ops_on_layout(
        &mut layout,
        [
            Op::SetWindowWidth {
                id: Some(2),
                change: SizeChange::SetFixed(5000),
            },
            Op::SetWindowHeight {
                id: Some(2),
                change: SizeChange::SetFixed(5000),
            },
        ],
    );
    settle_floating_pair(&mut layout);
    let r = floating_rect(&layout, 2);
    assert!(r.size.w <= limits.max.w + 0.5, "{r:?} {limits:?}");
    assert!(r.size.h <= limits.max.h + 0.5, "{r:?} {limits:?}");
    assert!(r.size.w >= limits.max.w - 1.5, "{r:?} {limits:?}");

    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 1,
                edges: ResizeEdge::BOTTOM_RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 1,
                dx: 5000.,
                dy: 5000.,
            },
            Op::InteractiveResizeEnd { window: 1 },
        ],
    );
    settle_floating_pair(&mut layout);
    let r = floating_rect(&layout, 1);
    assert!(r.size.w <= limits.max.w + 0.5, "{r:?} {limits:?}");
    assert!(r.size.h <= limits.max.h + 0.5, "{r:?} {limits:?}");
}

use crate::layout::group_chrome::{ball_locs, GroupBarHit};
use crate::layout::window_chrome::{ChromeAction, BALL};

/// Tiled windows 1..=n, each in its own column of fixed width `col_w`.
fn tiled_group_layout(n: usize, col_w: i32) -> Layout<TestWindow> {
    let mut options = Options::default();
    options.layout.gaps = GROUP_GAP;
    let mut ops = vec![Op::AddOutput(1)];
    for id in 1..=n {
        ops.push(Op::AddWindow {
            params: TestWindowParams::new(id),
        });
    }
    for id in 1..=n {
        ops.push(Op::Communicate(id));
        ops.push(Op::SetWindowWidth {
            id: Some(id),
            change: SizeChange::SetFixed(col_w),
        });
    }
    for id in 1..=n {
        ops.push(Op::Communicate(id));
    }
    ops.push(Op::CompleteAnimations);
    check_ops_with_options(options, ops)
}

fn settle_windows(layout: &mut Layout<TestWindow>, n: usize) {
    let mut ops: Vec<Op> = (1..=n).map(Op::Communicate).collect();
    ops.push(Op::CompleteAnimations);
    check_ops_on_layout(layout, ops);
}

fn first_output(layout: &Layout<TestWindow>) -> Output {
    layout.outputs().next().cloned().unwrap()
}

/// Tiled group cells of `id`'s group (content-relative), in tile order.
fn tiled_cells(layout: &Layout<TestWindow>, id: usize) -> Vec<(usize, Rectangle<f64, Logical>)> {
    layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .group_cells(&id)
}

fn tiled_group_column_span(layout: &Layout<TestWindow>, id: usize) -> (usize, f64, f64) {
    let ws = layout.active_workspace().unwrap();
    let col = ws.scrolling().column_idx_of(&id).unwrap();
    let (x, w) = ws.scrolling().column_strip_span(col);
    (col, x, w)
}

/// Output point on the body of the floating group bar of `id` (above the balls).
fn floating_bar_point(layout: &Layout<TestWindow>, id: usize) -> Point<f64, Logical> {
    let gb = layout
        .active_workspace()
        .unwrap()
        .floating_group_box(&id)
        .unwrap();
    view_point(
        layout,
        gb.loc.x + GROUP_BAR_W / 2.,
        gb.loc.y + WindowChrome::extra_height() + 4.,
    )
}

/// Output point on the body of the tiled group bar of `id` (above the balls).
fn tiled_bar_point(layout: &Layout<TestWindow>, id: usize) -> Point<f64, Logical> {
    let (_, x, _) = tiled_group_column_span(layout, id);
    let wa = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .working_area();
    view_point(
        layout,
        x + GROUP_BAR_W / 2.,
        wa.loc.y + GROUP_GAP + WindowChrome::extra_height() + 4.,
    )
}

#[test]
fn floating_group_can_rise_until_l_bottom_meets_band_top() {
    let mut layout = floating_group_layout((500, 400));
    form_pair(&mut layout);
    let band_top = layout
        .active_workspace()
        .unwrap()
        .floating()
        .title_bar_band_top();

    check_ops_on_layout(
        &mut layout,
        [Op::MoveFloatingWindow {
            id: Some(1),
            x: PositionChange::AdjustFixed(0.),
            y: PositionChange::AdjustFixed(-2000.),
            animate: false,
        }],
    );

    let gb = layout
        .active_workspace()
        .unwrap()
        .floating_group_box(&1)
        .unwrap();
    let expected_y = band_top + GROUP_BOTTOM_H - gb.size.h;
    assert!(
        (gb.loc.y - expected_y).abs() < 1.5,
        "group should rise until L bottom bar top meets band.top: y={} expected={} band_top={}",
        gb.loc.y,
        expected_y,
        band_top
    );
    assert!(
        gb.loc.y + 1e-6 < band_top,
        "group top must be allowed above band.top (was clamped before)"
    );
    assert!(
        ((gb.loc.y + gb.size.h - GROUP_BOTTOM_H) - band_top).abs() < 1.5,
        "L bottom bar top should sit on band.top"
    );
}

#[test]
fn floating_group_bar_geometry_and_ball_hits() {
    let mut layout = floating_group_layout((500, 400));
    let cells_box = form_pair(&mut layout);
    let output = first_output(&layout);

    let gb = layout
        .active_workspace()
        .unwrap()
        .floating_group_box(&1)
        .unwrap();
    assert_rect_near(
        gb,
        rect_f(
            cells_box.loc.x - GROUP_LEFT,
            cells_box.loc.y,
            cells_box.size.w + GROUP_LEFT,
            cells_box.size.h + GROUP_BELOW,
        ),
        "bar on the left, bottom bar below",
    );

    let p = floating_bar_point(&layout, 1);
    let (id, hit, floating) = layout.group_bar_hit_at(&output, p).expect("bar body");
    assert!(id == 1 || id == 2);
    assert_eq!(hit, GroupBarHit::Body);
    assert!(floating);

    let expected = [
        ChromeAction::Minimize,
        ChromeAction::Half,
        ChromeAction::FullViewport,
        ChromeAction::Fullscreen,
        ChromeAction::Close,
    ];
    let side = crate::layout::group_chrome::side_bar_rect(cells_box, GROUP_GAP);
    let bar = view_point(&layout, side.loc.x, side.loc.y);
    for (loc, action) in ball_locs(side.size.h).iter().zip(expected) {
        let p = bar + *loc + Point::from((BALL / 2., BALL / 2.));
        let (_, hit, _) = layout.group_bar_hit_at(&output, p).expect("ball");
        assert_eq!(hit, GroupBarHit::Ball(action));
    }

    // Right of the bar is the window, not the bar.
    let p = view_point(&layout, cells_box.loc.x + 5., cells_box.loc.y + 50.);
    assert!(layout.group_bar_hit_at(&output, p).is_none());
}

#[test]
fn floating_group_click_to_drop_creates_tiled_group_column() {
    let mut layout = floating_group_layout((500, 400));
    form_pair(&mut layout);
    let output = first_output(&layout);

    let p = floating_bar_point(&layout, 1);
    assert!(layout.group_drag_begin(&output, &1, p));
    assert!(layout.group_drag_is_tiling());
    layout.group_drag_motion(&output, Point::from((900., 300.)));
    layout.update_render_elements(Some(&output));
    let mon = layout.monitor_for_output(&output).unwrap();
    assert!(
        matches!(
            mon.insert_hint.as_ref().map(|h| h.position),
            Some(InsertPosition::NewColumn(_))
        ),
        "click-to-drop shows a new-column hint"
    );
    assert!(mon.floating_join_preview.is_none(), "no join preview");
    layout.group_drag_drop();
    layout.verify_invariants();
    settle_floating_pair(&mut layout);

    assert!(!window_is_floating(&layout, 1));
    assert!(!window_is_floating(&layout, 2));
    let ws = layout.active_workspace().unwrap();
    assert!(ws.scrolling().is_in_group(&1));
    assert!(ws.scrolling().is_in_group(&2));
    let cells = tiled_cells(&layout, 1);
    assert_eq!(cells.len(), 2);
    let union = cells[0].1.merge(cells[1].1);
    let (_, _, w) = tiled_group_column_span(&layout, 1);
    assert!(
        (w - (GROUP_LEFT + union.size.w)).abs() < 0.5,
        "{w} {union:?}"
    );
    let wa = ws.scrolling().working_area();
    assert!((union.size.h - (wa.size.h - 2. * GROUP_GAP - GROUP_BELOW)).abs() < 0.5);
}

#[test]
fn floating_group_held_drag_stays_floating() {
    let mut layout = floating_group_layout((500, 400));
    form_pair(&mut layout);
    let output = first_output(&layout);
    let gb0 = layout
        .active_workspace()
        .unwrap()
        .floating_group_box(&1)
        .unwrap();

    let p = floating_bar_point(&layout, 1);
    assert!(layout.group_drag_begin(&output, &1, p));
    layout.group_drag_set_held();
    layout.group_drag_motion(&output, p + Point::from((60., 40.)));
    layout.update_render_elements(Some(&output));
    let mon = layout.monitor_for_output(&output).unwrap();
    assert!(mon.insert_hint.is_none(), "held drag shows no tiling hint");
    layout.group_drag_drop();
    layout.verify_invariants();
    settle_floating_pair(&mut layout);

    assert!(window_is_floating(&layout, 1));
    assert_eq!(group_mates(&layout, 1), vec![2]);
    let gb = layout
        .active_workspace()
        .unwrap()
        .floating_group_box(&1)
        .unwrap();
    let mut expected = gb0;
    expected.loc += Point::from((60., 40.));
    assert_rect_near(gb, expected, "group box follows the pointer");
}

#[test]
fn group_drag_cancel_restores_floating_and_tiled() {
    let mut layout = floating_group_layout((500, 400));
    form_pair(&mut layout);
    let output = first_output(&layout);
    let before = [group_cell(&layout, 1), group_cell(&layout, 2)];

    let p = floating_bar_point(&layout, 1);
    assert!(layout.group_drag_begin(&output, &1, p));
    layout.group_drag_motion(&output, Point::from((900., 500.)));
    layout.group_drag_cancel();
    layout.verify_invariants();
    settle_floating_pair(&mut layout);
    assert_rect_near(group_cell(&layout, 1), before[0], "1 back");
    assert_rect_near(group_cell(&layout, 2), before[1], "2 back");
    assert!(!layout.group_drag_active());

    // Tiled: picking the bar floats the group; cancel puts the column back.
    let mut layout = tiled_group_layout(3, 200);
    assert!(layout.create_group_from_windows(&[1, 2]));
    settle_windows(&mut layout, 3);
    let (col, _, w) = tiled_group_column_span(&layout, 1);
    let output = first_output(&layout);
    let p = tiled_bar_point(&layout, 1);
    let (_, hit, floating) = layout.group_bar_hit_at(&output, p).expect("tiled bar");
    assert_eq!(hit, GroupBarHit::Body);
    assert!(!floating);
    assert!(layout.group_drag_begin(&output, &1, p));
    assert!(window_is_floating(&layout, 1));
    layout.group_drag_motion(&output, Point::from((700., 300.)));
    layout.group_drag_cancel();
    layout.verify_invariants();
    settle_windows(&mut layout, 3);
    assert!(!window_is_floating(&layout, 1));
    let (col2, _, w2) = tiled_group_column_span(&layout, 1);
    assert_eq!(col, col2);
    assert!((w - w2).abs() < 1.);
}

#[test]
fn tiled_group_remove_one_redistributes_and_last_becomes_normal_column() {
    let mut layout = tiled_group_layout(3, 200);
    assert!(layout.create_group_from_windows(&[1, 2, 3]));
    settle_windows(&mut layout, 3);
    let (_, _, w0) = tiled_group_column_span(&layout, 1);
    assert_eq!(tiled_cells(&layout, 1).len(), 3);

    check_ops_on_layout(&mut layout, [Op::CloseWindow(3)]);
    settle_windows(&mut layout, 2);
    let cells = tiled_cells(&layout, 1);
    assert_eq!(cells.len(), 2);
    let union = cells[0].1.merge(cells[1].1);
    assert!((GROUP_LEFT + union.size.w - w0).abs() < 1., "same width");
    let wa = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .working_area();
    assert!(
        (union.size.h - (wa.size.h - 2. * GROUP_GAP - GROUP_BELOW)).abs() < 1.,
        "fills height"
    );

    check_ops_on_layout(&mut layout, [Op::CloseWindow(2)]);
    settle_windows(&mut layout, 1);
    let ws = layout.active_workspace().unwrap();
    assert!(!ws.scrolling().is_in_group(&1));
    assert!(tiled_cells(&layout, 1).is_empty());
    let (_, _, w) = tiled_group_column_span(&layout, 1);
    assert!((w - w0).abs() < 1., "normal column keeps the group width");
}

#[test]
fn tiled_group_width_is_sum_capped_at_band() {
    let mut layout = tiled_group_layout(2, 300);
    let sizes: Vec<f64> = (1..=2)
        .map(|id| {
            layout
                .active_workspace()
                .unwrap()
                .tiles_with_render_positions()
                .find(|(t, _, _)| *t.window().id() == id)
                .unwrap()
                .0
                .tile_size()
                .w
        })
        .collect();
    assert!(layout.create_group_from_windows(&[1, 2]));
    settle_windows(&mut layout, 2);
    let (_, _, w) = tiled_group_column_span(&layout, 1);
    let expected = GROUP_LEFT + sizes[0] + GROUP_GAP + sizes[1];
    assert!((w - expected).abs() < 1., "{w} vs {expected}");

    let mut layout = tiled_group_layout(2, 1000);
    assert!(layout.create_group_from_windows(&[1, 2]));
    settle_windows(&mut layout, 2);
    let (_, _, w) = tiled_group_column_span(&layout, 1);
    let wa = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .working_area();
    assert!((w - (wa.size.w - 2. * GROUP_GAP)).abs() < 1., "capped: {w}");

    // Resizing one member against the other keeps the column width.
    let mut layout = tiled_group_layout(2, 300);
    assert!(layout.create_group_from_windows(&[1, 2]));
    settle_windows(&mut layout, 2);
    let (_, _, w0) = tiled_group_column_span(&layout, 1);
    let c0 = tiled_cells(&layout, 1);
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 1,
                edges: ResizeEdge::RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 1,
                dx: 50.,
                dy: 0.,
            },
            Op::InteractiveResizeEnd { window: 1 },
        ],
    );
    settle_windows(&mut layout, 2);
    let c1 = tiled_cells(&layout, 1);
    assert!((c1[0].1.size.w - (c0[0].1.size.w + 50.)).abs() < 0.5);
    assert!((c1[1].1.loc.x - (c0[1].1.loc.x + 50.)).abs() < 0.5);
    assert!((c1[1].1.size.w - (c0[1].1.size.w - 50.)).abs() < 0.5);
    let (_, _, w1) = tiled_group_column_span(&layout, 1);
    assert!((w1 - w0).abs() < 0.5, "{w1} vs {w0}");

    // The group's outer edge resizes the whole group.
    check_ops_on_layout(
        &mut layout,
        [
            Op::InteractiveResizeBegin {
                window: 2,
                edges: ResizeEdge::RIGHT,
            },
            Op::InteractiveResizeUpdate {
                window: 2,
                dx: 60.,
                dy: 0.,
            },
            Op::InteractiveResizeEnd { window: 2 },
        ],
    );
    settle_windows(&mut layout, 2);
    let (_, _, w1) = tiled_group_column_span(&layout, 1);
    assert!((w1 - (w0 + 60.)).abs() < 0.5, "{w1} vs {w0}");
    let c1 = tiled_cells(&layout, 1);

    // Half scales the cells proportionally.
    assert!(layout.group_bar_action(&1, ChromeAction::Half));
    settle_windows(&mut layout, 2);
    let (_, _, w) = tiled_group_column_span(&layout, 1);
    let half = (wa.size.w - GROUP_GAP) * 0.5 - GROUP_GAP;
    assert!((w - half).abs() < 1., "{w} vs {half}");
    let c2 = tiled_cells(&layout, 1);
    let r_before = c1[0].1.size.w / c1[1].1.size.w;
    let r_after = c2[0].1.size.w / c2[1].1.size.w;
    assert!((r_before - r_after).abs() < 0.02);
}

#[test]
fn tiled_group_column_is_gutter_only() {
    let mut layout = tiled_group_layout(3, 300);
    assert!(layout.create_group_from_windows(&[1, 2]));
    settle_windows(&mut layout, 3);
    let (_, x, w) = tiled_group_column_span(&layout, 1);
    let ws = layout.active_workspace().unwrap();
    let view_x = ws.scroll_view_pos();
    let center = Point::from((x + w / 2. - view_x, 300.));
    assert!(matches!(
        ws.scrolling().insert_position(center),
        InsertPosition::NewColumn(_)
    ));
}

#[test]
fn selection_group_auto_layouts() {
    for n in 2..=4 {
        let mut layout = tiled_group_layout(n, 200);
        let ids: Vec<usize> = (1..=n).collect();
        assert!(layout.create_group_from_windows(&ids));
        settle_windows(&mut layout, n);
        let cells: Vec<_> = tiled_cells(&layout, 1)
            .into_iter()
            .map(|(_, c)| c)
            .collect();
        assert_eq!(cells.len(), n);
        let wa = layout
            .active_workspace()
            .unwrap()
            .scrolling()
            .working_area();
        let max_h = wa.size.h - 2. * GROUP_GAP - GROUP_BELOW;
        let union = cells[1..].iter().fold(cells[0], |acc, r| acc.merge(*r));
        assert!((union.size.h - max_h).abs() < 1.);
        match n {
            2 => {
                assert!((cells[0].loc.y - cells[1].loc.y).abs() < 0.5);
                assert!((cells[0].size.h - max_h).abs() < 1.);
                assert!(
                    (cells[1].loc.x - (cells[0].loc.x + cells[0].size.w + GROUP_GAP)).abs() < 0.5
                );
            }
            3 => {
                assert!((cells[0].loc.y - cells[1].loc.y).abs() < 0.5);
                assert!(
                    (cells[2].size.w - union.size.w).abs() < 0.5,
                    "full-width strip"
                );
                assert!(cells[2].loc.y > cells[0].loc.y + cells[0].size.h);
            }
            _ => {
                assert!((cells[0].loc.y - cells[1].loc.y).abs() < 0.5);
                assert!((cells[2].loc.y - cells[3].loc.y).abs() < 0.5);
                assert!((cells[0].loc.x - cells[2].loc.x).abs() < 0.5);
                assert!((cells[1].loc.x - cells[3].loc.x).abs() < 0.5);
            }
        }
        let ws = layout.active_workspace().unwrap();
        assert_eq!(ws.scrolling().columns().count(), 1);
    }

    let mut layout = tiled_group_layout(5, 150);
    assert!(!layout.create_group_from_windows(&[1]));
    assert!(!layout.create_group_from_windows(&[1, 2, 3, 4, 5]));
    layout.verify_invariants();
    assert!(!layout.active_workspace().unwrap().is_group_member(&1));
}

#[test]
fn rubber_band_selection_creates_group_via_button() {
    let mut layout = tiled_group_layout(3, 200);
    let output = first_output(&layout);

    assert!(layout.selection_begin(&output, Point::from((2., 2.))));
    layout.selection_motion(Point::from((1270., 700.)));
    layout.update_render_elements(Some(&output));
    assert!(layout.selection_release(), "3 windows: button shown");
    assert!(layout.selection_awaiting_click());
    let view = layout.monitor_for_output(&output).unwrap().view_size();
    let button = crate::layout::selection::button_rect(Point::from((1270., 700.)), view);
    let click = button.loc + button.size.downscale(2.).to_point();
    assert!(layout.selection_click(click));
    layout.verify_invariants();
    settle_windows(&mut layout, 3);
    let ws = layout.active_workspace().unwrap();
    for id in 1..=3 {
        assert!(ws.scrolling().is_in_group(&id));
    }

    // One window selected: no button. Click elsewhere and Esc dismiss.
    let mut layout = tiled_group_layout(3, 200);
    let output = first_output(&layout);
    let x2 = layout
        .active_workspace()
        .unwrap()
        .tiles_with_render_positions()
        .find(|(t, _, _)| *t.window().id() == 2)
        .unwrap()
        .1
        .x;
    assert!(layout.selection_begin(&output, Point::from((x2 + 20., 2.))));
    layout.selection_motion(Point::from((x2 + 60., 40.)));

    assert!(!layout.selection_release());
    assert!(!layout.selection_active());

    assert!(layout.selection_begin(&output, Point::from((2., 2.))));
    layout.selection_motion(Point::from((1270., 700.)));
    assert!(layout.selection_release());
    assert!(!layout.selection_click(Point::from((5., 5.))));
    assert!(!layout.selection_active());

    assert!(layout.selection_begin(&output, Point::from((2., 2.))));
    layout.selection_motion(Point::from((1270., 700.)));
    layout.selection_cancel();
    assert!(!layout.selection_active());
    assert!(!layout.active_workspace().unwrap().is_group_member(&1));
}

/// Tile rect of `id`, workspace view (= output) coordinates.
fn tile_view_rect(layout: &Layout<TestWindow>, id: usize) -> Rectangle<f64, Logical> {
    let ws = layout.active_workspace().unwrap();
    let (tile, pos, _) = ws
        .tiles_with_render_positions()
        .find(|(t, _, _)| *t.window().id() == id)
        .unwrap();
    Rectangle::new(pos, tile.tile_size())
}

fn rect_center(r: Rectangle<f64, Logical>) -> Point<f64, Logical> {
    r.loc + r.size.downscale(2.).to_point()
}

fn is_grouped(layout: &Layout<TestWindow>, id: usize) -> bool {
    layout.active_workspace().unwrap().is_group_member(&id)
}

fn move_pointer(
    layout: &mut Layout<TestWindow>,
    id: usize,
    from: Point<f64, Logical>,
    to: Point<f64, Logical>,
) {
    check_ops_on_layout(
        layout,
        [Op::InteractiveMoveUpdate {
            window: id,
            dx: to.x - from.x,
            dy: to.y - from.y,
            output_idx: 1,
            px: to.x,
            py: to.y,
        }],
    );
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pickup {
    /// Click-to-drop press released without dragging.
    Click,
    /// Click-to-drop press that became a held drag before the window detached.
    Held,
    /// Click-to-drop press, detached while the button is down, then a held drag.
    PressThenHeld,
}

/// Click-to-drop pickup of `id` grabbed at its center, detached by a small motion. Returns the
/// pointer position.
fn pickup(layout: &mut Layout<TestWindow>, id: usize, how: Pickup) -> Point<f64, Logical> {
    let p = rect_center(tile_view_rect(layout, id));
    check_ops_on_layout(
        layout,
        [Op::InteractiveMoveBegin {
            window: id,
            output_idx: 1,
            px: p.x,
            py: p.y,
        }],
    );
    layout.interactive_move_click_to_drop(&id);
    match how {
        Pickup::Click => layout.interactive_move_click_released(&id),
        Pickup::Held => layout.interactive_move_held_drag(&id),
        Pickup::PressThenHeld => (),
    }
    layout.verify_invariants();
    let to = p + Point::from((4., 0.));
    move_pointer(layout, id, p, to);
    assert!(matches!(
        layout.interactive_move,
        Some(InteractiveMoveState::Moving(_))
    ));
    if how == Pickup::PressThenHeld {
        layout.interactive_move_held_drag(&id);
        layout.verify_invariants();
    }
    to
}

fn cancel_move(layout: &mut Layout<TestWindow>, id: usize) {
    layout.mark_interactive_move_cancelled();
    layout.interactive_move_end(&id);
    layout.verify_invariants();
}

/// Point in the right half of `id`'s tile, above the bottom bands.
fn right_half_point(layout: &Layout<TestWindow>, id: usize) -> Point<f64, Logical> {
    let r = tile_view_rect(layout, id);
    Point::from((r.loc.x + r.size.w * 0.75, r.loc.y + r.size.h * 0.4))
}

fn tiled_pair_layout() -> Layout<TestWindow> {
    let mut layout = tiled_group_layout(3, 200);
    assert!(layout.create_group_from_windows(&[1, 2]));
    settle_windows(&mut layout, 3);
    layout
}

/// Side and bottom bar rects expected around the cells box `cells`.
fn expected_group_l(cells: Rectangle<f64, Logical>) -> [Rectangle<f64, Logical>; 2] {
    let title = WindowChrome::extra_height();
    [
        rect_f(
            cells.loc.x - GROUP_LEFT,
            cells.loc.y + title,
            GROUP_BAR_W,
            cells.size.h - title + GROUP_BELOW,
        ),
        rect_f(
            cells.loc.x - GROUP_GAP,
            cells.loc.y + cells.size.h + GROUP_GAP,
            cells.size.w + GROUP_GAP,
            GROUP_BOTTOM_H,
        ),
    ]
}

#[test]
fn group_l_bars_frame_cells_tiled() {
    let mut layout = tiled_pair_layout();
    let output = first_output(&layout);
    layout.update_render_elements(Some(&output));
    let rects = layout.active_workspace().unwrap().group_chrome_rects();
    assert_eq!(rects.len(), 2);
    let cells = tile_view_rect(&layout, 1).merge(tile_view_rect(&layout, 2));
    let [side, bottom] = expected_group_l(cells);
    assert_rect_near(rects[0], side, "tiled side bar below the title bars");
    assert_rect_near(
        rects[1],
        bottom,
        "tiled bottom bar to the cells' right edge",
    );

    let wa = layout
        .active_workspace()
        .unwrap()
        .scrolling()
        .working_area();
    let rail = wa.loc.y + wa.size.h - GROUP_GAP;
    assert!(
        (bottom.loc.y + bottom.size.h - rail).abs() < 1.,
        "bottom bar ends at the tiled band bottom"
    );

    let (_, hit, floating) = layout
        .group_bar_hit_at(&output, rect_center(rects[1]))
        .expect("bottom bar grabs the group");
    assert_eq!(hit, GroupBarHit::Body);
    assert!(!floating);
}

#[test]
fn group_l_bars_frame_cells_floating() {
    let mut layout = floating_pair_layout();
    form_pair(&mut layout);
    let output = first_output(&layout);
    layout.update_render_elements(Some(&output));
    let ws = layout.active_workspace().unwrap();
    let rects = ws.group_chrome_rects();
    assert_eq!(rects.len(), 2);
    let gb = ws.floating_group_box(&1).unwrap();
    let cells = rect_f(
        gb.loc.x - ws.scroll_view_pos() + GROUP_LEFT,
        gb.loc.y,
        gb.size.w - GROUP_LEFT,
        gb.size.h - GROUP_BELOW,
    );
    let [side, bottom] = expected_group_l(cells);
    assert_rect_near(rects[0], side, "floating side bar below the title bars");
    assert_rect_near(rects[1], bottom, "floating bottom bar");

    let p = rect_center(rects[1]);
    let (_, hit, floating) = layout
        .group_bar_hit_at(&output, p)
        .expect("bottom bar grabs the group");
    assert_eq!(hit, GroupBarHit::Body);
    assert!(floating);
}

#[test]
fn drag_into_tiled_group_joins_up_to_four() {
    let mut layout = tiled_group_layout(5, 150);
    assert!(layout.create_group_from_windows(&[1, 2]));
    settle_windows(&mut layout, 5);
    let output = first_output(&layout);

    for (id, target) in [(3, 2), (4, 3)] {
        let from = pickup(&mut layout, id, Pickup::Click);
        let to = right_half_point(&layout, target);
        move_pointer(&mut layout, id, from, to);
        let join = current_join(&layout).expect("right half of a member is a zone");
        assert!(join.tiled);
        assert_eq!(join.window, target);
        layout.update_render_elements(Some(&output));
        let mon = layout.monitor_for_output(&output).unwrap();
        assert!(
            mon.insert_hint.is_none(),
            "no tiled insert hint over a zone"
        );
        assert!(mon.floating_join_preview.is_some(), "exact cell preview");
        assert!(layout
            .workspaces()
            .all(|(_, _, ws)| ws.live_reflow_gap().is_none_or(|g| g.origin)));
        layout.interactive_move_end(&id);
        layout.verify_invariants();
        settle_windows(&mut layout, 5);
        let cells = tiled_cells(&layout, 1);
        assert_eq!(cells.len(), id, "{cells:?}");
        assert!(cells.iter().any(|(c, _)| *c == id));
    }

    // Full group: no zones.
    let from = pickup(&mut layout, 5, Pickup::Click);
    let to = rect_center(tile_view_rect(&layout, 1));
    move_pointer(&mut layout, 5, from, to);
    assert!(current_join(&layout).is_none());
    cancel_move(&mut layout, 5);
    assert_eq!(tiled_cells(&layout, 1).len(), 4);
    assert!(!is_grouped(&layout, 5));
}

#[test]
fn floating_drag_into_tiled_group_joins_as_tiled_member() {
    let mut layout = tiled_pair_layout();
    layout.set_window_floating(Some(&3), true);
    settle_windows(&mut layout, 3);
    let from = rect_center(tile_view_rect(&layout, 3));
    check_ops_on_layout(
        &mut layout,
        [Op::InteractiveMoveBegin {
            window: 3,
            output_idx: 1,
            px: from.x,
            py: from.y,
        }],
    );
    move_pointer(&mut layout, 3, from, from + Point::from((4., 0.)));
    assert!(window_is_floating(&layout, 3));
    let to = right_half_point(&layout, 2);
    move_pointer(&mut layout, 3, from + Point::from((4., 0.)), to);
    let join = current_join(&layout).expect("floating drag over a tiled group");
    assert!(join.tiled);
    layout.interactive_move_end(&3);
    layout.verify_invariants();
    assert!(!window_is_floating(&layout, 3));
    assert_eq!(tiled_cells(&layout, 1).len(), 3);
}

#[test]
fn tiled_pair_click_pickup_dissolves_only_on_drop() {
    let mut layout = tiled_pair_layout();
    let output = first_output(&layout);
    layout.update_render_elements(Some(&output));
    let rects0 = layout.active_workspace().unwrap().group_chrome_rects();
    assert_eq!(rects0.len(), 2);
    let (_, _, w0) = tiled_group_column_span(&layout, 2);
    let from = pickup(&mut layout, 1, Pickup::Click);
    assert!(is_grouped(&layout, 2), "group kept while carrying");
    let (_, _, w) = tiled_group_column_span(&layout, 2);
    assert!((w - w0).abs() < 0.5, "same column width");
    layout.update_render_elements(Some(&output));
    let rects = layout.active_workspace().unwrap().group_chrome_rects();
    assert_eq!(rects.len(), 2);
    assert_rect_near(rects[0], rects0[0], "side bar kept for the whole group");
    assert_rect_near(rects[1], rects0[1], "bottom bar kept for the whole group");

    let to = rect_center(tile_view_rect(&layout, 3));
    move_pointer(&mut layout, 1, from, to);
    layout.interactive_move_end(&1);
    layout.verify_invariants();
    assert!(!is_grouped(&layout, 2), "dissolved on drop");
    assert!(!is_grouped(&layout, 1));
}

#[test]
fn tiled_pair_click_pickup_dropped_back_rejoins() {
    let mut layout = tiled_pair_layout();
    let from = pickup(&mut layout, 1, Pickup::Click);
    let to = right_half_point(&layout, 2);
    move_pointer(&mut layout, 1, from, to);
    let join = current_join(&layout).expect("own deferred group offers zones");
    assert_eq!(join.window, 2);
    layout.interactive_move_end(&1);
    layout.verify_invariants();
    let cells = tiled_cells(&layout, 2);
    assert_eq!(cells.len(), 2);
    assert!(cells.iter().any(|(c, _)| *c == 1));
}

#[test]
fn tiled_pair_held_pickup_dissolves_immediately() {
    for how in [Pickup::Held, Pickup::PressThenHeld] {
        let mut layout = tiled_pair_layout();
        pickup(&mut layout, 1, how);
        assert!(window_is_floating(&layout, 1));
        assert!(!is_grouped(&layout, 2), "dissolved at hold");
        cancel_move(&mut layout, 1);
        assert_eq!(tiled_cells(&layout, 2).len(), 2, "cancel re-joins");
    }
}

#[test]
fn floating_pair_held_pickup_dissolves_only_on_release() {
    let mut layout = floating_pair_layout();
    form_pair(&mut layout);
    let gb0 = layout
        .active_workspace()
        .unwrap()
        .floating_group_box(&2)
        .unwrap();
    for how in [Pickup::Held, Pickup::PressThenHeld] {
        let from = pickup(&mut layout, 1, how);
        assert!(window_is_floating(&layout, 1));
        assert!(is_grouped(&layout, 2), "group kept while dragging");
        let gb = layout
            .active_workspace()
            .unwrap()
            .floating_group_box(&2)
            .unwrap();
        assert_rect_near(gb, gb0, "group box kept");
        if how == Pickup::PressThenHeld {
            move_pointer(&mut layout, 1, from, Point::from((600., 600.)));
            layout.interactive_move_end(&1);
            layout.verify_invariants();
            assert!(!is_grouped(&layout, 2), "dissolved on release");
        } else {
            cancel_move(&mut layout, 1);
            assert_eq!(floating_partner(&layout, 1), Some(2), "cancel keeps it");
            assert_rect_near(
                layout
                    .active_workspace()
                    .unwrap()
                    .floating_group_box(&2)
                    .unwrap(),
                gb0,
                "restored box",
            );
            settle_floating_pair(&mut layout);
        }
    }
}

#[test]
fn floating_pair_click_pickup_dissolves_immediately() {
    let mut layout = floating_pair_layout();
    form_pair(&mut layout);
    pickup(&mut layout, 1, Pickup::Click);
    assert!(!window_is_floating(&layout, 1), "click pickup retiles");
    assert!(!is_grouped(&layout, 2), "dissolved at click");
    cancel_move(&mut layout, 1);
    assert_eq!(floating_partner(&layout, 1), Some(2), "cancel re-joins");
}

#[test]
fn three_member_groups_leave_immediately() {
    for how in [Pickup::Click, Pickup::Held] {
        let mut layout = tiled_group_layout(4, 200);
        assert!(layout.create_group_from_windows(&[1, 2, 3]));
        settle_windows(&mut layout, 4);
        pickup(&mut layout, 1, how);
        assert_eq!(tiled_cells(&layout, 2).len(), 2, "tiled: left at pickup");
        cancel_move(&mut layout, 1);
        assert_eq!(tiled_cells(&layout, 2).len(), 3, "tiled: cancel re-joins");

        let mut layout = floating_group_layout((500, 400));
        form_three_split_left(&mut layout);
        pickup(&mut layout, 1, how);
        assert_eq!(group_mates(&layout, 2).len(), 1, "floating: left at pickup");
        cancel_move(&mut layout, 1);
        assert_eq!(
            group_mates(&layout, 2).len(),
            2,
            "floating: cancel re-joins"
        );
    }
}

#[test]
fn esc_during_deferred_pickup_restores_group() {
    let mut layout = tiled_pair_layout();
    let cells0 = tiled_cells(&layout, 1);
    let from = pickup(&mut layout, 1, Pickup::Click);
    move_pointer(&mut layout, 1, from, Point::from((900., 300.)));
    cancel_move(&mut layout, 1);
    let cells = tiled_cells(&layout, 1);
    assert_eq!(cells.len(), 2);
    for ((a, ra), (b, rb)) in cells.iter().zip(&cells0) {
        assert_eq!(a, b);
        assert_rect_near(*ra, *rb, "same cell");
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: if std::env::var_os("RUN_SLOW_TESTS").is_none() {
            eprintln!("ignoring slow test");
            0
        } else {
            ProptestConfig::default().cases
        },
        ..ProptestConfig::default()
    })]

    #[test]
    fn random_operations_dont_panic(
        ops: Vec<Op>,
        layout_config in arbitrary_layout_part(),
    ) {
        // eprintln!("{ops:?}");
        let options = Options {
            layout: niri_config::Layout::from_part(&layout_config),
            ..Default::default()
        };

        check_ops_with_options(options, ops);
    }
}

#[test]
fn minimize_restores_tiled_column_and_floating_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::AddWindow {
            params: TestWindowParams::new(2),
        },
        Op::AddWindow {
            params: TestWindowParams::new(3),
        },
        Op::ToggleWindowFloating { id: Some(3) },
    ];
    let mut layout = check_ops(ops);
    let floating_before = layout
        .active_workspace()
        .unwrap()
        .floating()
        .tiles()
        .count();
    assert_eq!(floating_before, 1);

    check_ops_on_layout(&mut layout, [Op::MinimizeWindow(1), Op::MinimizeWindow(3)]);
    let ws = layout.active_workspace().unwrap();
    assert!(ws.is_minimized(&1));
    assert!(ws.is_minimized(&3));
    assert_eq!(ws.scrolling().columns().count(), 1);
    assert!(layout.focus().is_some_and(|w| *w.id() == 2));

    check_ops_on_layout(
        &mut layout,
        [Op::UnminimizeWindow(1), Op::UnminimizeWindow(3)],
    );
    let ws = layout.active_workspace().unwrap();
    assert!(!ws.is_minimized(&1));
    assert!(!ws.is_minimized(&3));
    assert!(
        ws.floating().has_window(&1),
        "minimized windows reopen floating"
    );
    assert!(ws.floating().has_window(&3));
    assert!(!ws.scrolling().columns().any(|c| c.contains(&1)));
    assert!(layout.focus().is_some_and(|w| *w.id() == 3));
}

#[test]
fn activate_tiling_sinks_floating_activate_floating_raises() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: Some(1) },
    ]);
    assert!(layout.active_workspace().unwrap().floating_render_on_top());
    check_ops_on_layout(&mut layout, [Op::FocusWindow(0)]);
    assert!(
        !layout.active_workspace().unwrap().floating_render_on_top(),
        "clicking tiling must put floating below"
    );
    check_ops_on_layout(&mut layout, [Op::FocusWindow(1)]);
    assert!(
        layout.active_workspace().unwrap().floating_render_on_top(),
        "clicking floating must raise the floating layer"
    );
}

#[test]
fn focus_follows_mouse_does_not_raise_sunk_floating() {
    let mut layout = check_ops([
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(0),
        },
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleWindowFloating { id: Some(1) },
        Op::FocusWindow(0),
    ]);
    assert!(
        !layout.active_workspace().unwrap().floating_render_on_top(),
        "precondition: floating is under tiling"
    );
    layout.activate_window_without_raising(&1);
    assert!(
        !layout.active_workspace().unwrap().floating_render_on_top(),
        "hover/FFM must not raise a sunk floating window"
    );
    assert!(
        layout.active_workspace().unwrap().floating_is_active(),
        "hover may still focus the floating window"
    );
    layout.activate_window(&1);
    assert!(
        layout.active_workspace().unwrap().floating_render_on_top(),
        "click must raise the floating layer"
    );
}

#[test]
fn show_desktop_hides_and_ends_on_new_window() {
    let ops = [
        Op::AddOutput(1),
        Op::AddWindow {
            params: TestWindowParams::new(1),
        },
        Op::ToggleShowDesktop,
    ];
    let mut layout = check_ops(ops);
    assert!(layout.active_workspace().unwrap().is_desktop_shown());
    assert!(layout.focus().is_none());

    check_ops_on_layout(&mut layout, [Op::ToggleShowDesktop]);
    assert!(!layout.active_workspace().unwrap().is_desktop_shown());
    assert!(layout.focus().is_some_and(|w| *w.id() == 1));

    check_ops_on_layout(
        &mut layout,
        [
            Op::ToggleShowDesktop,
            Op::AddWindow {
                params: TestWindowParams::new(2),
            },
        ],
    );
    assert!(!layout.active_workspace().unwrap().is_desktop_shown());
    assert!(layout.focus().is_some_and(|w| *w.id() == 2));
}
