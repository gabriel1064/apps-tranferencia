use smithay::backend::input::ButtonState;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, CursorImageStatus, GestureHoldBeginEvent, GestureHoldEndEvent,
    GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent,
    GestureSwipeEndEvent, GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData,
    MotionEvent, PointerGrab, PointerInnerHandle, RelativeMotionEvent,
};
use smithay::input::SeatHandler;
use smithay::utils::{Logical, Point};

use crate::niri::State;

/// Pointer movement past which a press on a group bar becomes a held drag.
const HELD_DRAG_THRESHOLD: f64 = 8.;

/// Whole-group move from its side bar.
///
/// Click picks the group up (click-to-drop into tiling as one group column on the next
/// click); press-hold-drag then release leaves it floating where released. Esc and right
/// click cancel.
pub struct GroupMoveGrab {
    start_data: PointerGrabStartData<State>,
    held_drag: bool,
    awaiting_drop_click: bool,
}

impl GroupMoveGrab {
    pub fn new(start_data: PointerGrabStartData<State>) -> Self {
        Self {
            start_data,
            held_drag: false,
            awaiting_drop_click: false,
        }
    }

    fn on_motion(&mut self, data: &mut State, location: Point<f64, Logical>) {
        if let Some((output, pos)) = data.niri.output_under(location) {
            let output = output.clone();
            data.niri.layout.group_drag_motion(&output, pos);
        }
        if !self.held_drag && !self.awaiting_drop_click {
            let c = location - self.start_data.location;
            if c.x * c.x + c.y * c.y >= HELD_DRAG_THRESHOLD * HELD_DRAG_THRESHOLD {
                self.held_drag = true;
                data.niri.layout.group_drag_set_held();
            }
        }
        data.niri.queue_redraw_all();
    }
}

impl PointerGrab<State> for GroupMoveGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.on_motion(data, event.location);
    }

    fn relative_motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, None, event);
    }

    fn button(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);

        const BTN_RIGHT: u32 = 0x111;
        if event.button == BTN_RIGHT && event.state == ButtonState::Pressed {
            data.niri.layout.group_drag_cancel();
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        if event.button != self.start_data.button {
            return;
        }
        match event.state {
            ButtonState::Released if !self.awaiting_drop_click => {
                if self.held_drag {
                    handle.unset_grab(self, data, event.serial, event.time, true);
                } else {
                    self.awaiting_drop_click = true;
                }
            }
            ButtonState::Pressed if self.awaiting_drop_click => {
                handle.unset_grab(self, data, event.serial, event.time, true);
            }
            _ => {}
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        if !data.niri.layout.is_overview_open() {
            let vertical = details.v120.map(|(_, v)| f64::from(v)).unwrap_or(0.);
            let horizontal = details.v120.map(|(h, _)| f64::from(h)).unwrap_or(0.);
            let delta = if vertical != 0. || horizontal != 0. {
                vertical + horizontal
            } else {
                details.axis.0 + details.axis.1
            };
            if delta != 0. {
                data.niri.layout.impulse_view(delta);
                data.niri.queue_redraw_all();
            }
            return;
        }
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
        if !data.niri.layout.group_drag_active() {
            // The group went away (e.g. a member closed and it dissolved).
            handle.unset_grab(
                self,
                data,
                smithay::utils::SERIAL_COUNTER.next_serial(),
                crate::utils::get_monotonic_time().as_millis() as u32,
                true,
            );
        }
    }

    fn gesture_swipe_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeBeginEvent,
    ) {
        handle.gesture_swipe_begin(data, event);
    }

    fn gesture_swipe_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeUpdateEvent,
    ) {
        handle.gesture_swipe_update(data, event);
    }

    fn gesture_swipe_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureSwipeEndEvent,
    ) {
        handle.gesture_swipe_end(data, event);
    }

    fn gesture_pinch_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchBeginEvent,
    ) {
        handle.gesture_pinch_begin(data, event);
    }

    fn gesture_pinch_update(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchUpdateEvent,
    ) {
        handle.gesture_pinch_update(data, event);
    }

    fn gesture_pinch_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GesturePinchEndEvent,
    ) {
        handle.gesture_pinch_end(data, event);
    }

    fn gesture_hold_begin(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldBeginEvent,
    ) {
        handle.gesture_hold_begin(data, event);
    }

    fn gesture_hold_end(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        event: &GestureHoldEndEvent,
    ) {
        handle.gesture_hold_end(data, event);
    }

    fn start_data(&self) -> &PointerGrabStartData<State> {
        &self.start_data
    }

    fn unset(&mut self, data: &mut State) {
        // No-op after a cancel (the drag is already gone).
        data.niri.layout.group_drag_drop();
        data.niri
            .cursor_manager
            .set_cursor_image(CursorImageStatus::default_named());
        data.niri.queue_redraw_all();
    }
}
