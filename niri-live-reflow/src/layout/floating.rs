use std::{cmp::max, iter::zip, rc::Rc};

use niri_config::{utils::MergeWith as _, PresetSize, RelativeTo};
use niri_ipc::{PositionChange, SizeChange, WindowLayout};
use smithay::{
    backend::renderer::gles::GlesRenderer,
    utils::{Logical, Point, Rectangle, Scale, Serial, Size},
};

use super::{
    closing_window::{ClosingWindow, ClosingWindowRenderElement},
    floating_group::{
        cell_limits_of, clamp_to_max, collapse_cells, group_join_at, half_member, join_cells,
        lone_join_at, member_on_edges, redistribute, resize_member, scale_cells, union_all,
        CellLimits, FloatingGroupId, FloatingJoinHint, FloatingJoinOp, FloatingSplitSide,
        GroupLimits, MAX_GROUP_MEMBERS,
    },
    group_chrome::{
        bottom_bar_w, content_box, group_box, group_extra, hit_at as group_bar_hit_at,
        side_bar_rect, GroupBarHit, GroupChrome, GroupChromeLayer, GROUP_BOTTOM_H,
    },
    scrolling::{compute_working_area, ColumnWidth},
    tile::{FloatingExpand, OpenAnimKind, Tile, TileRenderElement, TileRenderSnapshot},
    window_chrome::WindowChrome,
    workspace::{InteractiveResize, ResolvedSize},
    ConfigureIntent, InteractiveResizeData, LayoutElement, Options, RemovedTile, SizeFrac,
};
use crate::{
    animation::{Animation, Clock, Curve},
    niri_render_elements,
    render_helpers::{renderer::NiriRenderer, xray::XrayPos, RenderCtx},
    utils::{
        center_preferring_top_left_in_area, clamp_preferring_top_left_in_area, ensure_min_max_size,
        ensure_min_max_size_maybe_zero, transaction::TransactionBlocker, ResizeEdge,
    },
    window::ResolvedWindowRules,
};

/// By how many logical pixels the directional move commands move floating windows.
pub const DIRECTIONAL_MOVE_PX: f64 = 50.;

/// Space for floating windows.
#[derive(Debug)]
pub struct FloatingSpace<W: LayoutElement> {
    /// Tiles in top-to-bottom order.
    tiles: Vec<Tile<W>>,

    /// Extra per-tile data.
    data: Vec<Data>,

    /// Id of the active window.
    ///
    /// The active window is not necessarily the topmost window. Focus-follows-mouse should
    /// activate a window, but not bring it to the top, because that's very annoying.
    ///
    /// This is always set to `Some()` when `tiles` isn't empty.
    active_window_id: Option<W::Id>,

    /// Ongoing interactive resize.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Group cells at the start of a grouped interactive resize.
    group_resize_start: Option<Vec<(W::Id, Rectangle<f64, Logical>)>>,

    /// Windows in the closing animation.
    closing_windows: Vec<ClosingWindow>,

    /// Side bars of the floating groups.
    group_chromes: Vec<(FloatingGroupId, GroupChrome)>,

    /// Empty cells (view coordinates) of members picked up with a deferred ungroup; the group
    /// keeps its box until the pickup resolves.
    group_reserved: Vec<(FloatingGroupId, Rectangle<f64, Logical>)>,

    /// Group boxes before a group Fullscreen, restored by a second Fullscreen.
    group_restore: Vec<(FloatingGroupId, Rectangle<f64, Logical>)>,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area for this space.
    working_area: Rectangle<f64, Logical>,

    /// Scale of the output the space is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties of the layout.
    options: Rc<Options>,
}

niri_render_elements! {
    FloatingSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        ClosingWindow = ClosingWindowRenderElement,
        GroupLayer = GroupChromeLayer,
    }
}

/// Extra per-tile data.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Data {
    /// Position relative to the working area.
    pos: Point<f64, SizeFrac>,

    /// Cached position in logical coordinates.
    ///
    /// Not rounded to physical pixels.
    logical_pos: Point<f64, Logical>,

    /// Cached actual size of the tile.
    size: Size<f64, Logical>,

    /// Working area used for conversions.
    working_area: Rectangle<f64, Logical>,

    /// Tiled scrolling working area (exclusive zone + struts). Used for Y clamp only.
    y_band: Rectangle<f64, Logical>,

    /// Item 5 group membership. Y clamp stays in `recompute_logical_pos` (item 2).
    group: Option<GroupSlot>,

    /// Set while the window is minimized: hidden, and where it goes back to.
    minimized: Option<MinimizedFrom>,
}

/// Where a minimized window is restored to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MinimizedFrom {
    Floating,
    Tiled {
        column_idx: usize,
        /// Index inside the column when it had other tiles; `None` restores a column.
        tile_idx: Option<usize>,
        width: ColumnWidth,
        is_full_width: bool,
        /// The tile's floating size and position from before, which parking it here clobbers.
        floating_window_size: Option<Size<i32, Logical>>,
        floating_pos: Option<Point<f64, SizeFrac>>,
    },
}

/// A member's place in its floating group.
#[derive(Debug, Clone, Copy, PartialEq)]
struct GroupSlot {
    id: FloatingGroupId,
    /// Cell in view coordinates; the tile is placed at `cell.loc` and asked for `cell.size`.
    cell: Rectangle<f64, Logical>,
}

impl Data {
    pub fn new<W: LayoutElement>(
        working_area: Rectangle<f64, Logical>,
        y_band: Rectangle<f64, Logical>,
        tile: &Tile<W>,
        logical_pos: Point<f64, Logical>,
    ) -> Self {
        let mut rv = Self {
            pos: Point::default(),
            logical_pos: Point::default(),
            size: Size::default(),
            working_area,
            y_band,
            group: None,
            minimized: None,
        };
        rv.update(tile);
        rv.set_logical_pos(logical_pos);
        rv
    }

    pub fn scale_by_working_area(
        area: Rectangle<f64, Logical>,
        pos: Point<f64, SizeFrac>,
    ) -> Point<f64, Logical> {
        let mut logical_pos = Point::from((pos.x, pos.y));
        logical_pos.x *= area.size.w;
        logical_pos.y *= area.size.h;
        logical_pos += area.loc;
        logical_pos
    }

    pub fn logical_to_size_frac_in_working_area(
        area: Rectangle<f64, Logical>,
        logical_pos: Point<f64, Logical>,
    ) -> Point<f64, SizeFrac> {
        let pos = logical_pos - area.loc;
        let mut pos = Point::from((pos.x, pos.y));
        pos.x /= f64::max(area.size.w, 1.0);
        pos.y /= f64::max(area.size.h, 1.0);
        pos
    }

    fn recompute_logical_pos(&mut self) {
        let mut logical_pos = Self::scale_by_working_area(self.working_area, self.pos);

        // Make sure the window doesn't go too much off-screen horizontally.
        // Numbers taken from Mutter.
        let min_on_screen_hor = f64::clamp(self.size.w / 4., 10., 75.);
        let max_off_screen_hor = f64::max(0., self.size.w - min_on_screen_hor);

        // Y: lone floating SSD title bars stay inside the tiled scrolling working area.
        // Grouped members follow the group box clamp instead (see `clamp_group_loc`).
        // Output-expand (fullscreen-on-floating) is taller than the band — don't clamp.
        if self.group.is_none() && self.size.h <= self.y_band.size.h + 1. {
            logical_pos.y = clamp_title_bar_y(logical_pos.y, self.size.h, self.y_band);
        }

        // X is pinned to the viewport: the tiled strip scrolls behind floating windows.
        let area = self.working_area;
        logical_pos.x = f64::max(logical_pos.x, area.loc.x - max_off_screen_hor);
        logical_pos.x = f64::min(
            logical_pos.x,
            area.loc.x + area.size.w - self.size.w + max_off_screen_hor,
        );

        self.logical_pos = logical_pos;
    }

    pub fn update_config(
        &mut self,
        working_area: Rectangle<f64, Logical>,
        y_band: Rectangle<f64, Logical>,
    ) {
        if self.working_area == working_area && self.y_band == y_band {
            return;
        }

        self.working_area = working_area;
        self.y_band = y_band;
        self.recompute_logical_pos();
    }

    pub fn update<W: LayoutElement>(&mut self, tile: &Tile<W>) {
        let size = tile.tile_size();
        if self.size == size {
            return;
        }

        self.size = size;
        self.recompute_logical_pos();
    }

    pub fn set_logical_pos(&mut self, logical_pos: Point<f64, Logical>) {
        self.pos = Self::logical_to_size_frac_in_working_area(self.working_area, logical_pos);

        // This will clamp the logical position to the current working area.
        self.recompute_logical_pos();
    }

    pub fn center(&self) -> Point<f64, Logical> {
        self.logical_pos + self.size.downscale(2.)
    }

    #[cfg(test)]
    fn verify_invariants(&self) {
        let mut temp = *self;
        temp.recompute_logical_pos();
        assert_eq!(
            self.logical_pos, temp.logical_pos,
            "cached logical pos must be up to date"
        );
    }
}

impl<W: LayoutElement> FloatingSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        Self {
            tiles: Vec::new(),
            data: Vec::new(),
            active_window_id: None,
            interactive_resize: None,
            group_resize_start: None,
            closing_windows: Vec::new(),
            group_chromes: Vec::new(),
            group_reserved: Vec::new(),
            group_restore: Vec::new(),
            view_size,
            working_area,
            scale,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        let y_band = title_bar_band(working_area, scale, &options);
        for (tile, data) in zip(&mut self.tiles, &mut self.data) {
            tile.update_config(view_size, scale, options.clone());
            data.update(tile);
            data.update_config(working_area, y_band);
            if let Some(slot) = data.group {
                data.set_logical_pos(slot.cell.loc);
            }
        }

        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.options = options;
    }

    pub fn update_shaders(&mut self) {
        for tile in &mut self.tiles {
            tile.update_shaders();
        }
        for (_, chrome) in &mut self.group_chromes {
            chrome.update_shaders();
        }
    }

    pub fn advance_animations(&mut self) {
        for tile in &mut self.tiles {
            tile.advance_animations();
        }

        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.tiles.iter().any(Tile::are_animations_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.tiles.iter().any(Tile::are_transitions_ongoing) || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(
        &mut self,
        is_active: bool,
        view_rect: Rectangle<f64, Logical>,
        view_pos: f64,
        sunk: bool,
    ) {
        let active = self.active_window_id.clone();
        for (tile, data) in zip(&mut self.tiles, &mut self.data) {
            tile.set_chrome_in_group(data.group.is_some());
            tile.set_floating(!matches!(data.minimized, Some(MinimizedFrom::Tiled { .. })));
            tile.set_sunk(sunk && data.minimized.is_none());
            data.update(tile);
        }
        for (tile, offset) in self.tiles_with_offsets_mut() {
            let id = tile.window().id();
            let is_active = is_active && Some(id) == active.as_ref();

            let mut tile_view_rect = view_rect;
            let view_off = Point::from((offset.x - view_pos, offset.y));
            tile_view_rect.loc -= view_off + tile.render_offset();
            tile.update_render_elements(is_active, tile_view_rect);
        }

        let mut gids: Vec<FloatingGroupId> = Vec::new();
        for data in &self.data {
            if let Some(slot) = data.group {
                if !gids.contains(&slot.id) {
                    gids.push(slot.id);
                }
            }
        }
        self.group_chromes.retain(|(gid, _)| gids.contains(gid));
        self.group_restore.retain(|(gid, _)| gids.contains(gid));
        self.group_reserved.retain(|(gid, _)| gids.contains(gid));
        let gap = self.options.layout.gaps;
        for gid in gids {
            let cells_box = self.group_render_cells_box(gid);
            let members = self.group_members(gid);
            let radius = members
                .first()
                .map(|&i| self.tiles[i].window().geometry_corner_radius().top_left)
                .unwrap_or_default();
            let bar = side_bar_rect(cells_box, gap);
            let bottom_w = bottom_bar_w(cells_box, gap);
            let scale = self.scale;
            let chrome = match self.group_chromes.iter().position(|(g, _)| *g == gid) {
                Some(pos) => &mut self.group_chromes[pos].1,
                None => {
                    self.group_chromes.push((gid, GroupChrome::new()));
                    &mut self.group_chromes.last_mut().unwrap().1
                }
            };
            chrome.update(bar.size.h, bottom_w, radius, scale);
            chrome.update_glass(&self.options.layout.group_border, self.options.blur);
        }
    }

    /// Union of the members' current (animated) tile rects and the reserved cell, in strip
    /// coordinates.
    fn group_render_cells_box(&self, gid: FloatingGroupId) -> Rectangle<f64, Logical> {
        let mut rects: Vec<_> = self
            .group_members(gid)
            .into_iter()
            .map(|i| {
                let tile = &self.tiles[i];
                let pos = self.data[i].logical_pos + tile.render_offset();
                let pos = pos
                    .to_physical_precise_round(self.scale)
                    .to_logical(self.scale);
                Rectangle::new(pos, tile.tile_size())
            })
            .collect();
        rects.extend(self.reserved_cell(gid));
        union_all(&rects)
    }

    fn reserved_cell(&self, gid: FloatingGroupId) -> Option<Rectangle<f64, Logical>> {
        self.group_reserved
            .iter()
            .find(|(g, _)| *g == gid)
            .map(|(_, c)| *c)
    }

    /// Side and bottom bar rects of the floating groups, workspace view coordinates.
    pub fn group_chrome_rects(&self, view_pos: f64) -> Vec<Rectangle<f64, Logical>> {
        let mut out = Vec::new();
        for (gid, chrome) in &self.group_chromes {
            if self.group_members(*gid).is_empty() {
                continue;
            }
            out.extend(chrome.rects(self.group_bar_loc(*gid, view_pos)));
        }
        out
    }

    /// Side bar origin of group `gid`, workspace view coordinates.
    fn group_bar_loc(&self, gid: FloatingGroupId, view_pos: f64) -> Point<f64, Logical> {
        let cells_box = self.group_render_cells_box(gid);
        let bar = side_bar_rect(cells_box, self.options.layout.gaps);
        Point::from((bar.loc.x - view_pos, bar.loc.y))
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.tiles.iter()
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.tiles.iter_mut()
    }

    pub fn tiles_with_offsets(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.data.iter().map(|d| d.logical_pos);
        zip(&self.tiles, offsets)
    }

    fn shown_tiles_with_offsets(&self) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> {
        zip(&self.tiles, &self.data)
            .filter(|(_, d)| d.minimized.is_none())
            .map(|(tile, d)| (tile, d.logical_pos))
    }

    pub fn tiles_with_offsets_mut(
        &mut self,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let offsets = self.data.iter().map(|d| d.logical_pos);
        zip(&mut self.tiles, offsets)
    }

    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        self.tiles_with_offsets().map(move |(tile, offset)| {
            let pos = offset + tile.render_offset();
            // Round to physical pixels.
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);
            (tile, pos)
        })
    }

    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> {
        let scale = self.scale;
        self.tiles_with_offsets_mut().map(move |(tile, offset)| {
            let mut pos = offset + tile.render_offset();
            // Round to physical pixels.
            if round {
                pos = pos.to_physical_precise_round(scale).to_logical(scale);
            }
            (tile, pos)
        })
    }

    pub fn tiles_with_ipc_layouts(&self) -> impl Iterator<Item = (&Tile<W>, WindowLayout)> {
        let scale = self.scale;
        self.tiles_with_offsets().map(move |(tile, offset)| {
            // Do not include animated render offset here to avoid IPC spam.
            let pos = offset;
            // Round to physical pixels.
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);

            let layout = WindowLayout {
                tile_pos_in_workspace_view: Some(pos.into()),
                ..tile.ipc_layout_template()
            };
            (tile, layout)
        })
    }

    pub fn new_window_toplevel_bounds(&self, rules: &ResolvedWindowRules) -> Size<i32, Logical> {
        let border_config = self.options.layout.border.merged_with(&rules.border);
        compute_toplevel_bounds(border_config, self.working_area.size)
    }

    /// Returns the geometry of the active tile relative to and clamped to the working area.
    ///
    /// During animations, assumes the final tile position.
    pub fn active_tile_visual_rectangle(&self, view_pos: f64) -> Option<Rectangle<f64, Logical>> {
        let (tile, offset) = self.shown_tiles_with_offsets().next()?;

        let tile_size = tile.tile_size();
        let tile_rect = Rectangle::new(Point::from((offset.x - view_pos, offset.y)), tile_size);

        self.working_area.intersection(tile_rect)
    }

    pub fn popup_target_rect(&self, id: &W::Id, view_pos: f64) -> Option<Rectangle<f64, Logical>> {
        for (tile, pos) in self.tiles_with_offsets() {
            if tile.window().id() == id {
                let view_tile = Point::from((pos.x - view_pos, pos.y));
                // Position within the working area.
                let mut target = self.working_area;
                target.loc -= view_tile;
                target.loc -= tile.window_loc();

                return Some(target);
            }
        }
        None
    }

    fn idx_of(&self, id: &W::Id) -> Option<usize> {
        self.tiles.iter().position(|tile| tile.window().id() == id)
    }

    fn contains(&self, id: &W::Id) -> bool {
        self.idx_of(id).is_some()
    }

    pub fn active_tile(&self) -> Option<&Tile<W>> {
        let id = self.active_window_id.as_ref()?;
        let idx = self.idx_of(id)?;
        if self.data[idx].minimized.is_some() {
            return None;
        }
        Some(&self.tiles[idx])
    }

    pub fn active_window(&self) -> Option<&W> {
        self.active_tile().map(Tile::window)
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        let id = self.active_window_id.as_ref()?;
        self.tiles
            .iter_mut()
            .find(|tile| tile.window().id() == id)
            .map(Tile::window_mut)
    }

    pub fn has_window(&self, id: &W::Id) -> bool {
        self.tiles.iter().any(|tile| tile.window().id() == id)
    }

    /// Whether there are no shown (non-minimized) tiles.
    pub fn is_empty(&self) -> bool {
        self.data.iter().all(|d| d.minimized.is_some())
    }

    pub fn is_minimized(&self, id: &W::Id) -> bool {
        self.idx_of(id)
            .is_some_and(|idx| self.data[idx].minimized.is_some())
    }

    pub fn minimized_from(&self, id: &W::Id) -> Option<MinimizedFrom> {
        self.data[self.idx_of(id)?].minimized
    }

    /// Shown (non-minimized) tiles with their render positions.
    pub fn shown_tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> {
        zip(self.tiles_with_render_positions(), &self.data)
            .filter(|(_, data)| data.minimized.is_none())
            .map(|(x, _)| x)
    }

    /// Park `tile` hidden at the bottom of the stack until [`Self::take_minimized`].
    pub fn add_minimized_tile(&mut self, mut tile: Tile<W>, from: MinimizedFrom) {
        tile.update_config(self.view_size, self.scale, self.options.clone());
        let pos = self.stored_or_default_tile_pos(&tile).unwrap_or_else(|| {
            center_preferring_top_left_in_area(self.working_area, tile.tile_size())
        });
        let mut data = Data::new(self.working_area, self.title_bar_band(), &tile, pos);
        data.minimized = Some(from);
        if self.active_window_id.is_none() {
            self.active_window_id = Some(tile.window().id().clone());
        }
        self.data.push(data);
        self.tiles.push(tile);
        self.fix_active_after_minimize();
    }

    /// Remove minimized `id`, with its tile state as before it was minimized.
    pub fn take_minimized(&mut self, id: &W::Id) -> Option<(RemovedTile<W>, MinimizedFrom)> {
        let idx = self.idx_of(id)?;
        let from = self.data[idx].minimized?;
        let mut removed = self.remove_tile_by_idx(idx);
        if let MinimizedFrom::Tiled {
            floating_window_size,
            floating_pos,
            ..
        } = from
        {
            removed.tile.floating_window_size = floating_window_size;
            removed.tile.floating_pos = floating_pos;
        }
        Some((removed, from))
    }

    /// Keep the active window a shown one when there is any.
    fn fix_active_after_minimize(&mut self) {
        let active_shown = self
            .active_window_id
            .as_ref()
            .and_then(|id| self.idx_of(id))
            .is_some_and(|idx| self.data[idx].minimized.is_none());
        if active_shown {
            return;
        }
        if let Some(idx) = self.data.iter().position(|d| d.minimized.is_none()) {
            self.active_window_id = Some(self.tiles[idx].window().id().clone());
        }
    }

    pub fn add_tile(&mut self, tile: Tile<W>, activate: bool) {
        self.add_tile_at(0, tile, activate);
    }

    fn add_tile_at(&mut self, mut idx: usize, mut tile: Tile<W>, activate: bool) {
        tile.set_floating(true);
        tile.update_config(self.view_size, self.scale, self.options.clone());

        // Restore the previous floating window size, and in case the tile is fullscreen,
        // unfullscreen it.
        let floating_size = tile.floating_window_size;
        let max_win = self.max_window_size(&tile);
        let win = tile.window_mut();
        let mut size = if !win.pending_sizing_mode().is_normal() {
            // If the window was fullscreen or maximized without a floating size, ask for (0, 0).
            floating_size.unwrap_or_default()
        } else {
            // If the window wasn't fullscreen without a floating size (e.g. it was tiled before),
            // ask for the current size. If the current size is unknown (the window was only ever
            // fullscreen until now), fall back to (0, 0).
            floating_size.unwrap_or_else(|| win.expected_size().unwrap_or_default())
        };
        size.w = size.w.min(max_win.w);
        size.h = size.h.min(max_win.h);

        // Apply min/max size window rules. If requesting a concrete size, apply completely; if
        // requesting (0, 0), apply only when min/max results in a fixed size.
        let min_size = win.min_size();
        let max_size = win.max_size();
        size.w = ensure_min_max_size_maybe_zero(size.w, min_size.w, max_size.w);
        size.h = ensure_min_max_size_maybe_zero(size.h, min_size.h, max_size.h);

        win.request_size_once(size, true);

        if activate || self.tiles.is_empty() {
            self.active_window_id = Some(win.id().clone());
        }

        // Make sure the tile isn't inserted below its parent.
        for (i, tile_above) in self.tiles.iter().enumerate().take(idx) {
            if win.is_child_of(tile_above.window()) {
                idx = i;
                break;
            }
        }

        let pos = self.stored_or_default_tile_pos(&tile).unwrap_or_else(|| {
            center_preferring_top_left_in_area(self.working_area, tile.tile_size())
        });

        let data = Data::new(self.working_area, self.title_bar_band(), &tile, pos);
        self.data.insert(idx, data);
        self.tiles.insert(idx, tile);

        self.bring_up_descendants_of(idx);
    }

    pub fn add_tile_above(&mut self, above: &W::Id, mut tile: Tile<W>, activate: bool) {
        let idx = self.idx_of(above).unwrap();

        let above_pos = self.data[idx].logical_pos;
        let above_size = self.data[idx].size;
        let tile_size = tile.tile_size();
        let pos = above_pos + (above_size.to_point() - tile_size.to_point()).downscale(2.);
        let pos = self.clamp_within_working_area(pos, tile_size);
        tile.floating_pos = Some(self.logical_to_size_frac(pos));

        self.add_tile_at(idx, tile, activate);
    }

    fn bring_up_descendants_of(&mut self, idx: usize) {
        let tile = &self.tiles[idx];
        let win = tile.window();

        // We always maintain the correct stacking order, so walking descendants back to front
        // should give us all of them.
        let mut descendants: Vec<usize> = Vec::new();
        for (i, tile_below) in self.tiles.iter().enumerate().skip(idx + 1).rev() {
            let win_below = tile_below.window();
            if win_below.is_child_of(win)
                || descendants
                    .iter()
                    .any(|idx| win_below.is_child_of(self.tiles[*idx].window()))
            {
                descendants.push(i);
            }
        }

        // Now, descendants is in back-to-front order, and repositioning them in the front-to-back
        // order will preserve the subsequent indices and work out right.
        let mut idx = idx;
        #[allow(clippy::explicit_counter_loop)]
        for descendant_idx in descendants.into_iter().rev() {
            self.raise_window(descendant_idx, idx);
            idx += 1;
        }
    }

    pub fn remove_active_tile(&mut self) -> Option<RemovedTile<W>> {
        let id = self.active_window_id.clone()?;
        Some(self.remove_tile(&id))
    }

    pub fn remove_tile(&mut self, id: &W::Id) -> RemovedTile<W> {
        let idx = self.idx_of(id).unwrap();
        self.remove_tile_by_idx(idx)
    }

    fn remove_tile_by_idx(&mut self, idx: usize) -> RemovedTile<W> {
        let mut tile = self.tiles.remove(idx);
        let data = self.data.remove(idx);

        // Pickup / close leaves the group; the rest grow into the vacated cell.
        if let Some(slot) = data.group {
            self.redistribute_group(slot.id, slot.cell);
        }

        if self.tiles.is_empty() {
            self.active_window_id = None;
        } else if Some(tile.window().id()) == self.active_window_id.as_ref() {
            // The active tile was removed, make the topmost tile active.
            self.active_window_id = Some(self.tiles[0].window().id().clone());
            self.fix_active_after_minimize();
        }

        // Stop interactive resize.
        if let Some(resize) = &self.interactive_resize {
            if tile.window().id() == &resize.window {
                self.interactive_resize = None;
                self.group_resize_start = None;
            }
        }
        // A pushed neighbor left mid-resize: the start snapshot is stale.
        if self
            .group_resize_start
            .as_ref()
            .is_some_and(|cells| cells.iter().any(|(id, _)| id == tile.window().id()))
        {
            self.group_resize_start = None;
            self.interactive_resize = None;
        }

        // Store the floating size if we have one.
        if let Some(size) = tile.window().expected_size() {
            tile.floating_window_size = Some(size);
        }
        // Store the floating position.
        tile.floating_pos = Some(data.pos);

        let width = ColumnWidth::Fixed(tile.tile_expected_or_current_size().w);
        RemovedTile {
            tile,
            width,
            is_full_width: false,
            is_floating: true,
        }
    }

    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        id: &W::Id,
        blocker: TransactionBlocker,
    ) {
        if self.is_minimized(id) {
            return;
        }
        let (tile, tile_pos) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == id)
            .unwrap();

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        self.start_close_animation_for_tile(
            renderer, snapshot, tile_size, tile_pos, blocker, false,
        );
    }

    pub fn start_minimize_animation_for_window(&mut self, renderer: &mut GlesRenderer, id: &W::Id) {
        if self.is_minimized(id) {
            return;
        }
        let Some((tile, tile_pos)) = self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == id)
        else {
            return;
        };

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();
        self.start_close_animation_for_tile(
            renderer,
            snapshot,
            tile_size,
            tile_pos,
            TransactionBlocker::completed(),
            true,
        );
    }

    pub fn activate_window_without_raising(&mut self, id: &W::Id) -> bool {
        if !self.contains(id) {
            return false;
        }

        self.active_window_id = Some(id.clone());
        true
    }

    pub fn activate_window(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.raise_window(idx, 0);
        self.active_window_id = Some(id.clone());
        self.bring_up_descendants_of(0);

        true
    }

    fn raise_window(&mut self, from_idx: usize, to_idx: usize) {
        assert!(to_idx <= from_idx);

        let tile = self.tiles.remove(from_idx);
        let data = self.data.remove(from_idx);
        self.tiles.insert(to_idx, tile);
        self.data.insert(to_idx, data);
    }

    pub fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
        minimize: bool,
    ) {
        let anim = if minimize {
            Animation::ease(self.clock.clone(), 0., 1., 0., 220, Curve::EaseOutCubic)
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

    pub fn toggle_window_width(&mut self, id: Option<&W::Id>, forwards: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        let idx = self.idx_of(&id).unwrap();
        self.leave_expand(idx);

        let available_size = self.working_area.size.w;

        let len = self.options.layout.preset_column_widths.len();
        let tile = &mut self.tiles[idx];
        let preset_idx = if let Some(idx) = tile.floating_preset_width_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let current_window = tile.window_expected_or_current_size().w;
            let current_tile = tile.tile_expected_or_current_size().w;

            let mut it = self
                .options
                .layout
                .preset_column_widths
                .iter()
                .map(|preset| resolve_preset_size(*preset, available_size));

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
        self.set_window_width(Some(&id), SizeChange::from(preset), true);

        self.tiles[idx].floating_preset_width_idx = Some(preset_idx);

        self.interactive_resize_end(Some(&id));
    }

    pub fn start_open_animation(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.tiles[idx].start_open_animation_kind(OpenAnimKind::Floating);
        true
    }

    pub fn toggle_window_height(&mut self, id: Option<&W::Id>, forwards: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        let idx = self.idx_of(&id).unwrap();
        self.leave_expand(idx);

        let available_size = self.working_area.size.h;

        let len = self.options.layout.preset_window_heights.len();
        let tile = &mut self.tiles[idx];
        let preset_idx = if let Some(idx) = tile.floating_preset_height_idx {
            (idx + if forwards { 1 } else { len - 1 }) % len
        } else {
            let current_window = tile.window_expected_or_current_size().h;
            let current_tile = tile.tile_expected_or_current_size().h;

            let mut it = self
                .options
                .layout
                .preset_window_heights
                .iter()
                .map(|preset| resolve_preset_size(*preset, available_size));

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

        let preset = self.options.layout.preset_window_heights[preset_idx];
        self.set_window_height(Some(&id), SizeChange::from(preset), true);

        let tile = &mut self.tiles[idx];
        tile.floating_preset_height_idx = Some(preset_idx);

        self.interactive_resize_end(Some(&id));
    }

    pub fn set_window_width(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(id).unwrap();
        self.leave_expand(idx);
        let max_win = self.max_window_size(&self.tiles[idx]);

        let tile = &mut self.tiles[idx];
        tile.floating_preset_width_idx = None;

        let available_size = self.working_area.size.w;
        let win = tile.window();
        let current_window = win.expected_size().unwrap_or_else(|| win.size()).w;
        let current_tile = tile.tile_expected_or_current_size().w;

        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let win_width = match change {
            SizeChange::SetFixed(win_width) => f64::from(win_width),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                let tile_width = available_size * prop;
                tile.window_width_for_tile_width(tile_width)
            }
            SizeChange::AdjustFixed(delta) => f64::from(current_window.saturating_add(delta)),
            SizeChange::AdjustProportion(delta) => {
                let current_prop = current_tile / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., MAX_F);
                let tile_width = available_size * prop;
                tile.window_width_for_tile_width(tile_width)
            }
        };
        let win_width = (win_width.round().clamp(1., MAX_PX) as i32).min(max_win.w);

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();

        let win_width = ensure_min_max_size(win_width, min_size.w, max_size.w);

        if let Some(slot) = self.data[idx].group {
            let tile_w = self.tiles[idx].tile_width_for_window_width(f64::from(win_width));
            let delta = Point::from((tile_w - slot.cell.size.w, 0.));
            self.resize_group_member(idx, ResizeEdge::RIGHT, delta, animate);
            return;
        }

        let win_height = win.expected_size().unwrap_or_default().h.min(max_win.h);
        let win_height = ensure_min_max_size(win_height, min_size.h, max_size.h);

        let win_size = Size::from((win_width, win_height));
        win.request_size_once(win_size, animate);
    }

    pub fn set_window_height(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(id).unwrap();
        self.leave_expand(idx);
        let max_win = self.max_window_size(&self.tiles[idx]);

        let tile = &mut self.tiles[idx];
        tile.floating_preset_height_idx = None;

        let available_size = self.working_area.size.h;
        let win = tile.window();
        let current_window = win.expected_size().unwrap_or_else(|| win.size()).h;
        let current_tile = tile.tile_expected_or_current_size().h;

        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let win_height = match change {
            SizeChange::SetFixed(win_height) => f64::from(win_height),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                let tile_height = available_size * prop;
                tile.window_height_for_tile_height(tile_height)
            }
            SizeChange::AdjustFixed(delta) => f64::from(current_window.saturating_add(delta)),
            SizeChange::AdjustProportion(delta) => {
                let current_prop = current_tile / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., MAX_F);
                let tile_height = available_size * prop;
                tile.window_height_for_tile_height(tile_height)
            }
        };
        let win_height = (win_height.round().clamp(1., MAX_PX) as i32).min(max_win.h);

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();

        let win_height = ensure_min_max_size(win_height, min_size.h, max_size.h);

        if let Some(slot) = self.data[idx].group {
            let tile_h = self.tiles[idx].tile_height_for_window_height(f64::from(win_height));
            let delta = Point::from((0., tile_h - slot.cell.size.h));
            self.resize_group_member(idx, ResizeEdge::BOTTOM, delta, animate);
            return;
        }

        let win_width = win.expected_size().unwrap_or_default().w.min(max_win.w);
        let win_width = ensure_min_max_size(win_width, min_size.w, max_size.w);

        let win_size = Size::from((win_width, win_height));
        win.request_size_once(win_size, animate);
    }

    /// Viewport-sized tile: fills `band` (scrolling working area) with no leftover gaps.
    /// Toggle: calling again while already filling restores the previous floating size.
    pub fn size_to_viewport(
        &mut self,
        id: Option<&W::Id>,
        band: Rectangle<f64, Logical>,
    ) -> Option<(f64, f64)> {
        let id = id.or(self.active_window_id.as_ref()).cloned()?;
        let idx = self.idx_of(&id).unwrap();
        self.toggle_expand(idx, FloatingExpand::WorkArea, band.loc, band.size)
    }

    /// Side of the 1:1 floating square: a bit under one quarter (half × half)
    /// of `band`.
    pub fn square_side(band: Rectangle<f64, Logical>) -> f64 {
        const SQUARE_FILL: f64 = 0.92;
        (band.size.w.min(band.size.h) * 0.5 * SQUARE_FILL).floor()
    }

    /// Lime ball: shrink (or grow) a floating tile to the 1:1 square, keeping
    /// its center. Leaves yellow/orange first. Returns whether anything changed.
    pub fn size_to_square(&mut self, id: Option<&W::Id>, band: Rectangle<f64, Logical>) -> bool {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return false;
        };
        let Some(idx) = self.idx_of(&id) else {
            return false;
        };
        if self.data[idx].group.is_some() {
            return false;
        }

        if self.tiles[idx].floating_expand != FloatingExpand::None
            || self.tiles[idx].window().pending_sizing_mode().is_fullscreen()
        {
            self.restore_expand(idx);
        }

        let side = Self::square_side(band);
        let center = self.data[idx].logical_pos + self.data[idx].size.to_point().downscale(2.);

        let max_win = self.max_window_size(&self.tiles[idx]);
        {
            let tile = &mut self.tiles[idx];
            let mut size = tile.square_window_size(side);
            size.w = size.w.min(max_win.w);
            size.h = size.h.min(max_win.h);
            tile.window_mut().request_size_once(size, true);
            tile.floating_window_size = Some(size);
            tile.floating_preset_width_idx = None;
            tile.floating_preset_height_idx = None;
        }
        self.data[idx].update(&self.tiles[idx]);

        let tile_size = self.data[idx].size;
        let pos = center - tile_size.to_point().downscale(2.);
        let pos = self.clamp_within_working_area(pos, tile_size);
        self.move_to(idx, pos, true);
        self.interactive_resize_end(Some(&id));
        true
    }

    /// Cover the whole output in real fullscreen (no chrome, no overlay bars).
    /// Toggle restores the previous floating size. Stays in the floating space.
    pub fn size_to_output(
        &mut self,
        id: Option<&W::Id>,
        _view_size: Size<f64, Logical>,
    ) -> Option<(f64, f64)> {
        let id = id.or(self.active_window_id.as_ref()).cloned()?;
        let idx = self.idx_of(&id)?;
        if self.data[idx].group.is_some() {
            return None;
        }

        if self.tiles[idx].window().pending_sizing_mode().is_fullscreen() {
            self.restore_expand(idx);
            let sz = self.tiles[idx].tile_size();
            return Some((self.data[idx].logical_pos.x, sz.w));
        }

        {
            let tile = &mut self.tiles[idx];
            if tile.floating_expand == FloatingExpand::None {
                tile.floating_expand_restore_pos = tile.floating_pos.or(Some(self.data[idx].pos));
                tile.floating_expand_restore_size = tile
                    .floating_window_size
                    .or_else(|| tile.window().expected_size());
            }
            tile.floating_expand = FloatingExpand::Output;
            tile.apply_border_config();
            tile.floating_preset_width_idx = None;
            tile.floating_preset_height_idx = None;
            tile.request_fullscreen(true, None);
        }
        self.data[idx].update(&self.tiles[idx]);
        self.move_to(idx, Point::from((0., 0.)), true);
        self.interactive_resize_end(Some(&id));
        Some((0., self.view_size.w))
    }

    fn toggle_expand(
        &mut self,
        idx: usize,
        mode: FloatingExpand,
        loc: Point<f64, Logical>,
        size: Size<f64, Logical>,
    ) -> Option<(f64, f64)> {
        if self.data[idx].group.is_some() {
            if mode == FloatingExpand::WorkArea {
                let to = Rectangle::new(loc, size);
                self.scale_group_to(self.data[idx].group.unwrap().id, to);
                let id = self.tiles[idx].window().id().clone();
                self.interactive_resize_end(Some(&id));
                return Some((to.loc.x, to.size.w));
            }
            return None;
        }

        if self.tiles[idx].floating_expand == mode {
            self.restore_expand(idx);
            let id = self.tiles[idx].window().id().clone();
            self.interactive_resize_end(Some(&id));
            let sz = self.tiles[idx].tile_size();
            return Some((self.data[idx].logical_pos.x, sz.w));
        }

        {
            let tile = &mut self.tiles[idx];
            if tile.floating_expand == FloatingExpand::None {
                tile.floating_expand_restore_pos = tile.floating_pos.or(Some(self.data[idx].pos));
                tile.floating_expand_restore_size = tile
                    .floating_window_size
                    .or_else(|| tile.window().expected_size());
            }
            tile.floating_expand = mode;
            tile.apply_border_config();
            tile.floating_preset_width_idx = None;
            tile.floating_preset_height_idx = None;
            // Yellow after orange: drop real fullscreen first.
            if tile.window().pending_sizing_mode().is_fullscreen() {
                tile.window_mut().request_size_once(Default::default(), true);
            }
            tile.request_tile_size(size, true, None);
        }
        self.data[idx].update(&self.tiles[idx]);
        self.move_to(idx, loc, true);
        let id = self.tiles[idx].window().id().clone();
        self.interactive_resize_end(Some(&id));
        Some((loc.x, size.w))
    }

    fn restore_expand(&mut self, idx: usize) {
        let pos;
        let size;
        {
            let tile = &mut self.tiles[idx];
            pos = tile.floating_expand_restore_pos;
            size = tile.floating_expand_restore_size;
            tile.clear_floating_expand();
            if let Some(size) = size {
                tile.window_mut().request_size_once(size, true);
                tile.floating_window_size = Some(size);
            } else {
                // Drop real fullscreen even if we have no stored size.
                tile.window_mut().request_size_once(Default::default(), true);
            }
        }
        self.data[idx].update(&self.tiles[idx]);
        if let Some(pos) = pos {
            self.tiles[idx].floating_pos = Some(pos);
            let logical = Data::scale_by_working_area(self.working_area, pos);
            self.move_to(idx, logical, true);
        }
    }

    /// A manual resize on a yellow/orange tile ends that mode (the window keeps
    /// whatever size the resize gives it; the frame and title bar come back).
    fn leave_expand(&mut self, idx: usize) {
        let was_fs = self.tiles[idx].window().pending_sizing_mode().is_fullscreen();
        if self.tiles[idx].floating_expand == FloatingExpand::None && !was_fs {
            return;
        }
        self.tiles[idx].clear_floating_expand();
        if was_fs {
            self.tiles[idx]
                .window_mut()
                .request_size_once(Default::default(), true);
        }
        self.data[idx].update(&self.tiles[idx]);
    }

    fn focus_directional(
        &mut self,
        distance: impl Fn(Point<f64, Logical>, Point<f64, Logical>) -> f64,
    ) -> bool {
        let Some(active_id) = &self.active_window_id else {
            return false;
        };
        let active_idx = self.idx_of(active_id).unwrap();
        let center = self.data[active_idx].center();

        let result = zip(&self.tiles, &self.data)
            .filter(|(_, data)| data.minimized.is_none())
            .filter(|(tile, _)| tile.window().id() != active_id)
            .map(|(tile, data)| (tile, distance(center, data.center())))
            .filter(|(_, dist)| *dist > 0.)
            .min_by(|(_, dist_a), (_, dist_b)| f64::total_cmp(dist_a, dist_b));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
            true
        } else {
            false
        }
    }

    pub fn focus_left(&mut self) -> bool {
        self.focus_directional(|focus, other| focus.x - other.x)
    }

    pub fn focus_right(&mut self) -> bool {
        self.focus_directional(|focus, other| other.x - focus.x)
    }

    pub fn focus_up(&mut self) -> bool {
        self.focus_directional(|focus, other| focus.y - other.y)
    }

    pub fn focus_down(&mut self) -> bool {
        self.focus_directional(|focus, other| other.y - focus.y)
    }

    pub fn focus_leftmost(&mut self) {
        let result = self
            .shown_tiles_with_offsets()
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_rightmost(&mut self) {
        let result = self
            .shown_tiles_with_offsets()
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.x, &pos_b.x));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_topmost(&mut self) {
        let result = self
            .shown_tiles_with_offsets()
            .min_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    pub fn focus_bottommost(&mut self) {
        let result = self
            .shown_tiles_with_offsets()
            .max_by(|(_, pos_a), (_, pos_b)| f64::total_cmp(&pos_a.y, &pos_b.y));
        if let Some((tile, _)) = result {
            let id = tile.window().id().clone();
            self.activate_window(&id);
        }
    }

    fn move_to(&mut self, idx: usize, new_pos: Point<f64, Logical>, animate: bool) {
        let group = self.data[idx].group;
        let old = self.data[idx].logical_pos;

        // In-layout moves carry the whole group, clamped so the L bottom bar can meet the band
        // top but not rise past it.
        if let Some(slot) = group {
            let id = self.tiles[idx].window().id().clone();
            let Some(gb) = self.group_box_of(&id) else {
                return;
            };
            let clamped = self.clamp_group_loc(gb.loc + (new_pos - old), gb.size);
            let delta = clamped - gb.loc;
            for i in self.group_members(slot.id) {
                let mut cell = self.cell_of(i);
                cell.loc += delta;
                self.set_cell(i, cell, animate);
            }
            self.interactive_resize_end(None);
            return;
        }

        if animate {
            self.move_and_animate(idx, new_pos);
        } else {
            self.data[idx].set_logical_pos(new_pos);
        }

        self.interactive_resize_end(None);
    }

    fn group_members(&self, gid: FloatingGroupId) -> Vec<usize> {
        (0..self.data.len())
            .filter(|&i| self.data[i].group.is_some_and(|s| s.id == gid))
            .collect()
    }

    fn tile_rect(&self, idx: usize) -> Rectangle<f64, Logical> {
        Rectangle::new(self.data[idx].logical_pos, self.data[idx].size)
    }

    /// Group cell of a member, or the tile rect of a lone window.
    fn cell_of(&self, idx: usize) -> Rectangle<f64, Logical> {
        match self.data[idx].group {
            Some(slot) => slot.cell,
            None => self.tile_rect(idx),
        }
    }

    fn member_cell_limits(&self, idx: usize) -> CellLimits {
        cell_limits_of(
            self.tiles[idx].min_size_nonfullscreen(),
            self.tiles[idx].max_size_nonfullscreen(),
        )
    }

    fn members_cell_limits(&self, members: &[usize]) -> Vec<CellLimits> {
        members
            .iter()
            .map(|&i| self.member_cell_limits(i))
            .collect()
    }

    /// Store `cell` for a grouped tile, request its size if it changed and place it there.
    fn set_cell(&mut self, idx: usize, cell: Rectangle<f64, Logical>, animate: bool) {
        let prev = self.data[idx].group.map(|s| s.cell);
        if let Some(slot) = &mut self.data[idx].group {
            slot.cell = cell;
        }
        if prev.is_none_or(|p| p.size != cell.size) {
            self.tiles[idx].request_tile_size(cell.size, animate, None);
        }
        if animate {
            self.move_and_animate(idx, cell.loc);
        } else {
            self.data[idx].set_logical_pos(cell.loc);
        }
    }

    /// Size limits of floating tiles and group boxes, from the tiled scrolling band.
    pub fn group_limits(&self) -> GroupLimits {
        GroupLimits::from_band(self.vertical_band(), self.options.layout.gaps)
    }

    /// Limits of the member cells' box of a group: the group box minus the side and bottom
    /// bars.
    pub fn cell_limits(&self) -> GroupLimits {
        let mut limits = self.group_limits();
        let extra = group_extra(self.options.layout.gaps);
        limits.max.w = (limits.max.w - extra.w).max(1.);
        limits.max.h = (limits.max.h - extra.h).max(1.);
        limits.min.w = limits.min.w.min(limits.max.w);
        limits.min.h = limits.min.h.min(limits.max.h);
        limits
    }

    /// Largest window size that keeps `tile` within the floating max.
    fn max_window_size(&self, tile: &Tile<W>) -> Size<i32, Logical> {
        let max = clamp_to_max(Size::from((f64::MAX, f64::MAX)), self.group_limits());
        Size::from((
            tile.window_width_for_tile_width(max.w).floor().max(1.) as i32,
            tile.window_height_for_tile_height(max.h).floor().max(1.) as i32,
        ))
    }

    /// Resize member `idx` individually from the current cells, pushing neighbors.
    fn resize_group_member(
        &mut self,
        idx: usize,
        edges: ResizeEdge,
        delta: Point<f64, Logical>,
        animate: bool,
    ) {
        let Some(slot) = self.data[idx].group else {
            return;
        };
        let members = self.group_members(slot.id);
        let cells: Vec<_> = members.iter().map(|&i| self.cell_of(i)).collect();
        let pos = members.iter().position(|&i| i == idx).unwrap();
        let gaps = self.options.layout.gaps;
        let member_limits = self.members_cell_limits(&members);
        let out = resize_member(
            &cells,
            &member_limits,
            pos,
            edges,
            delta,
            gaps,
            self.cell_limits(),
        );
        for (&i, cell) in zip(&members, out) {
            self.set_cell(i, cell, animate);
        }
    }

    /// A member with `removed` cell left group `gid`: the rest fill the space, or the group
    /// dissolves when one member is left.
    fn redistribute_group(&mut self, gid: FloatingGroupId, removed: Rectangle<f64, Logical>) {
        let members = self.group_members(gid);
        let reserved = self.reserved_cell(gid).is_some();
        if members.is_empty() {
            self.group_reserved.retain(|(g, _)| *g != gid);
            return;
        }
        if members.len() + usize::from(reserved) < 2 {
            for i in members {
                self.data[i].group = None;
            }
            return;
        }
        if members.len() < 2 {
            // A lone member next to a reserved cell keeps the box.
            return;
        }
        let cells: Vec<_> = members.iter().map(|&i| self.cell_of(i)).collect();
        let out = redistribute(&cells, removed, self.options.layout.gaps);
        for (&i, cell) in zip(&members, out) {
            self.set_cell(i, cell, true);
        }
    }

    /// Topmost floating tile (or group) under `pointer` (workspace view coords) as a join
    /// target: the targeted window and the drop hint in view coords. A full group yields
    /// `None`, so the drop behaves like empty space.
    pub fn join_target(
        &self,
        pointer: Point<f64, Logical>,
        view_pos: f64,
        incoming: CellLimits,
    ) -> Option<(W::Id, FloatingJoinHint)> {
        let gap = self.options.layout.gaps;
        let to_view = |r: Rectangle<f64, Logical>| {
            Rectangle::new(Point::from((r.loc.x - view_pos, r.loc.y)), r.size)
        };
        for idx in 0..self.tiles.len() {
            if self.data[idx].minimized.is_some() {
                continue;
            }
            let rect = to_view(self.tile_rect(idx));
            match self.data[idx].group {
                None => {
                    if rect.size.w < 1. || rect.size.h < 1. || !rect.contains(pointer) {
                        continue;
                    }
                    let mut hint =
                        lone_join_at(rect, pointer, gap, incoming, self.member_cell_limits(idx))?;
                    // The new group's side bar takes the left edge; cells shift right.
                    hint.hint.loc.x += group_extra(gap).w;
                    return Some((self.tiles[idx].window().id().clone(), hint));
                }
                Some(slot) => {
                    let members = self.group_members(slot.id);
                    let cells: Vec<_> = self
                        .join_base_cells(slot.id, &members)
                        .into_iter()
                        .map(to_view)
                        .collect();
                    if !union_all(&cells).contains(pointer) && !rect.contains(pointer) {
                        continue;
                    }
                    let limits = self.members_cell_limits(&members);
                    let (ci, hint) = group_join_at(&cells, &limits, pointer, gap, incoming)?;
                    return Some((self.tiles[members[ci]].window().id().clone(), hint));
                }
            }
        }
        None
    }

    /// Join `incoming` into `target`'s group (or form a pair with a lone `target`) by `op`.
    /// Fails when the group is full.
    pub fn join_group(&mut self, incoming: &W::Id, target: &W::Id, op: FloatingJoinOp) -> bool {
        let (Some(iidx), Some(tidx)) = (self.idx_of(incoming), self.idx_of(target)) else {
            return false;
        };
        if iidx == tidx {
            return false;
        }

        let new_group = self.data[tidx].group.is_none();
        let (gid, members) = match self.data[tidx].group {
            Some(slot) => (slot.id, self.group_members(slot.id)),
            None => (FloatingGroupId::next(), vec![tidx]),
        };
        if members.len() >= MAX_GROUP_MEMBERS || members.contains(&iidx) {
            return false;
        }
        if let Some(slot) = self.data[iidx].group.take() {
            self.redistribute_group(slot.id, slot.cell);
        }
        let op = match op {
            FloatingJoinOp::BottomStrip if members.len() == 1 => {
                FloatingJoinOp::Split(FloatingSplitSide::Bottom)
            }
            op => op,
        };
        let cells = self.join_base_cells(gid, &members);
        self.group_reserved.retain(|(g, _)| *g != gid);
        let target_pos = members.iter().position(|&i| i == tidx).unwrap();
        let member_limits = self.members_cell_limits(&members);
        let incoming_limits = self.member_cell_limits(iidx);
        let Some((mut out, mut incoming_cell)) = join_cells(
            &cells,
            &member_limits,
            target_pos,
            op,
            self.options.layout.gaps,
            incoming_limits,
        ) else {
            return false;
        };
        if new_group {
            let shift = group_extra(self.options.layout.gaps).w;
            for cell in &mut out {
                cell.loc.x += shift;
            }
            incoming_cell.loc.x += shift;
            let mut all = out.clone();
            all.push(incoming_cell);
            let from = union_all(&all);
            let max_w = self.cell_limits().max.w;
            if from.size.w > max_w {
                let to = Rectangle::new(from.loc, Size::from((max_w, from.size.h)));
                let scaled = scale_cells(&all, from, to, self.options.layout.gaps);
                incoming_cell = scaled[scaled.len() - 1];
                out = scaled[..scaled.len() - 1].to_vec();
            }
        }

        for (&i, cell) in zip(&members, out) {
            if self.data[i].group.is_none() {
                self.data[i].group = Some(GroupSlot {
                    id: gid,
                    cell: self.tile_rect(i),
                });
            }
            self.tiles[i].floating_preset_width_idx = None;
            self.tiles[i].floating_preset_height_idx = None;
            self.set_cell(i, cell, true);
        }

        // The caller animates the incoming tile from the drop point.
        self.data[iidx].group = Some(GroupSlot {
            id: gid,
            cell: incoming_cell,
        });
        let tile = &mut self.tiles[iidx];
        tile.floating_preset_width_idx = None;
        tile.floating_preset_height_idx = None;
        tile.request_tile_size(incoming_cell.size, true, None);
        self.data[iidx].set_logical_pos(incoming_cell.loc);

        self.interactive_resize_end(None);
        true
    }

    /// Cells a join on group `gid` starts from: the cells of `members`, grown into the reserved
    /// cell if there is one.
    fn join_base_cells(
        &self,
        gid: FloatingGroupId,
        members: &[usize],
    ) -> Vec<Rectangle<f64, Logical>> {
        let cells: Vec<_> = members.iter().map(|&i| self.cell_of(i)).collect();
        match self.reserved_cell(gid) {
            Some(reserved) => collapse_cells(&cells, reserved, self.options.layout.gaps),
            None => cells,
        }
    }

    /// Remove `id` but keep its group cell reserved, so the group keeps its box until
    /// [`Self::release_reserved_cell`] or [`Self::restore_group_member`].
    pub fn remove_tile_reserving(&mut self, id: &W::Id) -> RemovedTile<W> {
        let idx = self.idx_of(id).unwrap();
        if let Some(slot) = self.data[idx].group.take() {
            self.group_reserved.retain(|(g, _)| *g != slot.id);
            self.group_reserved.push((slot.id, slot.cell));
        }
        self.remove_tile_by_idx(idx)
    }

    /// Resolve a deferred ungroup of `member`'s group: the reserved cell is freed (the others
    /// redistribute, or the group dissolves).
    pub fn release_reserved_cell(&mut self, member: &W::Id) -> bool {
        let Some(gid) = self.group_id_of(member) else {
            return false;
        };
        let Some(pos) = self.group_reserved.iter().position(|(g, _)| *g == gid) else {
            return false;
        };
        let (_, removed) = self.group_reserved.remove(pos);
        self.redistribute_group(gid, removed);
        true
    }

    /// Put `tile` back into the floating group of the other windows of `cells` (view
    /// coordinates, the picked window included) with exactly those cells. Works when the others
    /// are still that group (possibly with a reserved cell) or, for a pair, the lone window the
    /// group dissolved into.
    pub fn restore_group_member(
        &mut self,
        mut tile: Tile<W>,
        cells: &[(W::Id, Rectangle<f64, Logical>)],
    ) -> Result<(), Tile<W>> {
        let picked = tile.window().id().clone();
        let Some(&(_, cell)) = cells.iter().find(|(id, _)| *id == picked) else {
            return Err(tile);
        };
        let others: Vec<usize> = cells
            .iter()
            .filter(|(id, _)| *id != picked)
            .filter_map(|(id, _)| self.idx_of(id))
            .collect();
        if others.is_empty() || others.len() + 1 != cells.len() || cells.len() > MAX_GROUP_MEMBERS {
            return Err(tile);
        }
        let gids: Vec<_> = others
            .iter()
            .map(|&i| self.data[i].group.map(|s| s.id))
            .collect();
        let gid = match gids[0] {
            Some(g)
                if gids.iter().all(|x| *x == Some(g))
                    && self.group_members(g).len() == others.len() =>
            {
                g
            }
            None if others.len() == 1 => FloatingGroupId::next(),
            _ => return Err(tile),
        };
        self.group_reserved.retain(|(g, _)| *g != gid);

        tile.floating_pos = Some(self.logical_to_size_frac(cell.loc));
        tile.floating_preset_width_idx = None;
        tile.floating_preset_height_idx = None;
        self.add_tile(tile, true);
        for (id, cell) in cells {
            let Some(i) = self.idx_of(id) else {
                continue;
            };
            match &mut self.data[i].group {
                Some(slot) => slot.id = gid,
                None => {
                    self.data[i].group = Some(GroupSlot {
                        id: gid,
                        cell: self.tile_rect(i),
                    })
                }
            }
            self.set_cell(i, *cell, true);
        }
        if let Some(i) = self.idx_of(&picked) {
            self.tiles[i].request_tile_size(cell.size, true, None);
        }
        self.interactive_resize_end(None);
        Ok(())
    }

    /// Other members of `id`'s group, in stacking order.
    #[cfg(test)]
    pub fn group_mates(&self, id: &W::Id) -> Vec<W::Id> {
        let Some(idx) = self.idx_of(id) else {
            return Vec::new();
        };
        let Some(slot) = self.data[idx].group else {
            return Vec::new();
        };
        self.group_members(slot.id)
            .into_iter()
            .filter(|&i| i != idx)
            .map(|i| self.tiles[i].window().id().clone())
            .collect()
    }

    /// Group cell of `id` in view coordinates.
    #[cfg(test)]
    pub fn group_cell(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        let idx = self.idx_of(id)?;
        self.data[idx].group.map(|s| s.cell)
    }

    /// Group of `id`, if it is a floating group member.
    pub fn group_id_of(&self, id: &W::Id) -> Option<FloatingGroupId> {
        let idx = self.idx_of(id)?;
        self.data[idx].group.map(|s| s.id)
    }

    /// Members of `id`'s group with their cells (view coordinates), in reading order (top to
    /// bottom, left to right).
    pub fn group_cells(&self, id: &W::Id) -> Vec<(W::Id, Rectangle<f64, Logical>)> {
        let Some(gid) = self.group_id_of(id) else {
            return Vec::new();
        };
        let mut out: Vec<_> = self
            .group_members(gid)
            .into_iter()
            .map(|i| (self.tiles[i].window().id().clone(), self.cell_of(i)))
            .collect();
        out.sort_by(|(_, a), (_, b)| {
            f64::total_cmp(&a.loc.y, &b.loc.y).then(f64::total_cmp(&a.loc.x, &b.loc.x))
        });
        out
    }

    /// Group box (bar included) of `id`'s group in view coordinates.
    pub fn group_box_of(&self, id: &W::Id) -> Option<Rectangle<f64, Logical>> {
        let mut cells: Vec<_> = self.group_cells(id).into_iter().map(|(_, c)| c).collect();
        if cells.is_empty() {
            return None;
        }
        let gid = self.group_id_of(id)?;
        cells.extend(self.reserved_cell(gid));
        Some(group_box(&cells, self.options.layout.gaps))
    }

    /// Set the cells of existing group members.
    pub fn set_group_cells(&mut self, cells: &[(W::Id, Rectangle<f64, Logical>)], animate: bool) {
        for (id, cell) in cells {
            if let Some(i) = self.idx_of(id) {
                if self.data[i].group.is_some() {
                    self.set_cell(i, *cell, animate);
                }
            }
        }
    }

    /// Clamp a group box position so the box stays reachable in the viewport. Vertically the L's
    /// bottom bar top may meet the title-bar band top (group can rise and cut/fade there); the
    /// member title bars still cannot sink past the band bottom.
    pub fn clamp_group_loc(
        &self,
        loc: Point<f64, Logical>,
        size: Size<f64, Logical>,
    ) -> Point<f64, Logical> {
        let area = self.working_area;
        let min_on = f64::clamp(size.w / 4., 10., 75.);
        let max_off = f64::max(0., size.w - min_on);
        let x = loc
            .x
            .max(area.loc.x - max_off)
            .min(area.loc.x + area.size.w - size.w + max_off);
        let y = clamp_group_y(loc.y, size.h, self.title_bar_band());
        Point::from((x, y))
    }

    /// Move `id`'s whole group so its box (bar included) starts at `loc` (clamped).
    pub fn move_group_box_to(&mut self, id: &W::Id, loc: Point<f64, Logical>, animate: bool) {
        let Some(gb) = self.group_box_of(id) else {
            return;
        };
        let loc = self.clamp_group_loc(loc, gb.size);
        let delta = loc - gb.loc;
        let cells: Vec<_> = self
            .group_cells(id)
            .into_iter()
            .map(|(id, mut c)| {
                c.loc += delta;
                (id, c)
            })
            .collect();
        self.set_group_cells(&cells, animate);
    }

    /// Scale group `gid` so its box (bar included) becomes `to`.
    fn scale_group_to(&mut self, gid: FloatingGroupId, to: Rectangle<f64, Logical>) {
        let members = self.group_members(gid);
        let cells: Vec<_> = members.iter().map(|&i| self.cell_of(i)).collect();
        let from = union_all(&cells);
        let to = content_box(to, self.options.layout.gaps);
        let scaled = scale_cells(&cells, from, to, self.options.layout.gaps);
        for (&i, cell) in zip(&members, scaled) {
            self.tiles[i].floating_preset_width_idx = None;
            self.tiles[i].floating_preset_height_idx = None;
            self.set_cell(i, cell, true);
        }
    }

    /// Group Half: the group box becomes a 50% tiled column wide, keeping its height and
    /// proportions. Returns the `(x, w)` of the new box.
    pub fn group_half(&mut self, id: &W::Id, band: Rectangle<f64, Logical>) -> Option<(f64, f64)> {
        let gid = self.group_id_of(id)?;
        let gb = self.group_box_of(id)?;
        let gaps = self.options.layout.gaps;
        let w = ((band.size.w - gaps) * 0.5 - gaps).max(group_extra(gaps).w + 1.);
        let to = Rectangle::new(gb.loc, Size::from((w, gb.size.h)));
        self.group_restore.retain(|(g, _)| *g != gid);
        self.scale_group_to(gid, to);
        self.interactive_resize_end(None);
        Some((to.loc.x, to.size.w))
    }

    /// Lime Half on member `id` inside its group box (see [`half_member`]).
    pub fn group_member_half(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };
        let Some(slot) = self.data[idx].group else {
            return false;
        };
        if self.reserved_cell(slot.id).is_some() {
            return false;
        }
        let members = self.group_members(slot.id);
        let cells: Vec<_> = members.iter().map(|&i| self.cell_of(i)).collect();
        let Some(pos) = members.iter().position(|&i| i == idx) else {
            return false;
        };
        let out = half_member(
            &cells,
            &self.members_cell_limits(&members),
            pos,
            self.options.layout.gaps,
        );
        for (&i, cell) in zip(&members, out) {
            self.tiles[i].floating_preset_width_idx = None;
            self.tiles[i].floating_preset_height_idx = None;
            self.set_cell(i, cell, true);
        }
        self.interactive_resize_end(None);
        true
    }

    /// Group Fullscreen: scale the group box to the whole band (no gaps); a second call
    /// restores the previous box. Returns the `(x, w)` of the new box.
    pub fn group_fullscreen_toggle(
        &mut self,
        id: &W::Id,
        band: Rectangle<f64, Logical>,
    ) -> Option<(f64, f64)> {
        let gid = self.group_id_of(id)?;
        let gb = self.group_box_of(id)?;
        let to = if let Some(pos) = self.group_restore.iter().position(|(g, _)| *g == gid) {
            self.group_restore.remove(pos).1
        } else {
            self.group_restore.push((gid, gb));
            Rectangle::new(band.loc, band.size)
        };
        self.scale_group_to(gid, to);
        self.interactive_resize_end(None);
        Some((to.loc.x, to.size.w))
    }

    /// Remove every member of `id`'s group without redistributing; returns the tiles with their
    /// cells (view coordinates) in reading order.
    pub fn take_group(&mut self, id: &W::Id) -> Vec<(Tile<W>, Rectangle<f64, Logical>)> {
        let cells = self.group_cells(id);
        let mut out = Vec::with_capacity(cells.len());
        for (id, cell) in cells {
            let Some(i) = self.idx_of(&id) else {
                continue;
            };
            self.data[i].group = None;
            out.push((self.remove_tile_by_idx(i).tile, cell));
        }
        out
    }

    /// Add `members` as a new floating group with the given cells (view coordinates). The
    /// first member is stacked on top and activated if `activate`.
    pub fn add_group(&mut self, members: Vec<(Tile<W>, Rectangle<f64, Logical>)>, activate: bool) {
        if members.is_empty() {
            return;
        }
        if members.len() == 1 {
            let (mut tile, cell) = members.into_iter().next().unwrap();
            tile.floating_pos = Some(self.logical_to_size_frac(cell.loc));
            self.add_tile(tile, activate);
            return;
        }
        let gid = FloatingGroupId::next();
        let mut ids = Vec::with_capacity(members.len());
        for (mut tile, cell) in members.into_iter().rev() {
            tile.floating_pos = Some(self.logical_to_size_frac(cell.loc));
            tile.floating_preset_width_idx = None;
            tile.floating_preset_height_idx = None;
            ids.push((tile.window().id().clone(), cell));
            self.add_tile(tile, false);
        }
        for (id, cell) in &ids {
            let i = self.idx_of(id).unwrap();
            self.data[i].group = Some(GroupSlot {
                id: gid,
                cell: self.tile_rect(i),
            });
            self.tiles[i].request_tile_size(cell.size, false, None);
            self.set_cell(i, *cell, false);
        }
        if activate {
            let first = ids.last().unwrap().0.clone();
            self.activate_window(&first);
        }
    }

    /// Group bar under `pos` (workspace view coordinates), topmost first: a member id of the
    /// group and what was hit. Bars below a window covering `pos` are skipped.
    pub fn group_bar_under(
        &self,
        pos: Point<f64, Logical>,
        view_pos: f64,
    ) -> Option<(W::Id, GroupBarHit)> {
        let strip = Point::from((pos.x + view_pos, pos.y));
        for idx in 0..self.tiles.len() {
            if self.data[idx].minimized.is_some() {
                continue;
            }
            if let Some(slot) = self.data[idx].group {
                let mut cells: Vec<_> = self
                    .group_members(slot.id)
                    .into_iter()
                    .map(|i| self.cell_of(i))
                    .collect();
                cells.extend(self.reserved_cell(slot.id));
                let cells_box = union_all(&cells);
                let gap = self.options.layout.gaps;
                let bar = side_bar_rect(cells_box, gap);
                let bottom_w = bottom_bar_w(cells_box, gap);
                if let Some(hit) = group_bar_hit_at(bar.size.h, bottom_w, strip - bar.loc) {
                    let mut target = idx;
                    if let GroupBarHit::Edge(edges) = hit {
                        let members = self.group_members(slot.id);
                        let member_cells: Vec<_> =
                            members.iter().map(|&i| self.cell_of(i)).collect();
                        if let Some(pos) = member_on_edges(&member_cells, edges) {
                            target = members[pos];
                        }
                    }
                    return Some((self.tiles[target].window().id().clone(), hit));
                }
            }
            if self.tile_rect(idx).contains(strip) {
                return None;
            }
        }
        None
    }

    /// Update the hovered bar ball. Returns true if anything changed.
    pub fn update_group_bar_hover(
        &mut self,
        pos: Option<Point<f64, Logical>>,
        view_pos: f64,
    ) -> bool {
        let hovered = pos.and_then(|pos| {
            let (id, hit) = self.group_bar_under(pos, view_pos)?;
            let GroupBarHit::Ball(action) = hit else {
                return None;
            };
            let gid = self.group_id_of(&id)?;
            let i = (0..super::window_chrome::BALL_COUNT)
                .find(|&i| super::window_chrome::ball_action(i) == action)?;
            Some((gid, i))
        });
        let mut changed = false;
        for (gid, chrome) in &mut self.group_chromes {
            let ball = hovered.filter(|(g, _)| g == gid).map(|(_, i)| i);
            changed |= chrome.set_hover(ball);
        }
        changed
    }

    fn move_by(&mut self, amount: Point<f64, Logical>) {
        let Some(active_id) = &self.active_window_id else {
            return;
        };
        let idx = self.idx_of(active_id).unwrap();

        let new_pos = self.data[idx].logical_pos + amount;
        self.move_to(idx, new_pos, true)
    }

    pub fn move_left(&mut self) {
        self.move_by(Point::from((-DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_right(&mut self) {
        self.move_by(Point::from((DIRECTIONAL_MOVE_PX, 0.)));
    }

    pub fn move_up(&mut self) {
        self.move_by(Point::from((0., -DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_down(&mut self) {
        self.move_by(Point::from((0., DIRECTIONAL_MOVE_PX)));
    }

    pub fn move_window(
        &mut self,
        id: Option<&W::Id>,
        x: PositionChange,
        y: PositionChange,
        animate: bool,
    ) {
        let Some(id) = id.or(self.active_window_id.as_ref()) else {
            return;
        };
        let idx = self.idx_of(id).unwrap();

        let mut pos = self.data[idx].logical_pos;

        let available_width = self.working_area.size.w;
        let available_height = self.working_area.size.h;
        let working_area_loc = self.working_area.loc;

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

        self.move_to(idx, pos, animate);
    }

    pub fn center_window(&mut self, id: Option<&W::Id>, view_pos: f64) {
        let Some(id) = id.or(self.active_window_id.as_ref()).cloned() else {
            return;
        };
        let idx = self.idx_of(&id).unwrap();

        let mut new_pos =
            center_preferring_top_left_in_area(self.working_area, self.data[idx].size);
        new_pos.x += view_pos;
        self.move_to(idx, new_pos, true);
    }

    pub fn descendants_added(&mut self, id: &W::Id) -> bool {
        let Some(idx) = self.idx_of(id) else {
            return false;
        };

        self.bring_up_descendants_of(idx);
        true
    }

    pub fn update_window(&mut self, id: &W::Id, serial: Option<Serial>) -> bool {
        let Some(tile_idx) = self.idx_of(id) else {
            return false;
        };

        let tile = &mut self.tiles[tile_idx];
        let data = &mut self.data[tile_idx];

        let resize = tile.window_mut().interactive_resize_data();

        // Do this before calling update_window() so it can get up-to-date info.
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }

        let prev_size = data.size;

        tile.update_window();
        data.update(tile);

        if let Some(slot) = data.group {
            // Grouped tiles stay anchored at their cell.
            if data.logical_pos != slot.cell.loc {
                data.set_logical_pos(slot.cell.loc);
            }
        } else if let Some(resize) = resize {
            // When resizing by top/left edge, update the position accordingly.
            let mut offset = Point::from((0., 0.));
            if resize.edges.contains(ResizeEdge::LEFT) {
                offset.x += prev_size.w - data.size.w;
            }
            if resize.edges.contains(ResizeEdge::TOP) {
                offset.y += prev_size.h - data.size.h;
            }
            data.set_logical_pos(data.logical_pos + offset);
        }

        true
    }

    pub fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        view_rect: Rectangle<f64, Logical>,
        view_pos: f64,
        focus_ring: bool,
        push: &mut dyn FnMut(FloatingSpaceRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);

        // Draw the closing windows on top of the other windows.
        //
        // FIXME: I guess this should rather preserve the stacking order when the window is closed.
        for closing in self.closing_windows.iter().rev() {
            let elem = closing.render(ctx.as_gles(), view_rect, scale);
            push(elem.into());
        }

        let active = self.active_window_id.clone();
        let mut bars_done: Vec<FloatingGroupId> = Vec::new();
        for (idx, (tile, tile_pos)) in self.tiles_with_render_positions().enumerate() {
            if self.data[idx].minimized.is_some() {
                continue;
            }
            let tile_pos = Point::from((tile_pos.x - view_pos, tile_pos.y));
            // For the active tile, draw the focus ring.
            let focus_ring = focus_ring && Some(tile.window().id()) == active.as_ref();

            // The bar stacks with its group's topmost member.
            if let Some(slot) = self.data[idx].group {
                if !bars_done.contains(&slot.id) {
                    bars_done.push(slot.id);
                    let loc = self.group_bar_loc(slot.id, view_pos);
                    if let Some((_, chrome)) =
                        self.group_chromes.iter().find(|(g, _)| *g == slot.id)
                    {
                        chrome.set_clips(tile.bottom_clip(), tile.top_clip());
                        chrome.render_layers(ctx.r(), loc, xray_pos.offset(loc), &mut |elem| {
                            push(elem.into())
                        });
                        chrome.set_clips(None, None);
                    }
                }
            }

            let xray_pos = xray_pos.offset(tile_pos);
            tile.render(ctx.r(), tile_pos, xray_pos, focus_ring, &mut |elem| {
                push(elem.into())
            });
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        if let Some(idx) = self.idx_of(&window) {
            self.leave_expand(idx);
        }

        let tile = self
            .tiles
            .iter_mut()
            .find(|tile| tile.window().id() == &window)
            .unwrap();

        let original_window_size = tile.window_size();

        let resize = InteractiveResize {
            window: window.clone(),
            original_window_size,
            data: InteractiveResizeData { edges },
        };
        self.interactive_resize = Some(resize);
        self.group_resize_start = self.idx_of(&window).and_then(|idx| {
            let slot = self.data[idx].group?;
            let cells = self
                .group_members(slot.id)
                .into_iter()
                .map(|i| (self.tiles[i].window().id().clone(), self.cell_of(i)))
                .collect();
            Some(cells)
        });

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

        let original_window_size = resize.original_window_size;
        let edges = resize.data.edges;

        if let Some(start) = &self.group_resize_start {
            let Some(pos) = start.iter().position(|(id, _)| id == window) else {
                return false;
            };
            let cells: Vec<_> = start.iter().map(|(_, cell)| *cell).collect();
            let ids: Vec<_> = start.iter().map(|(id, _)| id.clone()).collect();
            let gaps = self.options.layout.gaps;
            let member_limits: Vec<_> = ids
                .iter()
                .filter_map(|id| self.idx_of(id).map(|i| self.member_cell_limits(i)))
                .collect();
            if member_limits.len() != cells.len() {
                return false;
            }
            let out = resize_member(
                &cells,
                &member_limits,
                pos,
                edges,
                delta,
                gaps,
                self.cell_limits(),
            );
            for (id, cell) in zip(ids, out) {
                if let Some(i) = self.idx_of(&id) {
                    self.set_cell(i, cell, false);
                }
            }
            return true;
        }

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let mut dx = delta.x;
            if edges.contains(ResizeEdge::LEFT) {
                dx = -dx;
            };

            let window_width = (original_window_size.w + dx).round() as i32;
            self.set_window_width(Some(window), SizeChange::SetFixed(window_width), false);
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            let mut dy = delta.y;
            if edges.contains(ResizeEdge::TOP) {
                dy = -dy;
            };

            let window_height = (original_window_size.h + dy).round() as i32;
            self.set_window_height(Some(window), SizeChange::SetFixed(window_height), false);
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
        }

        self.interactive_resize = None;
        self.group_resize_start = None;
    }

    pub fn refresh(&mut self, is_active: bool, is_focused: bool) {
        let active = self.active_window_id.clone();
        for (tile, data) in zip(&mut self.tiles, &self.data) {
            let win = tile.window_mut();

            win.set_active_in_column(true);
            let tiled = matches!(data.minimized, Some(MinimizedFrom::Tiled { .. }));
            win.set_floating(!tiled);
            win.set_minimized(data.minimized.is_some());

            let mut is_active =
                is_active && Some(win.id()) == active.as_ref() && data.minimized.is_none();
            if self.options.deactivate_unfocused_windows {
                is_active &= is_focused;
            }
            win.set_activated(is_active);

            let resize_data = self
                .interactive_resize
                .as_ref()
                .filter(|resize| &resize.window == win.id())
                .map(|resize| resize.data);
            win.set_interactive_resize(resize_data);

            let border_config = self.options.layout.border.merged_with(&win.rules().border);
            let bounds = compute_toplevel_bounds(border_config, self.working_area.size);
            win.set_bounds(bounds);

            // If transactions are disabled, also disable combined throttling, for more
            // intuitive behavior.
            let intent = if self.options.disable_resize_throttling {
                ConfigureIntent::CanSend
            } else {
                win.configure_intent()
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

    pub fn clamp_within_working_area(
        &self,
        pos: Point<f64, Logical>,
        size: Size<f64, Logical>,
    ) -> Point<f64, Logical> {
        let mut rect = Rectangle::new(pos, size);
        clamp_preferring_top_left_in_area(self.working_area, &mut rect);
        // Y: title bar in the tiled scrolling band (not the full parent area).
        rect.loc.y = clamp_title_bar_y(pos.y, size.h, self.title_bar_band());
        rect.loc
    }

    /// Tiled scrolling working area (exclusive zone + struts).
    fn vertical_band(&self) -> Rectangle<f64, Logical> {
        compute_working_area(self.working_area, self.scale, self.options.layout.struts)
    }

    /// Band the SSD title bars are clamped into, see [`title_bar_band`].
    fn title_bar_band(&self) -> Rectangle<f64, Logical> {
        title_bar_band(self.working_area, self.scale, &self.options)
    }

    /// Top of the title-bar band: floating groups fade/cut here when rising.
    pub fn title_bar_band_top(&self) -> f64 {
        self.title_bar_band().loc.y
    }

    pub fn has_groups(&self) -> bool {
        self.data.iter().any(|d| d.group.is_some())
    }

    /// Apply bottom (and for group members, top) edge clips for the next render.
    pub fn set_edge_clips(&self, bottom: f64, top: f64) {
        for (tile, data) in zip(&self.tiles, &self.data) {
            tile.set_bottom_clip(Some(bottom));
            if data.group.is_some() {
                tile.set_top_clip(Some(top));
            }
        }
    }

    pub fn clear_edge_clips(&self) {
        for tile in &self.tiles {
            tile.set_bottom_clip(None);
            tile.set_top_clip(None);
        }
    }

    pub fn scale_by_working_area(&self, pos: Point<f64, SizeFrac>) -> Point<f64, Logical> {
        Data::scale_by_working_area(self.working_area, pos)
    }

    pub fn logical_to_size_frac(&self, logical_pos: Point<f64, Logical>) -> Point<f64, SizeFrac> {
        Data::logical_to_size_frac_in_working_area(self.working_area, logical_pos)
    }

    fn move_and_animate(&mut self, idx: usize, new_pos: Point<f64, Logical>) {
        // Moves up to this logical pixel distance are not animated.
        const ANIMATION_THRESHOLD_SQ: f64 = 10. * 10.;

        let tile = &mut self.tiles[idx];
        let data = &mut self.data[idx];

        let prev_pos = data.logical_pos;
        data.set_logical_pos(new_pos);
        let new_pos = data.logical_pos;

        let diff = prev_pos - new_pos;
        if diff.x * diff.x + diff.y * diff.y > ANIMATION_THRESHOLD_SQ {
            tile.animate_move_from(prev_pos - new_pos);
        }
    }

    pub fn new_window_size(
        &self,
        width: Option<PresetSize>,
        height: Option<PresetSize>,
        rules: &ResolvedWindowRules,
    ) -> Size<i32, Logical> {
        let border = self.options.layout.border.merged_with(&rules.border);

        let resolve = |size: Option<PresetSize>, working_area_size: f64| {
            if let Some(size) = size {
                let size = match resolve_preset_size(size, working_area_size) {
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
            }
        };

        let width = resolve(width, self.working_area.size.w);
        let height = resolve(height, self.working_area.size.h);

        Size::from((width, height))
    }

    pub fn stored_or_default_tile_pos(&self, tile: &Tile<W>) -> Option<Point<f64, Logical>> {
        let pos = tile.floating_pos.map(|pos| self.scale_by_working_area(pos));
        pos.or_else(|| {
            tile.window().rules().default_floating_position.map(|pos| {
                let relative_to = pos.relative_to;
                let size = tile.tile_size();
                let area = self.working_area;

                let mut pos = Point::from((pos.x.0, pos.y.0));
                if relative_to == RelativeTo::TopRight
                    || relative_to == RelativeTo::BottomRight
                    || relative_to == RelativeTo::Right
                {
                    pos.x = area.size.w - size.w - pos.x;
                }
                if relative_to == RelativeTo::BottomLeft
                    || relative_to == RelativeTo::BottomRight
                    || relative_to == RelativeTo::Bottom
                {
                    pos.y = area.size.h - size.h - pos.y;
                }
                if relative_to == RelativeTo::Top || relative_to == RelativeTo::Bottom {
                    pos.x += area.size.w / 2.0 - size.w / 2.0
                }
                if relative_to == RelativeTo::Left || relative_to == RelativeTo::Right {
                    pos.y += area.size.h / 2.0 - size.h / 2.0
                }

                pos + self.working_area.loc
            })
        })
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    #[cfg(test)]
    pub fn scale(&self) -> f64 {
        self.scale
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
    pub fn verify_invariants(&self) {
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());
        assert_eq!(self.tiles.len(), self.data.len());

        for (i, (tile, data)) in zip(&self.tiles, &self.data).enumerate() {
            assert!(Rc::ptr_eq(&self.options, &tile.options));
            assert_eq!(self.view_size, tile.view_size());
            assert_eq!(self.clock, tile.clock);
            assert_eq!(self.scale, tile.scale());
            tile.verify_invariants();

            if let Some(idx) = tile.floating_preset_width_idx {
                assert!(idx < self.options.layout.preset_column_widths.len());
            }
            if let Some(idx) = tile.floating_preset_height_idx {
                assert!(idx < self.options.layout.preset_window_heights.len());
            }

            let mode = tile.window().pending_sizing_mode();
            assert!(
                mode.is_normal() || mode.is_fullscreen(),
                "floating windows can only be normal or fullscreen, got {mode:?}"
            );
            if mode.is_fullscreen() {
                assert_eq!(
                    tile.floating_expand,
                    super::tile::FloatingExpand::Output,
                    "floating fullscreen is the orange (output) mode"
                );
            }

            data.verify_invariants();

            if let Some(slot) = data.group {
                let members = self.group_members(slot.id);
                let reserved = self.reserved_cell(slot.id);
                let slots = members.len() + usize::from(reserved.is_some());
                assert!(
                    (2..=MAX_GROUP_MEMBERS).contains(&slots),
                    "floating groups have 2..={MAX_GROUP_MEMBERS} members, got {slots}"
                );
                if let Some(reserved) = reserved {
                    assert!(
                        !super::floating_group::rects_overlap(slot.cell, reserved),
                        "group cells must not overlap the reserved cell"
                    );
                }
                for &j in &members {
                    if j != i {
                        let other = self.cell_of(j);
                        assert!(
                            !super::floating_group::rects_overlap(slot.cell, other),
                            "group cells must not overlap: {:?} vs {other:?}",
                            slot.cell
                        );
                    }
                }
            }

            let mut data2 = *data;
            data2.update(tile);
            data2.update_config(self.working_area, self.title_bar_band());
            assert_eq!(data, &data2, "tile data must be up to date");

            for tile_below in &self.tiles[i + 1..] {
                assert!(
                    !tile_below.window().is_child_of(tile.window()),
                    "children must be stacked above parents"
                );
            }
        }

        if let Some(id) = &self.active_window_id {
            assert!(!self.tiles.is_empty());
            assert!(self.contains(id), "active window must be present in tiles");
        } else {
            assert!(self.tiles.is_empty());
        }

        if let Some(resize) = &self.interactive_resize {
            assert!(
                self.contains(&resize.window),
                "interactive resize window must be present in tiles"
            );
        }
    }
}

/// Clamp floating tile Y so the SSD title bar stays fully inside `band`.
///
/// `tile_h` is `Tile::tile_size().h` (includes chrome extra height). The title
/// bar is the top of the tile; the window body may extend past `band.bottom`.
fn clamp_title_bar_y(y: f64, tile_h: f64, band: Rectangle<f64, Logical>) -> f64 {
    let chrome_h = f64::min(WindowChrome::extra_height(), tile_h.max(0.));
    let min_y = band.loc.y;
    let max_y = f64::max(min_y, band.loc.y + band.size.h - chrome_h);
    f64::clamp(y, min_y, max_y)
}

/// Clamp a floating group box Y: the L bottom bar may rise until its top meets `band.top`
/// (content above that line is cut/faded); member title bars still cannot sink past the band
/// bottom.
fn clamp_group_y(y: f64, group_h: f64, band: Rectangle<f64, Logical>) -> f64 {
    let min_y = band.loc.y + GROUP_BOTTOM_H - group_h.max(GROUP_BOTTOM_H);
    let title_h = f64::min(WindowChrome::extra_height(), group_h.max(0.));
    let max_y = f64::max(min_y, band.loc.y + band.size.h - title_h);
    f64::clamp(y, min_y, max_y)
}

/// Tiled scrolling working area with the bottom raised to the bottom clip line
/// (`band.bottom - gaps`), so a clamped title bar always stays visible and clickable.
fn title_bar_band(
    working_area: Rectangle<f64, Logical>,
    scale: f64,
    options: &Options,
) -> Rectangle<f64, Logical> {
    let mut band = compute_working_area(working_area, scale, options.layout.struts);
    band.size.h = f64::max(0., band.size.h - options.layout.gaps);
    band
}

fn compute_toplevel_bounds(
    border_config: niri_config::Border,
    working_area_size: Size<f64, Logical>,
) -> Size<i32, Logical> {
    let mut border = 0.;
    if !border_config.off {
        border = border_config.width * 2.;
    }

    Size::from((
        f64::max(working_area_size.w - border, 1.),
        f64::max(working_area_size.h - border, 1.),
    ))
    .to_i32_floor()
}

fn resolve_preset_size(preset: PresetSize, view_size: f64) -> ResolvedSize {
    match preset {
        PresetSize::Proportion(proportion) => ResolvedSize::Tile(view_size * proportion),
        PresetSize::Fixed(width) => ResolvedSize::Window(f64::from(width)),
    }
}

#[cfg(test)]
mod title_bar_clamp_tests {
    use super::*;

    fn band() -> Rectangle<f64, Logical> {
        Rectangle::new(Point::from((0., 24.)), Size::from((1280., 648.)))
    }

    #[test]
    fn title_bar_cannot_go_above_band_top() {
        let band = band();
        let y = clamp_title_bar_y(-2000., 226., band);
        assert!(y >= band.loc.y);
        assert_eq!(y, band.loc.y);
    }

    #[test]
    fn title_bar_cannot_go_below_band_bottom() {
        let band = band();
        let chrome = WindowChrome::extra_height();
        let y = clamp_title_bar_y(8000., 226., band);
        let title_bar_bottom = y + chrome;
        let band_bottom = band.loc.y + band.size.h;
        assert!(title_bar_bottom <= band_bottom);
        assert_eq!(title_bar_bottom, band_bottom);
        assert!(y + 226. > band_bottom);
    }

    #[test]
    fn group_can_rise_until_l_bottom_bar_meets_band_top() {
        let band = band();
        let group_h = 400.;
        let y = clamp_group_y(-2000., group_h, band);
        let expected = band.loc.y + GROUP_BOTTOM_H - group_h;
        assert_eq!(y, expected);
        // Bottom bar top sits on band.top; bottom bar remains below the cut line.
        assert_eq!(y + group_h - GROUP_BOTTOM_H, band.loc.y);
    }
}
