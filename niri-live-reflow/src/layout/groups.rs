//! Window groups at the layout level: side bar hits and actions, whole-group drags, and the
//! rubber-band selection that creates groups.

use niri_config::CornerRadius;
use smithay::output::Output;
use smithay::utils::{Logical, Point, Rectangle};

use super::floating_group::MAX_GROUP_MEMBERS;
use super::group_chrome::GroupBarHit;
use super::monitor::{InsertHint, InsertWorkspace};
use super::selection::{button_rect, rects_touch, selection_rect, SelectionView};
use super::window_chrome::ChromeAction;
use super::workspace::WorkspaceId;
use super::{InsertPosition, InteractiveMoveState, Layout, LayoutElement};

type Rect = Rectangle<f64, Logical>;

/// Where a dragged group came from, for Esc / right-click cancel.
#[derive(Debug)]
enum GroupDragOrigin<W: LayoutElement> {
    /// Floating group cells (view coordinates).
    Floating(Vec<(W::Id, Rect)>),
    /// Tiled group column index.
    Tiled(usize),
}

/// A whole group following the pointer. The group lives in the floating space during the
/// drag; in tiling mode the drop turns it into a tiled group column.
#[derive(Debug)]
pub struct GroupDrag<W: LayoutElement> {
    output: Output,
    ws_id: WorkspaceId,
    lead: W::Id,
    /// Click-to-drop into tiling (true) or held drag that stays floating (false).
    tiling: bool,
    origin: GroupDragOrigin<W>,
    /// Pointer minus group box origin, workspace view coordinates.
    pointer_offset: Point<f64, Logical>,
    /// Pointer within the output.
    pointer: Point<f64, Logical>,
    /// Column index a tiling drop inserts at.
    insert_idx: usize,
}

/// Rubber-band selection on empty desktop.
#[derive(Debug)]
pub struct Selection<W: LayoutElement> {
    output: Output,
    ws_id: WorkspaceId,
    start: Point<f64, Logical>,
    current: Point<f64, Logical>,
    selected: Vec<W::Id>,
    /// "Criar grupo" button after release, output coordinates.
    button: Option<Rect>,
    button_hovered: bool,
}

impl<W: LayoutElement> Layout<W> {
    /// Group side bar under the pointer: a member id, what was hit, and whether the group is
    /// floating.
    pub fn group_bar_hit_at(
        &self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> Option<(W::Id, GroupBarHit, bool)> {
        if self.interactive_move.is_some() || self.group_drag.is_some() || self.overview_open {
            return None;
        }
        let mon = self.monitor_for_output(output)?;
        mon.active_workspace_ref()
            .group_bar_under(pos_within_output)
    }

    /// Track hovered group bar balls. Returns true if anything changed.
    pub fn update_group_bar_hover(
        &mut self,
        pointer: Option<(&Output, Point<f64, Logical>)>,
    ) -> bool {
        let target = if self.interactive_move.is_some() || self.group_drag.is_some() {
            None
        } else {
            pointer.and_then(|(output, pos)| {
                let mon = self.monitor_for_output(output)?;
                Some((mon.active_workspace_ref().id(), pos))
            })
        };
        let mut changed = false;
        for ws in self.workspaces_mut() {
            let pos = target.filter(|(id, _)| *id == ws.id()).map(|(_, p)| p);
            changed |= ws.update_group_bar_hover(pos);
        }
        changed
    }

    /// Members of `id`'s group (floating or tiled); empty when `id` is not grouped.
    pub fn group_member_ids(&self, id: &W::Id) -> Vec<W::Id> {
        self.workspaces()
            .find(|(_, _, ws)| ws.has_window(id))
            .map(|(_, _, ws)| ws.group_member_ids(id))
            .unwrap_or_default()
    }

    /// Run a side bar ball action on `id`'s group. Close is left to the caller (it needs the
    /// toplevels). Returns whether anything was done.
    pub fn group_bar_action(&mut self, id: &W::Id, action: ChromeAction) -> bool {
        match action {
            ChromeAction::Close => false,
            ChromeAction::Minimize => {
                let ids = self.group_member_ids(id);
                for member in &ids {
                    self.minimize_window(member);
                }
                !ids.is_empty()
            }
            _ => {
                let Some(ws) = self.workspaces_mut().find(|ws| ws.has_window(id)) else {
                    return false;
                };
                ws.group_resize_action(id, action)
            }
        }
    }

    /// Lime Half on a group member's own title bar. Returns false when `id` is not grouped.
    pub fn group_member_half(&mut self, id: &W::Id) -> bool {
        let Some(ws) = self
            .workspaces_mut()
            .find(|ws| ws.has_window(id) && ws.is_group_member(id))
        else {
            return false;
        };
        ws.group_member_half(id)
    }

    pub fn group_drag_active(&self) -> bool {
        self.group_drag.is_some()
    }

    pub fn group_drag_is_tiling(&self) -> bool {
        self.group_drag.as_ref().is_some_and(|d| d.tiling)
    }

    /// Pick up `id`'s whole group by its bar. A tiled group is floated in place first. Starts
    /// in click-to-drop tiling mode.
    pub fn group_drag_begin(
        &mut self,
        output: &Output,
        id: &W::Id,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        if self.interactive_move.is_some() || self.group_drag.is_some() || self.overview_open {
            return false;
        }
        let Some(mon) = self.monitor_for_output_mut(output) else {
            return false;
        };
        let ws = mon.active_workspace();
        if !ws.has_window(id) || !ws.is_group_member(id) {
            return false;
        }
        let ws_id = ws.id();
        let origin = if ws.is_floating_group_member(id) {
            GroupDragOrigin::Floating(ws.floating_group_cells(id))
        } else {
            let Some(col) = ws.tiled_group_column(id) else {
                return false;
            };
            if !ws.float_tiled_group(id) {
                return false;
            }
            GroupDragOrigin::Tiled(col)
        };

        let members = ws.group_member_ids(id);
        for member in members.iter().rev() {
            ws.activate_window(member);
        }
        ws.activate_window(id);

        let Some(gb) = ws.floating_group_box(id) else {
            return false;
        };
        let insert_idx = ws.group_insert_idx(pos_within_output.x + ws.scroll_view_pos());
        self.group_drag = Some(GroupDrag {
            output: output.clone(),
            ws_id,
            lead: id.clone(),
            tiling: true,
            origin,
            pointer_offset: pos_within_output - gb.loc,
            pointer: pos_within_output,
            insert_idx,
        });
        true
    }

    pub fn group_drag_motion(&mut self, output: &Output, pos_within_output: Point<f64, Logical>) {
        let Some(drag) = &mut self.group_drag else {
            return;
        };
        if drag.output != *output {
            return;
        }
        drag.pointer = pos_within_output;
        self.group_drag_follow();
    }

    /// Keep the dragged group under the pointer (also after view pans).
    pub(super) fn group_drag_follow(&mut self) {
        let Some(drag) = &self.group_drag else {
            return;
        };
        let (ws_id, lead, pointer, offset) = (
            drag.ws_id,
            drag.lead.clone(),
            drag.pointer,
            drag.pointer_offset,
        );
        let Some(ws) = self.workspaces_mut().find(|ws| ws.id() == ws_id) else {
            self.group_drag = None;
            return;
        };
        if !ws.is_floating_group_member(&lead) {
            // The group dissolved under us (a member closed).
            self.group_drag = None;
            return;
        }
        ws.move_floating_group_to(&lead, pointer - offset, false);
        let idx = ws.group_insert_idx(pointer.x + ws.scroll_view_pos());
        if let Some(drag) = &mut self.group_drag {
            drag.insert_idx = idx;
        }
    }

    /// The press turned into a held drag: the group will stay floating where released.
    pub fn group_drag_set_held(&mut self) {
        if let Some(drag) = &mut self.group_drag {
            drag.tiling = false;
        }
    }

    /// Drop the group: into tiling as one group column (click-to-drop), or leave it floating
    /// (held drag).
    pub fn group_drag_drop(&mut self) {
        let Some(drag) = self.group_drag.take() else {
            return;
        };
        if !drag.tiling {
            return;
        }
        if let Some(ws) = self.workspaces_mut().find(|ws| ws.id() == drag.ws_id) {
            ws.tile_floating_group(&drag.lead, drag.insert_idx);
        }
    }

    /// Cancel (Esc / right-click): put the group back where it was picked up.
    pub fn group_drag_cancel(&mut self) {
        let Some(drag) = self.group_drag.take() else {
            return;
        };
        let Some(ws) = self.workspaces_mut().find(|ws| ws.id() == drag.ws_id) else {
            return;
        };
        match drag.origin {
            GroupDragOrigin::Floating(cells) => ws.set_floating_group_cells(&cells, true),
            GroupDragOrigin::Tiled(col) => {
                ws.tile_floating_group(&drag.lead, col);
            }
        }
    }

    /// Tiling insert hint of a click-to-drop group drag.
    pub(super) fn group_drag_insert_hint(&mut self) {
        let Some(drag) = &self.group_drag else {
            return;
        };
        if !drag.tiling {
            return;
        }
        let hint = InsertHint {
            workspace: InsertWorkspace::Existing(drag.ws_id),
            position: InsertPosition::NewColumn(drag.insert_idx),
            corner_radius: CornerRadius::default(),
        };
        let output = drag.output.clone();
        if let Some(mon) = self.monitor_for_output_mut(&output) {
            mon.insert_hint = Some(hint);
        }
    }

    pub fn selection_active(&self) -> bool {
        self.selection.is_some()
    }

    /// Whether the selection is released and waits for a click on its button.
    pub fn selection_awaiting_click(&self) -> bool {
        self.selection.as_ref().is_some_and(|s| s.button.is_some())
    }

    /// Start a rubber-band selection on the active workspace of `output`.
    pub fn selection_begin(
        &mut self,
        output: &Output,
        pos_within_output: Point<f64, Logical>,
    ) -> bool {
        if self.interactive_move.is_some() || self.group_drag.is_some() || self.overview_open {
            return false;
        }
        let Some(mon) = self.monitor_for_output(output) else {
            return false;
        };
        let ws_id = mon.active_workspace_ref().id();
        self.selection = Some(Selection {
            output: output.clone(),
            ws_id,
            start: pos_within_output,
            current: pos_within_output,
            selected: Vec::new(),
            button: None,
            button_hovered: false,
        });
        true
    }

    /// Windows of `ws_id` touched by `rect` (view coordinates), topmost first.
    fn windows_touching(&self, ws_id: WorkspaceId, rect: Rect) -> Vec<W::Id> {
        let Some((_, _, ws)) = self.workspaces().find(|(_, _, ws)| ws.id() == ws_id) else {
            return Vec::new();
        };
        ws.tiles_with_render_positions()
            .filter(|(_, _, visible)| *visible)
            .filter(|(tile, pos, _)| rects_touch(Rectangle::new(*pos, tile.tile_size()), rect))
            .map(|(tile, _, _)| tile.window().id().clone())
            .collect()
    }

    pub fn selection_motion(&mut self, pos_within_output: Point<f64, Logical>) {
        let Some(sel) = &mut self.selection else {
            return;
        };
        if let Some(button) = sel.button {
            sel.button_hovered = button.contains(pos_within_output);
            return;
        }
        sel.current = pos_within_output;
        let (ws_id, rect) = (sel.ws_id, selection_rect(sel.start, sel.current));
        let selected = self.windows_touching(ws_id, rect);
        if let Some(sel) = &mut self.selection {
            sel.selected = selected;
        }
    }

    /// Button release: with 2..=4 windows selected, show the "Criar grupo" button and return
    /// true; otherwise end the selection.
    pub fn selection_release(&mut self) -> bool {
        let Some(sel) = &mut self.selection else {
            return false;
        };
        if sel.button.is_some() {
            return true;
        }
        if !(2..=MAX_GROUP_MEMBERS).contains(&sel.selected.len()) {
            self.selection = None;
            return false;
        }
        let output = sel.output.clone();
        let view = self
            .monitor_for_output(&output)
            .map(|mon| mon.view_size())
            .unwrap_or_default();
        let Some(sel) = &mut self.selection else {
            return false;
        };
        sel.button = Some(button_rect(sel.current, view));
        true
    }

    /// Click while the button is shown: on the button, create the group; elsewhere, dismiss.
    /// Returns true if a group was created.
    pub fn selection_click(&mut self, pos_within_output: Point<f64, Logical>) -> bool {
        let Some(sel) = self.selection.take() else {
            return false;
        };
        if !sel.button.is_some_and(|b| b.contains(pos_within_output)) {
            return false;
        }
        self.create_group_from_windows(&sel.selected)
    }

    pub fn selection_cancel(&mut self) {
        self.selection = None;
    }

    /// Group `ids` (2..=4 windows of one workspace) into a tiled group column at the leftmost
    /// window's place.
    pub fn create_group_from_windows(&mut self, ids: &[W::Id]) -> bool {
        if !(2..=MAX_GROUP_MEMBERS).contains(&ids.len()) {
            return false;
        }
        if let Some(InteractiveMoveState::Moving(move_)) = &self.interactive_move {
            if ids.contains(move_.tile.window().id()) {
                return false;
            }
        }
        let Some(ws) = self
            .workspaces_mut()
            .find(|ws| ids.iter().all(|id| ws.has_window(id)))
        else {
            return false;
        };
        ws.create_tiled_group(ids)
    }

    /// Push the selection state to the monitors' overlays.
    pub(super) fn refresh_selection_overlay(&mut self) {
        let view = self.selection.as_ref().map(|sel| {
            let highlights = self
                .workspaces()
                .find(|(_, _, ws)| ws.id() == sel.ws_id)
                .map(|(_, _, ws)| {
                    ws.tiles_with_render_positions()
                        .filter(|(tile, _, _)| sel.selected.contains(tile.window().id()))
                        .map(|(tile, pos, _)| Rectangle::new(pos, tile.tile_size()))
                        .collect()
                })
                .unwrap_or_default();
            let view = SelectionView {
                rect: sel
                    .button
                    .is_none()
                    .then(|| selection_rect(sel.start, sel.current)),
                highlights,
                button: sel.button.map(|b| (b, sel.button_hovered)),
            };
            (sel.output.clone(), sel.ws_id, view)
        });
        for mon in self.monitors_mut() {
            let scale = mon.output().current_scale().fractional_scale();
            match &view {
                Some((output, ws_id, view)) if mon.output() == output => {
                    mon.selection_ws = Some(*ws_id);
                    mon.selection_overlay.update(view.clone(), scale);
                }
                _ => {
                    mon.selection_ws = None;
                    mon.selection_overlay
                        .update(SelectionView::default(), scale);
                }
            }
        }
    }
}
