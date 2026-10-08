use std::{cmp::max, rc::Rc, time::Duration};

use niri_config::{
    utils::MergeWith as _, CornerRadius, OutputName, PresetSize,
    Workspace as WorkspaceConfig,
};
use niri_ipc::{ColumnDisplay, PositionChange, SizeChange, WindowLayout};
use smithay::{
    backend::renderer::{
        element::{utils::CropRenderElement, Kind},
        gles::GlesRenderer,
    },
    desktop::{layer_map_for_output, Window},
    output::Output,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Physical, Point, Rectangle, Serial, Size, Transform},
    wayland::{compositor::with_states, shell::xdg::SurfaceCachedState},
};

use super::{
    floating::{FloatingSpace, FloatingSpaceRenderElement, MinimizedFrom},
    floating_group::{FloatingJoinHint, FloatingJoinOp, MAX_GROUP_MEMBERS},
    group_chrome::{auto_cells, auto_content_size, group_extra, GroupBarHit},
    scrolling::{
        Column, ColumnWidth, LiveReflowGap, ScrollDirection, ScrollingSpace,
        ScrollingSpaceRenderElement,
    },
    shadow::Shadow,
    tile::{self, OpenAnimKind, Tile, TileRenderSnapshot},
    window_chrome::ChromeAction,
    ActivateWindow, HitType, InsertPosition, InteractiveResizeData, LayoutElement, Options,
    RemovedTile, SizeFrac,
};
use crate::{
    animation::Clock,
    niri_render_elements,
    render_helpers::{
        renderer::NiriRenderer,
        shadow::ShadowRenderElement,
        solid_color::{SolidColorBuffer, SolidColorRenderElement},
        xray::{Xray, XrayPos},
        RenderCtx,
    },
    utils::{
        ensure_min_max_size, ensure_min_max_size_maybe_zero,
        id::IdCounter,
        output_size, send_scale_transform,
        transaction::{Transaction, TransactionBlocker},
        ResizeEdge,
    },
    window::ResolvedWindowRules,
};

#[derive(Debug)]
pub struct Workspace<W: LayoutElement> {
    /// The scrollable-tiling layout.
    scrolling: ScrollingSpace<W>,

    /// The floating layout.
    floating: FloatingSpace<W>,

    /// Whether the floating layout is active instead of the scrolling layout.
    floating_is_active: FloatingActive,

    /// The original output of this workspace.
    ///
    /// Most of the time this will be the workspace's current output, however, after an output
    /// disconnection, it may remain pointing to the disconnected output.
    pub(super) original_output: OutputId,

    /// Current output of this workspace.
    output: Option<Output>,

    /// Latest known output scale for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    scale: smithay::output::Scale,

    /// Latest known output transform for this workspace.
    ///
    /// This should be set from the current workspace output, or, if all outputs have been
    /// disconnected, preserved until a new output is connected.
    transform: Transform,

    /// Latest known view size for this workspace.
    ///
    /// This should be computed from the current workspace output size, or, if all outputs have
    /// been disconnected, preserved until a new output is connected.
    view_size: Size<f64, Logical>,

    /// Latest known working area for this workspace.
    ///
    /// Not rounded to physical pixels.
    ///
    /// This is similar to view size, but takes into account things like layer shell exclusive
    /// zones.
    working_area: Rectangle<f64, Logical>,

    /// This workspace's shadow in the overview.
    shadow: Shadow,

    /// This workspace's background.
    background_buffer: SolidColorBuffer,

    /// Clock for driving animations.
    pub(super) clock: Clock,

    /// Configurable properties of the layout as received from the parent monitor.
    pub(super) base_options: Rc<Options>,

    /// Configurable properties of the layout with logical sizes adjusted for the current `scale`.
    pub(super) options: Rc<Options>,

    /// Optional name of this workspace.
    pub(super) name: Option<String>,

    /// Layout config overrides for this workspace.
    layout_config: Option<niri_config::LayoutPart>,

    /// Unique ID of this workspace.
    id: WorkspaceId,

    /// Pointer position in this workspace's coordinates, set when mapping a new window.
    pub(super) pointer_pos: Option<Point<f64, Logical>>,

    /// Show desktop: every window is hidden (not rendered, hit or focused) while the layout
    /// stays untouched, so turning it off restores everything as it was.
    desktop_shown: bool,
}

#[derive(Debug, Clone)]
pub struct OutputId(String);

impl OutputId {
    pub fn matches(&self, output: &Output) -> bool {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        output_name.matches(&self.0)
    }
}

static WORKSPACE_ID_COUNTER: IdCounter = IdCounter::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

impl WorkspaceId {
    fn next() -> WorkspaceId {
        WorkspaceId(WORKSPACE_ID_COUNTER.next())
    }

    pub fn get(self) -> u64 {
        self.0
    }

    pub fn specific(id: u64) -> Self {
        Self(id)
    }
}

niri_render_elements! {
    WorkspaceRenderElement<R> => {
        Scrolling = CropRenderElement<ScrollingSpaceRenderElement<R>>,
        Floating = CropRenderElement<FloatingSpaceRenderElement<R>>,
    }
}

#[derive(Debug)]
pub(super) struct InteractiveResize<W: LayoutElement> {
    pub window: W::Id,
    pub original_window_size: Size<f64, Logical>,
    pub data: InteractiveResizeData,
}

/// Resolved width or height in logical pixels.
#[derive(Debug, Clone, Copy)]
pub enum ResolvedSize {
    /// Size of the tile including borders.
    Tile(f64),
    /// Size of the window excluding borders.
    Window(f64),
}

/// Whether the floating space is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatingActive {
    /// The scrolling space is active.
    No,
    /// The scrolling space is active, but the floating space should render on top, even if the
    /// active scrolling window is fullscreen.
    ///
    /// This is necessary for focus-follows-mouse that activates but doesn't raise the window to
    /// avoid being annoying.
    NoButRaised,
    /// The floating space is active and rendered on top.
    Yes,
    /// The floating space is focused, but still rendered under tiling.
    ///
    /// Used by focus-follows-mouse: hovering a floating window that sits below tiling must not
    /// raise it. Only a real click (`activate_window`) brings the floating layer back on top.
    YesButSunk,
}

/// Where to put a newly added window.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceAddWindowTarget<'a, W: LayoutElement> {
    /// No particular preference.
    #[default]
    Auto,
    /// As a new column at this index.
    NewColumnAt(usize),
    /// Next to this existing window.
    NextTo(&'a W::Id),
    /// As a new column after the last one.
    End,
}

impl OutputId {
    pub fn new(output: &Output) -> Self {
        let output_name = output.user_data().get::<OutputName>().unwrap();
        Self(output_name.format_make_model_serial_or_connector())
    }
}

impl FloatingActive {
    fn get(self) -> bool {
        matches!(self, Self::Yes | Self::YesButSunk)
    }
}

impl<W: LayoutElement> Workspace<W> {
    pub fn new(output: Output, clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config(output, None, clock, options)
    }

    pub fn new_with_config(
        output: Output,
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
        base_options: Rc<Options>,
    ) -> Self {
        let original_output = config
            .as_ref()
            .and_then(|c| c.open_on_output.clone())
            .map(OutputId)
            .unwrap_or(OutputId::new(&output));

        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = output.current_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = output_size(&output);
        let working_area = compute_working_area(&output);

        let scrolling = ScrollingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            scrolling,
            floating,
            floating_is_active: FloatingActive::No,
            original_output,
            scale,
            transform: output.current_transform(),
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            output: Some(output),
            clock,
            base_options,
            options,
            name: config.map(|c| c.name.0),
            layout_config,
            id: WorkspaceId::next(),
            pointer_pos: None,
            desktop_shown: false,
        }
    }

    pub fn new_with_config_no_outputs(
        mut config: Option<WorkspaceConfig>,
        clock: Clock,
        base_options: Rc<Options>,
    ) -> Self {
        let original_output = OutputId(
            config
                .as_ref()
                .and_then(|c| c.open_on_output.clone())
                .unwrap_or_default(),
        );

        let layout_config = config.as_mut().and_then(|c| c.layout.take().map(|x| x.0));

        let scale = smithay::output::Scale::Integer(1);
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(layout_config.as_ref())
                .adjusted_for_scale(scale.fractional_scale()),
        );

        let view_size = Size::from((1280., 720.));
        let working_area = Rectangle::from_size(Size::from((1280., 720.)));

        let scrolling = ScrollingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let floating = FloatingSpace::new(
            view_size,
            working_area,
            scale.fractional_scale(),
            clock.clone(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, view_size);

        Self {
            scrolling,
            floating,
            floating_is_active: FloatingActive::No,
            output: None,
            scale,
            transform: Transform::Normal,
            original_output,
            view_size,
            working_area,
            shadow: Shadow::new(shadow_config),
            background_buffer: SolidColorBuffer::new(view_size, options.layout.background_color),
            clock,
            base_options,
            options,
            name: config.map(|c| c.name.0),
            layout_config,
            id: WorkspaceId::next(),
            pointer_pos: None,
            desktop_shown: false,
        }
    }

    pub fn new_no_outputs(clock: Clock, options: Rc<Options>) -> Self {
        Self::new_with_config_no_outputs(None, clock, options)
    }

    pub fn id(&self) -> WorkspaceId {
        self.id
    }

    pub fn name(&self) -> Option<&String> {
        self.name.as_ref()
    }

    pub fn unname(&mut self) {
        self.name = None;
    }

    pub fn has_windows_or_name(&self) -> bool {
        self.has_windows() || self.name.is_some()
    }

    pub fn scale(&self) -> smithay::output::Scale {
        self.scale
    }

    pub fn advance_animations(&mut self) {
        self.scrolling.advance_animations();
        self.floating.advance_animations();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.scrolling.are_animations_ongoing() || self.floating.are_animations_ongoing()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.scrolling.are_transitions_ongoing() || self.floating.are_transitions_ongoing()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        self.scrolling
            .update_render_elements(is_active && !self.floating_is_active.get());

        let view_rect = Rectangle::from_size(self.view_size);
        // Floating under the tiling layer is dimmed (only when there is tiling to be under).
        let sunk = !self.floating_render_on_top() && !self.scrolling.is_empty();
        self.floating.update_render_elements(
            is_active && self.floating_is_active.get(),
            view_rect,
            0.,
            sunk,
        );

        self.shadow.update_render_elements(
            self.view_size,
            true,
            CornerRadius::default(),
            self.scale.fractional_scale(),
            1.,
        );
    }

    pub fn update_config(&mut self, base_options: Rc<Options>) {
        let scale = self.scale.fractional_scale();
        let options = Rc::new(
            Options::clone(&base_options)
                .with_merged_layout(self.layout_config.as_ref())
                .adjusted_for_scale(scale),
        );

        self.scrolling.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        self.floating.update_config(
            self.view_size,
            self.working_area,
            self.scale.fractional_scale(),
            options.clone(),
        );

        let shadow_config =
            compute_workspace_shadow_config(options.overview.workspace_shadow, self.view_size);
        self.shadow.update_config(shadow_config);

        self.background_buffer
            .set_color(options.layout.background_color);

        self.base_options = base_options;
        self.options = options;
    }

    pub fn update_layout_config(&mut self, layout_config: Option<niri_config::LayoutPart>) {
        if self.layout_config == layout_config {
            return;
        }

        self.layout_config = layout_config;
        self.update_config(self.base_options.clone());
    }

    pub fn update_shaders(&mut self) {
        self.scrolling.update_shaders();
        self.floating.update_shaders();
        self.shadow.update_shaders();
    }

    pub fn windows(&self) -> impl Iterator<Item = &W> + '_ {
        self.tiles().map(Tile::window)
    }

    pub fn windows_mut(&mut self) -> impl Iterator<Item = &mut W> + '_ {
        self.tiles_mut().map(Tile::window_mut)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles();
        let floating = self.floating.tiles();
        scrolling.chain(floating)
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        let scrolling = self.scrolling.tiles_mut();
        let floating = self.floating.tiles_mut();
        scrolling.chain(floating)
    }

    pub fn is_floating(&self, id: &W::Id) -> bool {
        self.floating.has_window(id)
    }

    pub fn current_output(&self) -> Option<&Output> {
        self.output.as_ref()
    }

    pub fn active_window(&self) -> Option<&W> {
        if self.floating_is_active.get() {
            self.floating.active_window()
        } else {
            self.scrolling.active_window()
        }
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        if self.floating_is_active.get() {
            self.floating.active_window_mut()
        } else {
            self.scrolling.active_window_mut()
        }
    }

    pub fn is_active_pending_fullscreen(&self) -> bool {
        self.scrolling.is_active_pending_fullscreen()
    }

    pub fn set_output(&mut self, output: Option<Output>) {
        if self.output == output {
            return;
        }

        if let Some(output) = self.output.take() {
            for win in self.windows() {
                win.output_leave(&output);
            }
        }

        self.output = output;

        if let Some(output) = &self.output {
            // Normalize original output: possibly replace connector with make/model/serial.
            if self.original_output.matches(output) {
                self.original_output = OutputId::new(output);
            }

            self.update_output_size();

            for win in self.windows() {
                self.enter_output_for_window(win);
            }
        }
    }

    fn enter_output_for_window(&self, window: &W) {
        if let Some(output) = &self.output {
            window.set_preferred_scale_transform(self.scale, self.transform);
            window.output_enter(output);
        }
    }

    pub fn update_output_size(&mut self) {
        let output = self.output.as_ref().unwrap();
        let scale = output.current_scale();
        let transform = output.current_transform();
        let view_size = output_size(output);
        let working_area = compute_working_area(output);
        self.set_view_size(scale, transform, view_size, working_area);
    }

    fn set_view_size(
        &mut self,
        scale: smithay::output::Scale,
        transform: Transform,
        size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
    ) {
        let scale_transform_changed = self.transform != transform
            || self.scale.integer_scale() != scale.integer_scale()
            || self.scale.fractional_scale() != scale.fractional_scale();
        if !scale_transform_changed && self.view_size == size && self.working_area == working_area {
            return;
        }

        let fractional_scale_changed = self.scale.fractional_scale() != scale.fractional_scale();

        self.scale = scale;
        self.transform = transform;
        self.view_size = size;
        self.working_area = working_area;

        if fractional_scale_changed {
            // Options need to be recomputed for the new scale.
            self.update_config(self.base_options.clone());
        } else {
            // Pass our existing options as is.
            self.scrolling.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );
            self.floating.update_config(
                size,
                working_area,
                scale.fractional_scale(),
                self.options.clone(),
            );

            let shadow_config =
                compute_workspace_shadow_config(self.options.overview.workspace_shadow, size);
            self.shadow.update_config(shadow_config);
        }

        self.background_buffer.resize(size);

        if scale_transform_changed {
            for window in self.windows() {
                window.set_preferred_scale_transform(self.scale, self.transform);
            }
        }
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn make_tile(&self, window: W) -> Tile<W> {
        Tile::new(
            window,
            self.view_size,
            self.scale.fractional_scale(),
            self.clock.clone(),
            self.options.clone(),
        )
    }

    pub fn add_tile(
        &mut self,
        mut tile: Tile<W>,
        target: WorkspaceAddWindowTarget<W>,
        activate: ActivateWindow,
        width: ColumnWidth,
        is_full_width: bool,
        is_floating: bool,
    ) {
        self.enter_output_for_window(tile.window());
        tile.restore_to_floating = is_floating;
        self.desktop_shown = false;

        match target {
            WorkspaceAddWindowTarget::Auto => {
                // Don't steal focus from an active fullscreen window.
                let activate = activate.map_smart(|| !self.is_active_pending_fullscreen());

                // If the tile is pending maximized or fullscreen, open it in the scrolling layout
                // where it can do that.
                if is_floating && tile.window().pending_sizing_mode().is_normal() {
                    self.floating.add_tile(tile, activate);

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else {
                    self.scrolling.add_tile_capped(
                        None,
                        tile,
                        activate,
                        width,
                        is_full_width,
                        None,
                        self.pointer_pos,
                    );

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                }
            }
            WorkspaceAddWindowTarget::NewColumnAt(col_idx) => {
                let activate = activate.map_smart(|| false);
                self.scrolling.add_tile_capped(
                    Some(col_idx),
                    tile,
                    activate,
                    width,
                    is_full_width,
                    None,
                    self.pointer_pos,
                );

                if activate {
                    self.floating_is_active = FloatingActive::No;
                }
            }
            WorkspaceAddWindowTarget::NextTo(next_to) => {
                let activate = activate.map_smart(|| self.active_window().unwrap().id() == next_to);

                let floating_has_window = self.floating.has_window(next_to);

                if is_floating && tile.window().pending_sizing_mode().is_normal() {
                    if floating_has_window {
                        self.floating.add_tile_above(next_to, tile, activate);
                    } else {
                        // FIXME: use static pos
                        let (next_to_tile, render_pos, _visible) = self
                            .scrolling
                            .tiles_with_render_positions()
                            .find(|(tile, _, _)| tile.window().id() == next_to)
                            .unwrap();

                        // Position the new tile in the center above the next_to tile. Think a
                        // dialog opening on top of a window.
                        let tile_size = tile.tile_size();
                        let pos = render_pos
                            + (next_to_tile.tile_size().to_point() - tile_size.to_point())
                                .downscale(2.);
                        let pos = self.floating.clamp_within_working_area(pos, tile_size);
                        let pos = self.floating.logical_to_size_frac(pos);
                        tile.floating_pos = Some(pos);

                        self.floating.add_tile(tile, activate);
                    }

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else if floating_has_window {
                    self.scrolling.add_tile_capped(
                        None,
                        tile,
                        activate,
                        width,
                        is_full_width,
                        None,
                        self.pointer_pos,
                    );

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                } else {
                    self.scrolling.add_tile_capped(
                        None,
                        tile,
                        activate,
                        width,
                        is_full_width,
                        Some(next_to),
                        self.pointer_pos,
                    );

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                }
            }
            WorkspaceAddWindowTarget::End => {
                let activate = activate.map_smart(|| !self.is_active_pending_fullscreen());

                if is_floating && tile.window().pending_sizing_mode().is_normal() {
                    self.floating.add_tile(tile, activate);

                    if activate || self.scrolling.is_empty() {
                        self.floating_is_active = FloatingActive::Yes;
                    }
                } else {
                    let idx = self.scrolling.columns().count();
                    self.scrolling.add_tile_capped(
                        Some(idx),
                        tile,
                        activate,
                        width,
                        is_full_width,
                        None,
                        self.pointer_pos,
                    );

                    if activate {
                        self.floating_is_active = FloatingActive::No;
                    }
                }
            }
        }
    }

    pub fn add_tile_to_column(
        &mut self,
        col_idx: usize,
        tile_idx: Option<usize>,
        tile: Tile<W>,
        activate: bool,
    ) {
        self.enter_output_for_window(tile.window());
        self.scrolling
            .add_tile_to_column(col_idx, tile_idx, tile, activate);

        if activate {
            self.floating_is_active = FloatingActive::No;
        }
    }

    pub fn add_column(&mut self, column: Column<W>, activate: bool) {
        for (tile, _) in column.tiles() {
            self.enter_output_for_window(tile.window());
        }

        self.scrolling.add_column(None, column, activate, None);

        if activate {
            self.floating_is_active = FloatingActive::No;
        }
    }

    fn update_focus_floating_tiling_after_removing(&mut self, removed_from_floating: bool) {
        if removed_from_floating {
            if self.floating.is_empty() {
                self.floating_is_active = FloatingActive::No;
            }
        } else {
            // Scrolling should remain focused if both are empty.
            if self.scrolling.is_empty() && !self.floating.is_empty() {
                self.floating_is_active = FloatingActive::Yes;
            }
        }
    }

    pub fn remove_tile(&mut self, id: &W::Id, transaction: Transaction) -> RemovedTile<W> {
        let mut from_floating = false;
        let removed = if self.floating.has_window(id) {
            from_floating = true;
            self.floating.remove_tile(id)
        } else {
            self.scrolling.remove_tile(id, transaction)
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        removed
    }

    pub(super) fn remove_tile_for_move(
        &mut self,
        id: &W::Id,
        transaction: Transaction,
    ) -> (RemovedTile<W>, Option<LiveReflowGap>) {
        let (removed, origin_gap) = self.scrolling.remove_tile_for_move(id, transaction);

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(false);

        (removed, origin_gap)
    }

    pub fn remove_active_tile(&mut self, transaction: Transaction) -> Option<RemovedTile<W>> {
        let from_floating = self.floating_is_active.get();
        let removed = if from_floating {
            self.floating.remove_active_tile()?
        } else {
            self.scrolling.remove_active_tile(transaction)?
        };

        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(removed)
    }

    pub fn remove_active_column(&mut self) -> Option<Column<W>> {
        let from_floating = self.floating_is_active.get();
        if from_floating {
            return None;
        }

        let column = self.scrolling.remove_active_column()?;

        if let Some(output) = &self.output {
            for (tile, _) in column.tiles() {
                tile.window().output_leave(output);
            }
        }

        self.update_focus_floating_tiling_after_removing(from_floating);

        Some(column)
    }

    pub fn resolve_default_width(
        &self,
        default_width: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_width {
            Some(Some(width)) => Some(width),
            Some(None) => None,
            None if is_floating => None,
            None => self.options.layout.default_column_width,
        }
    }

    pub fn resolve_default_height(
        &self,
        default_height: Option<Option<PresetSize>>,
        is_floating: bool,
    ) -> Option<PresetSize> {
        match default_height {
            Some(Some(height)) => Some(height),
            Some(None) => None,
            None if is_floating => None,
            // We don't have a global default at the moment.
            None => None,
        }
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
        (min_size, max_size): (Size<i32, Logical>, Size<i32, Logical>),
    ) -> Size<i32, Logical> {
        let mut size = if is_floating {
            self.floating.new_window_size(width, height, rules)
        } else {
            self.scrolling.new_window_size(width, height, rules)
        };

        // If the window has a fixed size, or we're picking some fixed size, apply min and max
        // size. This is to ensure that a fixed-size window rule works on open, while still
        // allowing the window freedom to pick its default size otherwise.
        let (min_size, max_size) = rules.apply_min_max_size(min_size, max_size);
        size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
        // For scrolling (where height is > 0) only ensure fixed height, since at runtime scrolling
        // will only honor fixed height currently.
        if min_size.h == max_size.h {
            size.h = ensure_min_max_size(size.h, min_size.h, max_size.h);
        } else if size.h > 0 {
            // Also always honor min height, scrolling always does.
            size.h = max(size.h, min_size.h);
        }

        size
    }

    pub fn configure_new_window(
        &self,
        window: &Window,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        is_floating: bool,
        rules: &ResolvedWindowRules,
    ) {
        window.with_surfaces(|surface, data| {
            send_scale_transform(surface, data, self.scale, self.transform);
        });

        let toplevel = window.toplevel().expect("no x11 support");
        let (min_size, max_size) = with_states(toplevel.wl_surface(), |state| {
            let mut guard = state.cached_state.get::<SurfaceCachedState>();
            let current = guard.current();
            (current.min_size, current.max_size)
        });
        toplevel.with_pending_state(|state| {
            if state.states.contains(xdg_toplevel::State::Fullscreen) {
                state.size = Some(self.view_size.to_i32_round());
            } else if state.states.contains(xdg_toplevel::State::Maximized) {
                state.size = Some(self.working_area.size.to_i32_round());
            } else {
                let size =
                    self.new_window_size(width, height, is_floating, rules, (min_size, max_size));
                state.size = Some(size);
            }

            if is_floating {
                state.bounds = Some(self.floating.new_window_toplevel_bounds(rules));
            } else {
                state.bounds = Some(self.scrolling.new_window_toplevel_bounds(rules));
            }
        });
    }

    pub(super) fn resolve_scrolling_width(
        &self,
        window: &W,
        width: Option<PresetSize>,
    ) -> ColumnWidth {
        let width = width.unwrap_or_else(|| PresetSize::Fixed(window.size().w));
        match width {
            PresetSize::Fixed(fixed) => {
                let mut fixed = f64::from(fixed);

                // Add border width since ColumnWidth includes borders.
                let rules = window.rules();
                let border = self.options.layout.border.merged_with(&rules.border);
                if !border.off {
                    fixed += border.width * 2.;
                }

                ColumnWidth::Fixed(fixed)
            }
            PresetSize::Proportion(prop) => ColumnWidth::Proportion(prop),
        }
    }

    pub fn focus_left(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_left()
        } else {
            self.scrolling.focus_left()
        }
    }

    pub fn focus_right(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_right()
        } else {
            self.scrolling.focus_right()
        }
    }

    pub fn focus_column_first(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_leftmost();
        } else {
            self.scrolling.focus_column_first();
        }
    }

    pub fn focus_column_last(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_rightmost();
        } else {
            self.scrolling.focus_column_last();
        }
    }

    pub fn focus_column_right_or_first(&mut self) {
        if !self.focus_right() {
            self.focus_column_first();
        }
    }

    pub fn focus_column_left_or_last(&mut self) {
        if !self.focus_left() {
            self.focus_column_last();
        }
    }

    pub fn focus_column(&mut self, index: usize) {
        if self.floating_is_active.get() {
            self.focus_tiling();
        }
        self.scrolling.focus_column(index);
    }

    pub fn focus_window_in_column(&mut self, index: u8) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.focus_window_in_column(index);
    }

    pub fn focus_down(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_down()
        } else {
            self.scrolling.focus_down()
        }
    }

    pub fn focus_up(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.focus_up()
        } else {
            self.scrolling.focus_up()
        }
    }

    pub fn focus_down_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            self.scrolling.focus_down_or_left();
        }
    }

    pub fn focus_down_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_down();
        } else {
            self.scrolling.focus_down_or_right();
        }
    }

    pub fn focus_up_or_left(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            self.scrolling.focus_up_or_left();
        }
    }

    pub fn focus_up_or_right(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_up();
        } else {
            self.scrolling.focus_up_or_right();
        }
    }

    pub fn focus_window_top(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_topmost();
        } else {
            self.scrolling.focus_top();
        }
    }

    pub fn focus_window_bottom(&mut self) {
        if self.floating_is_active.get() {
            self.floating.focus_bottommost();
        } else {
            self.scrolling.focus_bottom();
        }
    }

    pub fn focus_window_down_or_top(&mut self) {
        if !self.focus_down() {
            self.focus_window_top();
        }
    }

    pub fn focus_window_up_or_bottom(&mut self) {
        if !self.focus_up() {
            self.focus_window_bottom();
        }
    }

    pub fn move_left(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_left();
            true
        } else {
            self.scrolling.move_left()
        }
    }

    pub fn move_right(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_right();
            true
        } else {
            self.scrolling.move_right()
        }
    }

    pub fn move_column_to_first(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_first();
    }

    pub fn move_column_to_last(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_last();
    }

    pub fn move_column_to_index(&mut self, index: usize) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.move_column_to_index(index);
    }

    pub fn move_down(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_down();
            true
        } else {
            self.scrolling.move_down()
        }
    }

    pub fn move_up(&mut self) -> bool {
        if self.floating_is_active.get() {
            self.floating.move_up();
            true
        } else {
            self.scrolling.move_up()
        }
    }

    pub fn consume_or_expel_window_left(&mut self, window: Option<&W::Id>) {
        if window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            return;
        }
        self.scrolling.consume_or_expel_window_left(window);
    }

    pub fn consume_or_expel_window_right(&mut self, window: Option<&W::Id>) {
        if window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            return;
        }
        self.scrolling.consume_or_expel_window_right(window);
    }

    pub fn consume_into_column(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.consume_into_column();
    }

    pub fn expel_from_column(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.expel_from_column();
    }

    pub fn swap_window_in_direction(&mut self, direction: ScrollDirection) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.swap_window_in_direction(direction);
    }

    pub fn toggle_column_tabbed_display(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.toggle_column_tabbed_display();
    }

    pub fn set_column_display(&mut self, display: ColumnDisplay) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.set_column_display(display);
    }

    pub fn center_column(&mut self) {
        if self.floating_is_active.get() {
            self.floating.center_window(None, 0.);
        } else {
            self.scrolling.center_column();
        }
    }

    pub fn center_window(&mut self, id: Option<&W::Id>) {
        if id.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            self.floating.center_window(id, 0.);
        } else {
            self.scrolling.center_window(id);
        }
    }

    pub fn center_visible_columns(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.center_visible_columns();
    }

    pub fn toggle_width(&mut self, forwards: bool) {
        if self.floating_is_active.get() {
            self.floating.toggle_window_width(None, forwards);
        } else {
            self.scrolling.toggle_width(forwards);
        }
    }

    pub fn toggle_full_width(&mut self) {
        if self.floating_is_active.get() {
            let band = self.scrolling.working_area();
            self.floating.size_to_viewport(None, band);
            return;
        }
        self.scrolling.toggle_full_width();
    }

    /// Yellow ball / client maximize: working-area fill with no leftover gaps.
    pub fn toggle_full_width_gapped(&mut self) {
        if self.floating_is_active.get() {
            let band = self.scrolling.working_area();
            self.floating.size_to_viewport(None, band);
            return;
        }
        self.scrolling.toggle_full_width_gapped();
    }

    pub fn set_full_width_gapped(&mut self, full: bool) {
        if self.floating_is_active.get() {
            if full {
                let band = self.scrolling.working_area();
                self.floating.size_to_viewport(None, band);
            }
            return;
        }
        self.scrolling.set_full_width_gapped(full);
    }

    pub fn set_column_width(&mut self, change: SizeChange) {
        if self.floating_is_active.get() {
            self.floating.set_window_width(None, change, true);
        } else {
            self.scrolling.set_window_width(None, change);
        }
    }

    pub fn set_window_width(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            self.floating.set_window_width(window, change, true);
        } else {
            self.scrolling.set_window_width(window, change);
        }
    }

    pub fn set_window_height(&mut self, window: Option<&W::Id>, change: SizeChange) {
        if window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            self.floating.set_window_height(window, change, true);
        } else {
            self.scrolling.set_window_height(window, change);
        }
    }

    pub fn reset_window_height(&mut self, window: Option<&W::Id>) {
        if window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            return;
        }
        self.scrolling.reset_window_height(window);
    }

    pub fn toggle_window_width(&mut self, window: Option<&W::Id>, forwards: bool) {
        if window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            self.floating.toggle_window_width(window, forwards);
        } else {
            self.scrolling.toggle_window_width(window, forwards);
        }
    }

    pub fn toggle_window_height(&mut self, window: Option<&W::Id>, forwards: bool) {
        if window.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            self.floating.toggle_window_height(window, forwards);
        } else {
            self.scrolling.toggle_window_height(window, forwards);
        }
    }

    pub fn expand_column_to_available_width(&mut self) {
        if self.floating_is_active.get() {
            return;
        }
        self.scrolling.expand_column_to_available_width();
    }

    pub fn set_fullscreen(&mut self, window: &W::Id, is_fullscreen: bool) {
        if self.floating.has_window(window) {
            let already = self.floating.tiles().any(|t| {
                t.window().id() == window && t.window().pending_sizing_mode().is_fullscreen()
            });
            if is_fullscreen == already {
                return;
            }
            self.floating.size_to_output(Some(window), self.view_size);
            if is_fullscreen {
                self.floating_is_active = FloatingActive::Yes;
            }
            return;
        }

        let restore_to_floating = false;
        if !is_fullscreen {
            // The window is in the scrolling layout and we're requesting an unfullscreen. If it is
            // indeed fullscreen (i.e. this isn't a duplicate unfullscreen request), then we may
            // need to unfullscreen into floating.
            let col = self
                .scrolling
                .columns()
                .find(|col| col.contains(window))
                .unwrap();

            // When going from fullscreen to maximized, don't consider restore_to_floating yet.
            if col.is_pending_fullscreen() && !col.is_pending_maximized() {
                let (tile, _) = col
                    .tiles()
                    .find(|(tile, _)| tile.window().id() == window)
                    .unwrap();
                if tile.restore_to_floating {
                    // Unfullscreen and float in one call so it has a chance to notice and request a
                    // (0, 0) size, rather than the scrolling column size.
                    self.toggle_window_floating_inner(Some(window), false);
                    return;
                }
            }
        }

        let tile = self
            .scrolling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.scrolling.set_fullscreen(window, is_fullscreen);

        // When going from normal to fullscreen, remember if we should unfullscreen to floating.
        let tile = self
            .scrolling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && !tile.window().pending_sizing_mode().is_normal() {
            tile.restore_to_floating = restore_to_floating;
        }
    }

    pub fn toggle_fullscreen(&mut self, window: &W::Id) {
        if self.floating.has_window(window) {
            self.floating.size_to_output(Some(window), self.view_size);
            self.floating_is_active = FloatingActive::Yes;
            return;
        }
        let tile = self
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let current = tile.window().pending_sizing_mode().is_fullscreen();
        self.set_fullscreen(window, !current);
    }

    pub fn set_maximized(&mut self, window: &W::Id, maximize: bool) {
        let mut restore_to_floating = false;
        if self.floating.has_window(window) {
            if maximize {
                restore_to_floating = true;
                self.toggle_window_floating(Some(window));
            } else {
                // Floating windows are never maximized, so this is an unmaximize request for an
                // already unmaximized window.
                return;
            }
        } else if !maximize {
            // The window is in the scrolling layout and we're requesting to unmaximize. If it is
            // indeed maximized (i.e. this isn't a duplicate unmaximize request), then we may
            // need to unmaximize into floating.
            let tile = self
                .scrolling
                .tiles()
                .find(|tile| tile.window().id() == window)
                .unwrap();
            // The tile cannot unmaximize into fullscreen (pending_sizing_mode() will be fullscreen
            // in that case and not maximized), so this check works.
            if tile.window().pending_sizing_mode().is_maximized() && tile.restore_to_floating {
                // Unmaximize and float in one call so it has a chance to notice and request a
                // (0, 0) size, rather than the scrolling column size.
                self.toggle_window_floating_inner(Some(window), false);
                return;
            }
        }

        let tile = self
            .scrolling
            .tiles()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        let was_normal = tile.window().pending_sizing_mode().is_normal();

        self.scrolling.set_maximized(window, maximize);

        // When going from normal to maximized, remember if we should unmaximize to floating.
        let tile = self
            .scrolling
            .tiles_mut()
            .find(|tile| tile.window().id() == window)
            .unwrap();
        if was_normal && !tile.window().pending_sizing_mode().is_normal() {
            tile.restore_to_floating = restore_to_floating;
        }
    }

    pub fn toggle_maximized(&mut self, window: &W::Id) {
        let mut current = false;

        // We have to check the column property in case the window is in the scrolling layout and
        // both maximized and fullscreen. In this case, only the column knows whether it's
        // maximized.
        //
        // In the floating layout, windows cannot be maximized.
        if let Some(col) = self.scrolling.columns().find(|col| col.contains(window)) {
            current = col.is_pending_maximized();
        }

        self.set_maximized(window, !current);
    }

    pub fn toggle_window_floating(&mut self, id: Option<&W::Id>) {
        self.toggle_window_floating_inner(id, true);
    }

    /// `square`: a tile leaving tiling takes the 1:1 square instead of keeping its size
    /// (false when unfullscreening / unmaximizing back to floating).
    fn toggle_window_floating_inner(&mut self, id: Option<&W::Id>, square: bool) {
        let active_id = self.active_window().map(|win| win.id().clone());
        let target_is_active = id.is_none_or(|id| Some(id) == active_id.as_ref());
        let Some(id) = id.cloned().or(active_id) else {
            return;
        };

        let (_, render_pos, _) = self
            .tiles_with_render_positions()
            .find(|(tile, _, _)| *tile.window().id() == id)
            .unwrap();

        if self.floating.has_window(&id) {
            let mut removed = self.floating.remove_tile(&id);
            removed.tile.set_floating(false);
            // FIXME: compute closest pos?
            self.scrolling.add_tile_capped(
                None,
                removed.tile,
                target_is_active,
                removed.width,
                removed.is_full_width,
                None,
                self.pointer_pos,
            );
            if target_is_active {
                self.floating_is_active = FloatingActive::No;
            }
        } else {
            let mut removed = self.scrolling.remove_tile(&id, Transaction::new());
            removed.tile.stop_move_animations();
            removed.tile.set_floating(true);

            // Leaving tiling always shrinks to the 1:1 square, never the tiled size.
            let tiled_size = removed.tile.tile_size();
            let side = super::floating::FloatingSpace::<W>::square_side(self.scrolling.working_area());
            if square {
                removed.tile.floating_window_size = Some(removed.tile.square_window_size(side));
            }

            // Come up with a default floating position: the square centered on
            // where the tile was.
            let stored_or_default = self.floating.stored_or_default_tile_pos(&removed.tile);
            if stored_or_default.is_none() {
                let center = render_pos + tiled_size.to_point().downscale(2.);
                let size = if square {
                    Size::from((side, side))
                } else {
                    tiled_size
                };
                let pos = center - size.to_point().downscale(2.);
                let pos = self.floating.clamp_within_working_area(pos, size);
                let pos = self.floating.logical_to_size_frac(pos);
                removed.tile.floating_pos = Some(pos);
            }

            self.floating.add_tile(removed.tile, target_is_active);
            if target_is_active {
                self.floating_is_active = FloatingActive::Yes;
            }
        }

        let now_floating = self.floating.has_window(&id);
        let (tile, new_render_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| *tile.window().id() == id)
            .unwrap();

        // Flip title-bar fill immediately on click-toggle, not on next layout refresh.
        tile.set_floating(now_floating);
        tile.animate_move_from(render_pos - new_render_pos);
        if now_floating {
            tile.start_open_animation_kind(OpenAnimKind::FloatIn);
        }
    }

    /// Side of the 1:1 square used for detached / lime-ball floating windows.
    pub fn floating_square_side(&self) -> f64 {
        super::floating::FloatingSpace::<W>::square_side(self.scrolling.working_area())
    }

    /// Lime ball on a floating window: resize it to the 1:1 square.
    pub fn size_floating_square(&mut self, id: Option<&W::Id>) -> bool {
        let band = self.scrolling.working_area();
        self.floating.size_to_square(id, band)
    }

    pub fn set_window_floating(&mut self, id: Option<&W::Id>, floating: bool) {
        if id.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) == floating
        {
            return;
        }

        self.toggle_window_floating(id);
    }

    pub fn focus_floating(&mut self) {
        if !self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn focus_tiling(&mut self) {
        if self.floating_is_active.get() {
            self.switch_focus_floating_tiling();
        }
    }

    pub fn switch_focus_floating_tiling(&mut self) {
        if self.floating.is_empty() {
            // If floating is empty, keep focus on scrolling.
            return;
        } else if self.scrolling.is_empty() {
            // If floating isn't empty but scrolling is, keep focus on floating.
            return;
        }

        self.floating_is_active = if self.floating_is_active.get() {
            FloatingActive::No
        } else {
            FloatingActive::Yes
        };
    }

    pub fn move_floating_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        if id.map_or(self.floating_is_active.get(), |id| {
            self.floating.has_window(id)
        }) {
            self.floating.move_window(id, x, y, animate);
        } else {
            // If the target tile isn't floating, set its stored floating position.
            let tile = if let Some(id) = id {
                self.scrolling
                    .tiles_mut()
                    .find(|tile| tile.window().id() == id)
                    .unwrap()
            } else if let Some(tile) = self.scrolling.active_tile_mut() {
                tile
            } else {
                return;
            };

            let pos = self.floating.stored_or_default_tile_pos(tile);

            // If there's no stored floating position, we can only set both components at once, not
            // adjust.
            let pos = pos.or_else(|| {
                (matches!(
                    x,
                    PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                ) && matches!(
                    y,
                    PositionChange::SetFixed(_) | PositionChange::SetProportion(_)
                ))
                .then_some(Point::default())
            });

            let Some(mut pos) = pos else {
                return;
            };

            let working_area = self.floating.working_area();
            let available_width = working_area.size.w;
            let available_height = working_area.size.h;
            let working_area_loc = working_area.loc;

            const MAX_F: f64 = 10000.;

            match x {
                PositionChange::SetFixed(x) => pos.x = x + working_area_loc.x,
                PositionChange::SetProportion(prop) => {
                    let prop = (prop / 100.).clamp(0., MAX_F);
                    pos.x = available_width * prop + working_area_loc.x;
                }
                PositionChange::AdjustFixed(x) => pos.x += x,
                PositionChange::AdjustProportion(prop) => {
                    let current_prop = (pos.x - working_area_loc.x) / available_width.max(1.);
                    let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                    pos.x = available_width * prop + working_area_loc.x;
                }
            }
            match y {
                PositionChange::SetFixed(y) => pos.y = y + working_area_loc.y,
                PositionChange::SetProportion(prop) => {
                    let prop = (prop / 100.).clamp(0., MAX_F);
                    pos.y = available_height * prop + working_area_loc.y;
                }
                PositionChange::AdjustFixed(y) => pos.y += y,
                PositionChange::AdjustProportion(prop) => {
                    let current_prop = (pos.y - working_area_loc.y) / available_height.max(1.);
                    let prop = (current_prop + prop / 100.).clamp(0., MAX_F);
                    pos.y = available_height * prop + working_area_loc.y;
                }
            }

            let pos = self.floating.logical_to_size_frac(pos);
            tile.floating_pos = Some(pos);
        }
    }

    pub fn has_windows(&self) -> bool {
        self.windows().next().is_some()
    }

    pub fn has_window(&self, window: &W::Id) -> bool {
        self.windows().any(|win| win.id() == window)
    }

    pub fn find_wl_surface(&self, wl_surface: &WlSurface) -> Option<&W> {
        self.windows().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn find_wl_surface_mut(&mut self, wl_surface: &WlSurface) -> Option<&mut W> {
        self.windows_mut().find(|win| win.is_wl_surface(wl_surface))
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>, bool)> + '_ {
        let shown = !self.desktop_shown;
        let on_top = self.floating_render_on_top();
        let scrolling: Vec<_> = self
            .scrolling
            .tiles_with_render_positions()
            .map(|(tile, pos, visible)| (tile, pos, visible && shown))
            .collect();

        let float_visible = self.is_floating_visible();
        let floating: Vec<_> = self
            .floating
            .tiles_with_render_positions()
            .map(|(tile, pos)| (tile, pos, float_visible))
            .collect();

        // First in the iterator is topmost for chrome/hit tests.
        let mut out = Vec::with_capacity(scrolling.len() + floating.len());
        if on_top {
            out.extend(floating);
            out.extend(scrolling);
        } else {
            out.extend(scrolling);
            out.extend(floating);
        }
        out.into_iter()
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scrolling = self.scrolling.tiles_with_render_positions_mut(round);
        let floating = self.floating.tiles_with_render_positions_mut(round);
        floating.chain(scrolling)
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let scrolling = self.scrolling.tiles_with_ipc_layouts();
        let floating = self.floating.tiles_with_ipc_layouts();
        floating.chain(scrolling)
    }

    pub fn active_tile_visual_rectangle(&self) -> Option<Rectangle<f64, Logical>> {
        if self.floating_is_active.get() {
            self.floating.active_tile_visual_rectangle(0.)
        } else {
            self.scrolling.active_tile_visual_rectangle()
        }
    }

    pub fn popup_target_rect(&self, window: &W::Id) -> Option<Rectangle<f64, Logical>> {
        if self.floating.has_window(window) {
            self.floating.popup_target_rect(window, 0.)
        } else {
            self.scrolling.popup_target_rect(window)
        }
    }

    pub fn render_scrolling<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if self.desktop_shown {
            return;
        }
        let scrolling_focus_ring = focus_ring && !self.floating_is_active();
        let scale = self.scale.fractional_scale();
        let clip = self.scrolling_tiles_clip_bounds();

        // Normal tiles end exactly at the clip line, so only ones crossing it fade; maximized and
        // fullscreen tiles extend into the gap and stay unfaded.
        let clip_y = (!self.render_above_top_layer()).then(|| self.bottom_clip_y());
        for tile in self.scrolling.tiles() {
            tile.set_bottom_clip(clip_y.filter(|_| tile.sizing_mode().is_normal()));
        }
        self.scrolling
            .render(ctx, xray_pos, scrolling_focus_ring, &mut |elem| {
                if let Some(elem) = CropRenderElement::from_element(elem, scale, clip) {
                    push(elem.into());
                }
            });
        for tile in self.scrolling.tiles() {
            tile.set_bottom_clip(None);
        }
    }

    pub fn render_floating<R: NiriRenderer>(
        &self,
        ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(WorkspaceRenderElement<R>),
    ) {
        if !self.is_floating_visible() {
            return;
        }

        let view_rect = Rectangle::from_size(self.view_size);
        let floating_focus_ring = focus_ring && self.floating_is_active();
        let scale = self.scale.fractional_scale();
        let bottom_clip_y = self.bottom_clip_y();
        let top_clip_y = self.floating.title_bar_band_top();
        let clip = if self.floating.has_groups() {
            tile::vertical_clip_bounds_at(top_clip_y, bottom_clip_y, scale)
        } else {
            tile::bottom_clip_bounds_at(bottom_clip_y, scale)
        };
        self.floating.set_edge_clips(bottom_clip_y, top_clip_y);
        self.floating.render(
            ctx,
            xray_pos,
            view_rect,
            0.,
            floating_focus_ring,
            &mut |elem| {
                if let Some(elem) = CropRenderElement::from_element(elem, scale, clip) {
                    push(elem.into());
                }
            },
        );
        self.floating.clear_edge_clips();
    }

    /// Bottom clip line for floating and dragged windows: top of the bottom bar
    /// (scrolling working-area bottom). Tiled windows keep normal gaps above it.
    pub fn bottom_clip_y(&self) -> f64 {
        tile::bottom_rail_y(self.scrolling.working_area(), self.options.layout.gaps)
    }

    /// Bottom of the scrolling working area, where tiled windows are hard-cropped.
    fn tiled_bottom_y(&self) -> f64 {
        let wa = self.scrolling.working_area();
        wa.loc.y + wa.size.h
    }

    /// Fullscreen tiled windows cover the rail; everything else is cut at the
    /// scrolling working-area bottom (exclusive zone + struts).
    ///
    /// This stays below [`Self::bottom_clip_y`] so normal tile shadows and maximized tiles keep
    /// the gap; tiles crossing the clip line are cut there by their own bottom fade.
    fn scrolling_tiles_clip_bounds(&self) -> Rectangle<i32, Physical> {
        let scale = self.scale.fractional_scale();
        if self.render_above_top_layer() {
            Rectangle::new(
                Point::from((-i32::MAX / 2, -i32::MAX / 2)),
                Size::from((i32::MAX, i32::MAX)),
            )
        } else {
            let wa = self.scrolling.working_area();
            tile::bottom_clip_bounds_at(wa.loc.y + wa.size.h, scale)
        }
    }

    pub fn render_shadow<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(ShadowRenderElement),
    ) {
        self.shadow.render(renderer, Point::from((0., 0.)), push);
    }

    pub fn render_background(&self) -> SolidColorRenderElement {
        SolidColorRenderElement::from_buffer(
            &self.background_buffer,
            Point::new(0., 0.),
            1.,
            Kind::Unspecified,
        )
    }

    pub fn render_above_top_layer(&self) -> bool {
        if self.desktop_shown {
            return false;
        }
        if self.scrolling.render_above_top_layer() {
            return true;
        }
        // Orange fullscreen on a floating window also covers the overlay bars.
        self.floating_is_active.get()
            && self.floating.active_tile().is_some_and(|tile| {
                tile.window().pending_sizing_mode().is_fullscreen()
            })
    }

    pub fn is_floating_visible(&self) -> bool {
        if self.desktop_shown {
            return false;
        }
        // If the focus is on a fullscreen scrolling window, hide the floating windows.
        matches!(
            self.floating_is_active,
            FloatingActive::Yes | FloatingActive::NoButRaised
        ) || !self.render_above_top_layer()
    }

    pub fn store_unmap_snapshot_if_empty(
        &mut self,
        renderer: &mut GlesRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
        window: &W::Id,
    ) {
        let view_size = self.view_size();
        for (tile, tile_pos) in self.tiles_with_render_positions_mut(false) {
            if tile.window().id() == window {
                let view_pos = Point::from((-tile_pos.x, -tile_pos.y));
                let view_rect = Rectangle::new(view_pos, view_size);
                tile.update_render_elements(false, view_rect);
                let xray_pos = xray_pos.offset(tile_pos);
                tile.store_unmap_snapshot_if_empty(
                    renderer,
                    xray,
                    xray_has_blocked_out_layers,
                    xray_pos,
                );
                return;
            }
        }
    }

    pub fn clear_unmap_snapshot(&mut self, window: &W::Id) {
        for tile in self.tiles_mut() {
            if tile.window().id() == window {
                let _ = tile.take_unmap_snapshot();
                return;
            }
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if self.floating.has_window(window) {
            self.floating
                .start_close_animation_for_window(renderer, window, blocker);
        } else {
            self.scrolling
                .start_close_animation_for_window(renderer, window, blocker);
        }
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        self.floating.start_close_animation_for_tile(
            renderer, snapshot, tile_size, tile_pos, blocker, false,
        );
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        self.scrolling.start_open_animation(id) || self.floating.start_open_animation(id)
    }

    pub fn start_minimize_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        window: &W::Id,
    ) {
        if self.floating.is_minimized(window) {
            return;
        }
        if self.floating.has_window(window) {
            self.floating
                .start_minimize_animation_for_window(renderer, window);
        } else {
            self.scrolling
                .start_minimize_animation_for_window(renderer, window);
        }
    }

    pub fn window_under(&self, pos: Point<f64, Logical>) -> Option<(&W, HitType)> {
        // Hits must match the render crops: floating windows are cut at the clip line, tiled
        // ones at the working-area bottom (maximized tiles reach it). Fullscreen covers both.
        let clipped = !self.render_above_top_layer();
        if self.desktop_shown || (clipped && pos.y >= self.tiled_bottom_y()) {
            return None;
        }

        // Hit-test matches z-order: raised floating on top, otherwise tiling first.
        let clipped_float = clipped && pos.y >= self.bottom_clip_y();
        let hit_floating = || {
            if self.is_floating_visible() && !clipped_float {
                self.floating
                    .shown_tiles_with_render_positions()
                    .find_map(|(tile, tile_pos)| HitType::hit_tile(tile, tile_pos, pos))
            } else {
                None
            }
        };

        if self.floating_render_on_top() {
            if let Some(rv) = hit_floating() {
                return Some(rv);
            }
            self.scrolling.window_under(pos)
        } else {
            self.scrolling.window_under(pos).or_else(hit_floating)
        }
    }

    pub fn resize_edges_under(&self, pos: Point<f64, Logical>) -> Option<ResizeEdge> {
        self.tiles_with_render_positions()
            .find_map(|(tile, tile_pos, visible)| {
                // This logic should be consistent with window_under() in when it returns Some vs.
                // None.
                if !visible {
                    return None;
                }

                let pos_within_tile = pos - tile_pos;

                if tile.hit(pos_within_tile).is_some() {
                    let size = tile.tile_size().to_f64();

                    // Always pick the nearest corner so Mod+RMB can resize in both
                    // axes from anywhere on the window (Hyprland-like free resize).
                    let mut edges = ResizeEdge::empty();
                    if pos_within_tile.x < size.w / 2. {
                        edges |= ResizeEdge::LEFT;
                    } else {
                        edges |= ResizeEdge::RIGHT;
                    }
                    if pos_within_tile.y < size.h / 2. {
                        edges |= ResizeEdge::TOP;
                    } else {
                        edges |= ResizeEdge::BOTTOM;
                    }
                    return Some(edges);
                }

                None
            })
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        self.floating.descendants_added(id)
    }

    pub fn update_window(&mut self, window: &W::Id, serial: Option<Serial>) {
        if !self.floating.update_window(window, serial) {
            self.scrolling.update_window(window, serial);
        }
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        self.scrolling
            .refresh(is_active && !self.floating_is_active.get(), is_focused);
        self.floating
            .refresh(is_active && self.floating_is_active.get(), is_focused);
    }

    pub fn scroll_amount_to_activate(&self, window: &W::Id) -> f64 {
        if self.floating.has_window(window) {
            return 0.;
        }

        self.scrolling.scroll_amount_to_activate(window)
    }

    pub fn is_urgent(&self) -> bool {
        self.windows().any(|win| win.is_urgent())
    }

    pub fn activate_window(&mut self, window: &W::Id) -> bool {
        if self.has_window(window) {
            self.unminimize_window(window);
            self.desktop_shown = false;
        }
        if self.floating.activate_window(window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.scrolling.activate_window(window) {
            self.floating_is_active = FloatingActive::No;
            true
        } else {
            false
        }
    }

    pub fn activate_window_keep_view(&mut self, window: &W::Id) -> bool {
        if self.has_window(window) {
            self.unminimize_window(window);
            self.desktop_shown = false;
        }
        if self.floating.activate_window(window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.scrolling.activate_window_keep_view(window) {
            self.floating_is_active = FloatingActive::No;
            true
        } else {
            false
        }
    }

    /// Focus `window`; pull the view only if the column is not mostly on screen.
    pub fn activate_window_pull_if_needed(&mut self, window: &W::Id) -> bool {
        if self.has_window(window) {
            self.unminimize_window(window);
            self.desktop_shown = false;
        }
        if self.floating.activate_window(window) {
            self.floating_is_active = FloatingActive::Yes;
            true
        } else if self.scrolling.activate_window_pull_if_needed(window) {
            self.floating_is_active = FloatingActive::No;
            true
        } else {
            false
        }
    }

    pub fn is_user_view_pan_active(&self) -> bool {
        self.scrolling.is_user_view_pan_active()
    }

    pub fn scroll_view_by(&mut self, delta: f64) {
        self.scrolling.scroll_view_by(delta);
    }

    pub fn coast_view(&mut self, velocity: f64) {
        self.scrolling.coast_view(velocity);
    }

    pub fn impulse_view(&mut self, delta: f64) {
        self.scrolling.impulse_view(delta);
    }

    pub fn stop_view_pan(&mut self) -> bool {
        self.scrolling.stop_view_pan()
    }

    pub fn brake_view_pan(&mut self) -> bool {
        self.scrolling.brake_view_pan()
    }

    pub fn activate_window_without_raising(&mut self, window: &W::Id) -> bool {
        self.unminimize_window(window);
        if self.floating.activate_window_without_raising(window) {
            // Hover must not raise a sunk floating window; stay under tiling.
            self.floating_is_active = match self.floating_is_active {
                FloatingActive::No | FloatingActive::YesButSunk => FloatingActive::YesButSunk,
                FloatingActive::Yes | FloatingActive::NoButRaised => FloatingActive::Yes,
            };
            true
        } else if self.scrolling.activate_window_keep_view(window) {
            self.floating_is_active = match self.floating_is_active {
                FloatingActive::No | FloatingActive::YesButSunk => FloatingActive::No,
                FloatingActive::NoButRaised => FloatingActive::NoButRaised,
                FloatingActive::Yes => FloatingActive::NoButRaised,
            };
            true
        } else {
            false
        }
    }

    /// Tiled live-reflow dest for a scrolling-layout drag.
    ///
    /// Item 4: callers skip this while `InteractiveMoveData.is_floating`.
    /// item 5: floating-over-floating split
    pub(super) fn live_reflow_for_move(
        &self,
        pos: Point<f64, Logical>,
        dragged: Size<f64, Logical>,
        origin: Option<LiveReflowGap>,
    ) -> Option<LiveReflowGap> {
        self.scrolling.live_reflow_for_move(pos, dragged, origin)
    }

    pub(super) fn scrolling_insert_position(&self, pos: Point<f64, Logical>) -> InsertPosition {
        self.scrolling.insert_position(pos)
    }

    pub(super) fn window_insert_position(&self, window: &W::Id) -> Option<InsertPosition> {
        self.scrolling.window_insert_position(window)
    }

    pub(super) fn live_reflow_gap(&self) -> Option<LiveReflowGap> {
        self.scrolling.live_reflow_gap()
    }

    pub(super) fn set_live_reflow_gap(&mut self, gap: Option<LiveReflowGap>) {
        self.scrolling.set_live_reflow_gap(gap);
    }

    pub(super) fn set_drag_spread(&mut self, spread: bool) {
        self.scrolling.set_drag_spread(spread);
    }

    pub(super) fn set_drag_edge_limit(&mut self, width: f64) {
        self.scrolling.set_drag_edge_limit(width);
    }

    /// Item 4: clear tiled live-reflow dest and column-spread for a floating drag.
    /// Does not change gutter extra-width math (item 7).
    pub(super) fn suppress_tiled_drop_ui(&mut self) {
        self.scrolling.set_live_reflow_gap(None);
        self.scrolling.set_drag_spread(false);
    }

    pub(super) fn set_live_reflow(
        &mut self,
        dest: Option<LiveReflowGap>,
        origin: Option<LiveReflowGap>,
    ) {
        self.scrolling.set_live_reflow(dest, origin, true);
    }

    pub fn split_column(
        &mut self,
        col_idx: usize,
        insert_left: bool,
        half: f64,
        tile: Tile<W>,
        activate: bool,
    ) {
        self.enter_output_for_window(tile.window());
        self.scrolling
            .split_column(col_idx, insert_left, half, tile, activate);
        if activate {
            self.floating_is_active = FloatingActive::No;
        }
    }

    /// Tiling insert-hint / leftover shadow. Callers skip this for a floating
    /// interactive-move (item 4). item 5: floating-over-floating split
    pub(super) fn insert_hint_area(
        &self,
        position: InsertPosition,
    ) -> Option<Rectangle<f64, Logical>> {
        self.scrolling.insert_hint_area(position)
    }

    /// Topmost visible floating tile or group under `pos` (workspace view coords) for a
    /// floating drag: target id and the drop hint (the cell the dragged window would take).
    pub(super) fn floating_join_target(
        &self,
        pos: Point<f64, Logical>,
        incoming: crate::layout::floating_group::CellLimits,
    ) -> Option<(W::Id, FloatingJoinHint)> {
        if !self.is_floating_visible() {
            return None;
        }
        self.floating.join_target(pos, 0., incoming)
    }

    pub(super) fn join_floating_group(
        &mut self,
        incoming: &W::Id,
        target: &W::Id,
        op: FloatingJoinOp,
    ) -> bool {
        self.floating.join_group(incoming, target, op)
    }

    /// Join zone of a tiled group column under `pos` (workspace view coords). Floating windows
    /// covering `pos` block it.
    pub(super) fn tiled_group_join_target(
        &self,
        pos: Point<f64, Logical>,
        incoming: crate::layout::floating_group::CellLimits,
    ) -> Option<(W::Id, FloatingJoinHint)> {
        let clipped = !self.render_above_top_layer();
        if self.desktop_shown || (clipped && pos.y >= self.tiled_bottom_y()) {
            return None;
        }
        if self.is_floating_visible() && !(clipped && pos.y >= self.bottom_clip_y()) {
            let over_floating = self
                .floating
                .shown_tiles_with_render_positions()
                .any(|(tile, tile_pos)| HitType::hit_tile(tile, tile_pos, pos).is_some());
            if over_floating {
                return None;
            }
        }
        self.scrolling.group_join_target(pos, incoming)
    }

    /// Insert `tile` into `target`'s tiled group by `op`. Gives the tile back on failure.
    pub(super) fn join_tiled_group(
        &mut self,
        target: &W::Id,
        op: FloatingJoinOp,
        tile: Tile<W>,
    ) -> Result<(), Tile<W>> {
        self.enter_output_for_window(tile.window());
        self.scrolling.join_group(target, op, tile)?;
        self.floating_is_active = FloatingActive::No;
        Ok(())
    }

    /// `(tiled, member count)` of `id`'s group.
    pub(super) fn group_pickup_info(&self, id: &W::Id) -> Option<(bool, usize)> {
        let floating = self.floating.group_cells(id);
        if !floating.is_empty() {
            return Some((false, floating.len()));
        }
        let tiled = self.scrolling.group_cells(id);
        (!tiled.is_empty()).then_some((true, tiled.len()))
    }

    /// Cells of `id`'s group (floating: strip coordinates; tiled: content coordinates in tile
    /// order), for putting a picked member back.
    pub(super) fn group_snapshot(&self, id: &W::Id) -> Vec<(W::Id, Rectangle<f64, Logical>)> {
        let floating = self.floating.group_cells(id);
        if !floating.is_empty() {
            return floating;
        }
        self.scrolling.group_cells(id)
    }

    /// Pick `id` out of its group keeping its cell reserved (deferred ungroup).
    pub(super) fn remove_tile_reserving(&mut self, id: &W::Id) -> RemovedTile<W> {
        let from_floating = self.floating.has_window(id);
        let removed = if from_floating {
            self.floating.remove_tile_reserving(id)
        } else {
            self.scrolling.remove_group_member_reserving(id)
        };
        if let Some(output) = &self.output {
            removed.tile.window().output_leave(output);
        }
        self.update_focus_floating_tiling_after_removing(from_floating);
        removed
    }

    /// Free the reserved cell of `member`'s group (the deferred ungroup takes effect).
    pub(super) fn release_reserved_cell(&mut self, member: &W::Id) -> bool {
        if self.floating.has_window(member) {
            self.floating.release_reserved_cell(member)
        } else {
            self.scrolling.release_reserved_cell(member)
        }
    }

    /// Put a picked member back into its group with the snapshot `cells`.
    pub(super) fn restore_group_member(
        &mut self,
        tile: Tile<W>,
        tiled: bool,
        cells: &[(W::Id, Rectangle<f64, Logical>)],
    ) -> Result<(), Tile<W>> {
        self.enter_output_for_window(tile.window());
        if tiled {
            self.scrolling.restore_group_member(tile, cells)?;
            self.floating_is_active = FloatingActive::No;
        } else {
            self.floating.restore_group_member(tile, cells)?;
            self.floating_is_active = FloatingActive::Yes;
        }
        Ok(())
    }

    /// Side and bottom bar rects of the visible groups, workspace view coordinates.
    pub fn group_chrome_rects(&self) -> Vec<Rectangle<f64, Logical>> {
        let mut rects = self.scrolling.group_chrome_rects();
        if self.is_floating_visible() {
            rects.extend(self.floating.group_chrome_rects(0.));
        }
        rects
    }

    /// Tiling drop quadrants (split/stack halves) on a column.
    /// Item 4: not used while the dragged window is floating.
    /// item 5: floating-over-floating split
    pub(super) fn drop_zone_guide_rects(&self, col_idx: usize) -> Vec<Rectangle<f64, Logical>> {
        self.scrolling.drop_zone_guide_rects(col_idx)
    }

    pub fn view_offset_gesture_begin(&mut self, is_touchpad: bool) {
        self.scrolling.view_offset_gesture_begin(is_touchpad);
    }

    pub fn view_offset_gesture_update(
        &mut self,
        delta_x: f64,
        timestamp: Duration,
        is_touchpad: bool,
    ) -> Option<bool> {
        self.scrolling
            .view_offset_gesture_update(delta_x, timestamp, is_touchpad)
    }

    pub fn view_offset_gesture_end(&mut self, is_touchpad: Option<bool>) -> bool {
        self.scrolling.view_offset_gesture_end(is_touchpad)
    }

    pub fn dnd_scroll_gesture_begin(&mut self) {
        self.scrolling.dnd_scroll_gesture_begin();
    }

    pub fn dnd_scroll_gesture_scroll(&mut self, pos: Point<f64, Logical>, speed: f64) -> bool {
        let config = &self.options.gestures.dnd_edge_view_scroll;
        let trigger_width = config.trigger_width;

        // Physical left/right output edges, so both sides accelerate the same.
        let width = self.view_size.w;
        let x = pos.x.clamp(0., width);
        let trigger_width = trigger_width.clamp(0., width / 2.);

        let delta = if x < trigger_width {
            -(trigger_width - x)
        } else if width - x < trigger_width {
            trigger_width - (width - x)
        } else {
            0.
        };

        let delta = if trigger_width < 0.01 {
            // Sanity check for trigger-width 0 or small window sizes.
            0.
        } else {
            // Normalize to [0, 1].
            delta / trigger_width
        };
        let delta = delta * speed;

        self.scrolling.dnd_scroll_gesture_scroll(delta)
    }

    pub fn dnd_scroll_gesture_end(&mut self) {
        self.scrolling.dnd_scroll_gesture_end();
    }

    pub fn release_dnd_scroll_to_coast(&mut self) {
        self.scrolling.release_dnd_scroll_to_coast();
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.floating.has_window(&window) {
            self.floating.interactive_resize_begin(window, edges)
        } else {
            self.scrolling.interactive_resize_begin(window, edges)
        }
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        if self.floating.has_window(window) {
            self.floating.interactive_resize_update(window, delta)
        } else {
            self.scrolling.interactive_resize_update(window, delta)
        }
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        if let Some(window) = window {
            if self.floating.has_window(window) {
                self.floating.interactive_resize_end(Some(window));
            } else {
                self.scrolling.interactive_resize_end(Some(window));
            }
        } else {
            self.floating.interactive_resize_end(None);
            self.scrolling.interactive_resize_end(None);
        }
    }

    pub fn floating_render_on_top(&self) -> bool {
        matches!(
            self.floating_is_active,
            FloatingActive::Yes | FloatingActive::NoButRaised
        )
    }

    pub fn floating_is_active(&self) -> bool {
        self.floating_is_active.get()
    }

    pub fn is_minimized(&self, id: &W::Id) -> bool {
        self.floating.is_minimized(id)
    }

    /// Hide `id` from the layout, remembering where it goes back to. The window stays on this
    /// workspace (parked hidden in the floating space).
    pub fn minimize_window(&mut self, id: &W::Id) -> bool {
        if !self.has_window(id) || self.floating.is_minimized(id) {
            return false;
        }

        let from_floating = self.floating.has_window(id);
        let (tile, from) = if from_floating {
            let removed = self.floating.remove_tile(id);
            (removed.tile, MinimizedFrom::Floating)
        } else {
            let column_idx = self.scrolling.column_idx_of(id).unwrap();
            let col = self.scrolling.columns().nth(column_idx).unwrap();
            let tile_idx = (col.tiles().count() > 1)
                .then(|| col.tiles().position(|(t, _)| t.window().id() == id))
                .flatten();
            let removed = self.scrolling.remove_tile(id, Transaction::new());
            let from = MinimizedFrom::Tiled {
                column_idx,
                tile_idx,
                width: removed.width,
                is_full_width: removed.is_full_width,
                floating_window_size: removed.tile.floating_window_size,
                floating_pos: removed.tile.floating_pos,
            };
            (removed.tile, from)
        };

        let mut tile = tile;
        tile.stop_move_animations();
        let _ = tile.take_unmap_snapshot();
        self.floating.add_minimized_tile(tile, from);
        self.update_focus_floating_tiling_after_removing(from_floating);
        true
    }

    /// Put minimized `id` back as floating and activate it. Ends show desktop.
    pub fn unminimize_window(&mut self, id: &W::Id) -> bool {
        let Some((removed, _from)) = self.floating.take_minimized(id) else {
            return false;
        };
        self.desktop_shown = false;

        let mut tile = removed.tile;
        tile.set_floating(true);
        self.floating.add_tile(tile, true);
        if let Some(t) = self.floating.tiles_mut().find(|t| t.window().id() == id) {
            t.start_open_animation_kind(OpenAnimKind::Unminimize);
        }
        self.floating_is_active = FloatingActive::Yes;
        true
    }

    pub fn is_desktop_shown(&self) -> bool {
        self.desktop_shown
    }

    pub fn toggle_show_desktop(&mut self) {
        self.desktop_shown = !self.desktop_shown;
    }

    pub fn floating_logical_to_size_frac(
        &self,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        self.floating.logical_to_size_frac(logical_pos)
    }

    pub fn scroll_view_pos(&self) -> f64 {
        self.scrolling.view_pos()
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    /// Tiled working area: exclusive zone plus struts (the clip line for the bottom rail).
    pub fn tiled_working_area(&self) -> Rectangle<f64, Logical> {
        self.scrolling.working_area()
    }

    /// Group side bar under `pos` (workspace view coordinates): a member id, what was hit, and
    /// whether the group is floating. Floating windows above a tiled bar block it.
    pub fn group_bar_under(&self, pos: Point<f64, Logical>) -> Option<(W::Id, GroupBarHit, bool)> {
        let clipped = !self.render_above_top_layer();
        if self.desktop_shown || (clipped && pos.y >= self.tiled_bottom_y()) {
            return None;
        }
        if self.is_floating_visible() && !(clipped && pos.y >= self.bottom_clip_y()) {
            if let Some((id, hit)) = self.floating.group_bar_under(pos, 0.) {
                return Some((id, hit, true));
            }
            let over_floating = self
                .floating
                .shown_tiles_with_render_positions()
                .any(|(tile, tile_pos)| HitType::hit_tile(tile, tile_pos, pos).is_some());
            if over_floating {
                return None;
            }
        }
        self.scrolling
            .group_bar_under(pos)
            .map(|(id, hit)| (id, hit, false))
    }

    /// Update hovered group bar balls. Returns true if anything changed.
    pub fn update_group_bar_hover(&mut self, pos: Option<Point<f64, Logical>>) -> bool {
        let hit = pos.and_then(|p| self.group_bar_under(p).map(|(_, _, f)| (p, f)));
        let floating_pos = hit.filter(|(_, f)| *f).map(|(p, _)| p);
        let tiled_pos = hit.filter(|(_, f)| !*f).map(|(p, _)| p);
        let mut changed = self.floating.update_group_bar_hover(floating_pos, 0.);
        changed |= self.scrolling.update_group_bar_hover(tiled_pos);
        changed
    }

    /// Whether `id` belongs to a floating or tiled group.
    pub fn is_group_member(&self, id: &W::Id) -> bool {
        self.floating.group_id_of(id).is_some() || self.scrolling.is_in_group(id)
    }

    /// Whether `id` belongs to a floating group.
    pub fn is_floating_group_member(&self, id: &W::Id) -> bool {
        self.floating.group_id_of(id).is_some()
    }

    /// All members of `id`'s group (floating or tiled).
    pub fn group_member_ids(&self, id: &W::Id) -> Vec<W::Id> {
        let floating = self.floating.group_cells(id);
        let cells = if floating.is_empty() {
            self.scrolling.group_cells(id)
        } else {
            floating
        };
        cells.into_iter().map(|(id, _)| id).collect()
    }

    /// Column index of `id`'s tiled group.
    pub fn tiled_group_column(&self, id: &W::Id) -> Option<usize> {
        self.scrolling
            .is_in_group(id)
            .then(|| self.scrolling.column_idx_of(id))
            .flatten()
    }

    /// Column index a group dropped with its pointer at strip `x` is inserted at.
    pub fn group_insert_idx(&self, x: f64) -> usize {
        self.scrolling.insert_idx_for_strip_x(x)
    }

    /// Floating group box (bar included) of `id`'s group, view coordinates.
    pub fn floating_group_box(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        self.floating.group_box_of(id)
    }

    /// Floating group cells of `id`'s group, view coordinates.
    pub fn floating_group_cells(&self, id: &W::Id) -> Vec<(W::Id, Rectangle<f64, Logical>)> {
        self.floating.group_cells(id)
    }

    pub fn set_floating_group_cells(
        &mut self,
        cells: &[(W::Id, Rectangle<f64, Logical>)],
        animate: bool,
    ) {
        self.floating.set_group_cells(cells, animate);
    }

    /// Move `id`'s floating group so its box (bar included) starts at `loc` (clamped).
    pub fn move_floating_group_to(&mut self, id: &W::Id, loc: Point<f64, Logical>, animate: bool) {
        self.floating.move_group_box_to(id, loc, animate);
    }

    /// Render positions of `ids`, for animating them after a group transition.
    fn render_positions_of(&self, ids: &[W::Id]) -> Vec<(W::Id, Point<f64, Logical>)> {
        self.tiles_with_render_positions()
            .filter(|(tile, _, _)| ids.contains(tile.window().id()))
            .map(|(tile, pos, _)| (tile.window().id().clone(), pos))
            .collect()
    }

    fn animate_from_render_positions(&mut self, before: &[(W::Id, Point<f64, Logical>)]) {
        for (tile, pos) in self.tiles_with_render_positions_mut(false) {
            if let Some((_, old)) = before.iter().find(|(id, _)| id == tile.window().id()) {
                tile.animate_move_from(*old - pos);
            }
        }
    }

    /// Lime Half on a group member's own title bar: half the group inside the same box.
    pub fn group_member_half(&mut self, id: &W::Id) -> bool {
        if self.floating.group_id_of(id).is_some() {
            self.floating.group_member_half(id)
        } else {
            self.scrolling.group_member_half(id)
        }
    }

    /// Group actions from the side bar balls that resize the group. Minimize and Close are
    /// per-window and handled by the caller.
    pub fn group_resize_action(&mut self, id: &W::Id, action: ChromeAction) -> bool {
        let band = self.scrolling.working_area();
        if self.floating.group_id_of(id).is_some() {
            let span = match action {
                ChromeAction::Half => self.floating.group_half(id, band),
                ChromeAction::FullViewport => self.floating.size_to_viewport(Some(id), band),
                ChromeAction::Fullscreen => self.floating.group_fullscreen_toggle(id, band),
                ChromeAction::Minimize | ChromeAction::Close => None,
            };
            return span.is_some();
        }
        if !self.scrolling.is_in_group(id) {
            return false;
        }
        match action {
            ChromeAction::Half => {
                self.scrolling
                    .set_group_width(id, ColumnWidth::Proportion(0.5), false);
            }
            ChromeAction::FullViewport => {
                self.scrolling
                    .set_group_width(id, ColumnWidth::Proportion(1.), true);
            }
            ChromeAction::Fullscreen => self.scrolling.toggle_group_fullscreen(id),
            ChromeAction::Minimize | ChromeAction::Close => return false,
        }
        if let Some(col) = self.scrolling.column_idx_of(id) {
            let (x, w) = self.scrolling.column_strip_span(col);
            self.scrolling.animate_view_to_center_strip(x, w);
        }
        true
    }

    /// Turn `id`'s tiled group column into a floating group at the same place.
    pub fn float_tiled_group(&mut self, id: &W::Id) -> bool {
        let Some(col) = self.scrolling.column_idx_of(id) else {
            return false;
        };
        if !self.scrolling.is_in_group(id) {
            return false;
        }
        let ids = self.group_member_ids(id);
        let before = self.render_positions_of(&ids);
        let mut members = self.scrolling.take_group_column(col);
        let view_x = self.scrolling.view_pos();
        for (tile, cell) in &mut members {
            tile.stop_move_animations();
            cell.loc.x -= view_x;
        }
        self.floating.add_group(members, true);
        self.floating.activate_window(id);
        self.floating_is_active = FloatingActive::Yes;
        self.animate_from_render_positions(&before);
        true
    }

    /// Turn `id`'s floating group into a tiled group column at `insert_idx`.
    pub fn tile_floating_group(&mut self, id: &W::Id, insert_idx: usize) -> bool {
        if self.floating.group_id_of(id).is_none() {
            return false;
        }
        let ids = self.group_member_ids(id);
        let before = self.render_positions_of(&ids);
        let mut members = self.floating.take_group(id);
        for (tile, _) in &mut members {
            tile.stop_move_animations();
        }
        self.scrolling.add_group_column(insert_idx, members, true);
        self.scrolling.activate_window(id);
        self.floating_is_active = FloatingActive::No;
        self.animate_from_render_positions(&before);
        true
    }

    /// Group `ids` (2..=4 windows of this workspace) into a tiled group column placed where
    /// the leftmost window was. Members are laid out automatically: 2 side by side, 3 as two on
    /// top and a full-width strip below, 4 as a 2x2 grid.
    pub fn create_tiled_group(&mut self, ids: &[W::Id]) -> bool {
        let n = ids.len();
        if !(2..=MAX_GROUP_MEMBERS).contains(&n) || ids.iter().any(|id| !self.has_window(id)) {
            return false;
        }
        let view_x = self.scrolling.view_pos();
        let before = self.render_positions_of(ids);
        let mut order: Vec<(W::Id, Point<f64, Logical>)> = before
            .iter()
            .map(|(id, pos)| (id.clone(), Point::from((pos.x + view_x, pos.y))))
            .collect();
        if order.len() != n {
            return false;
        }
        order.sort_by(|(_, a), (_, b)| f64::total_cmp(&a.x, &b.x).then(f64::total_cmp(&a.y, &b.y)));
        let leftmost_x = order[0].1.x;

        let mut tiles = Vec::with_capacity(n);
        for (id, _) in &order {
            let mut removed = if self.floating.has_window(id) {
                self.floating.remove_tile(id)
            } else {
                self.scrolling.remove_tile(id, Transaction::new())
            };
            removed.tile.stop_move_animations();
            tiles.push(removed.tile);
        }

        let gaps = self.options.layout.gaps;
        let band = self.scrolling.working_area();
        let extra = group_extra(gaps);
        let max = Size::from((
            (band.size.w - gaps * 2. - extra.w).max(1.),
            (band.size.h - gaps * 2. - extra.h).max(1.),
        ));
        let sizes: Vec<_> = tiles.iter().map(|t| t.tile_size()).collect();
        let content = auto_content_size(&sizes, gaps, max);
        let cells = auto_cells(n, Rectangle::from_size(content), gaps);
        let idx = self.scrolling.insert_idx_for_strip_x(leftmost_x);
        let members = tiles.into_iter().zip(cells).collect();
        self.scrolling.add_group_column(idx, members, true);
        self.scrolling.activate_window(&order[0].0);
        self.floating_is_active = FloatingActive::No;
        self.animate_from_render_positions(&before);
        true
    }

    pub fn layout_config(&self) -> Option<&niri_config::LayoutPart> {
        self.layout_config.as_ref()
    }

    #[cfg(test)]
    pub fn scrolling(&self) -> &ScrollingSpace<W> {
        &self.scrolling
    }

    #[cfg(test)]
    pub fn floating(&self) -> &FloatingSpace<W> {
        &self.floating
    }

    #[cfg(test)]
    pub fn verify_invariants(&self, move_win_id: Option<&W::Id>) {
        use approx::assert_abs_diff_eq;

        let scale = self.scale.fractional_scale();
        assert!(scale > 0.);
        assert!(scale.is_finite());

        let options = Options::clone(&self.base_options)
            .with_merged_layout(self.layout_config.as_ref())
            .adjusted_for_scale(scale);
        assert_eq!(
            &*self.options, &options,
            "options must be base options adjusted for scale"
        );

        assert!(self.view_size.w > 0.);
        assert!(self.view_size.h > 0.);

        assert_eq!(self.background_buffer.size(), self.view_size);
        assert_eq!(
            self.background_buffer.color().components(),
            options.layout.background_color.to_array_unpremul(),
        );

        assert_eq!(self.view_size, self.scrolling.view_size());
        assert_eq!(self.working_area, self.scrolling.parent_area());
        assert_eq!(&self.clock, self.scrolling.clock());
        assert!(Rc::ptr_eq(&self.options, self.scrolling.options()));
        self.scrolling.verify_invariants();

        assert_eq!(self.view_size, self.floating.view_size());
        assert_eq!(self.working_area, self.floating.working_area());
        assert_eq!(&self.clock, self.floating.clock());
        assert!(Rc::ptr_eq(&self.options, self.floating.options()));
        self.floating.verify_invariants();

        if self.floating.is_empty() {
            assert!(
                !self.floating_is_active.get(),
                "when floating is empty it must never be active"
            );
        } else if self.scrolling.is_empty() {
            assert!(
                self.floating_is_active.get(),
                "when scrolling is empty but floating isn't, floating should be active"
            );
        }

        for (tile, tile_pos, visible) in self.tiles_with_render_positions() {
            if Some(tile.window().id()) != move_win_id {
                assert_eq!(tile.interactive_move_offset, Point::from((0., 0.)));
            }

            let rounded_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

            // Tile positions must be rounded to physical pixels.
            assert_abs_diff_eq!(tile_pos.x, rounded_pos.x, epsilon = 1e-5);
            assert_abs_diff_eq!(tile_pos.y, rounded_pos.y, epsilon = 1e-5);

            if let Some(alpha) = &tile.alpha_animation {
                let anim = &alpha.anim;
                if visible {
                    assert_eq!(anim.to(), 1., "visible tiles can animate alpha only to 1");
                }

                assert!(
                    !alpha.hold_after_done,
                    "tiles in the layout cannot have held alpha animation"
                );
            }
        }
    }
}

pub(super) fn compute_working_area(output: &Output) -> Rectangle<f64, Logical> {
    layer_map_for_output(output).non_exclusive_zone().to_f64()
}

fn compute_workspace_shadow_config(
    config: niri_config::WorkspaceShadow,
    view_size: Size<f64, Logical>,
) -> niri_config::Shadow {
    // Gaps between workspaces are a multiple of the view height, so shadow settings should also be
    // normalized to the view height to prevent them from overlapping on lower resolutions.
    let norm = view_size.h / 1080.;

    let mut config = niri_config::Shadow::from(config);
    config.softness *= norm;
    config.spread *= norm;
    config.offset.x.0 *= norm;
    config.offset.y.0 *= norm;

    config
}
