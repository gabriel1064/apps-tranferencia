use smithay::backend::input::ButtonState;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
    PointerInnerHandle, RelativeMotionEvent,
};
use smithay::input::SeatHandler;
use smithay::output::Output;
use smithay::utils::{Logical, Point};

use crate::niri::State;

/// Rubber-band selection from a left drag on empty desktop.
///
/// While the button is held the rectangle follows the pointer and highlights every window it
/// touches. On release with 2..=4 windows a "Criar grupo" button appears next to the pointer
/// and the grab waits for one more click: on the button it creates a tiled group, anywhere
/// else it dismisses. Esc and right click cancel.
pub struct SelectGrab {
    start_data: PointerGrabStartData<State>,
    output: Output,
    released: bool,
}

impl SelectGrab {
    pub fn new(start_data: PointerGrabStartData<State>, output: Output) -> Self {
        Self {
            start_data,
            output,
            released: false,
        }
    }

    fn pos_within_output(
        &self,
        data: &State,
        location: Point<f64, Logical>,
    ) -> Point<f64, Logical> {
        data.niri
            .global_space
            .output_geometry(&self.output)
            .map(|geo| location - geo.loc.to_f64())
            .unwrap_or(location)
    }
}

impl PointerGrab<State> for SelectGrab {
    fn motion(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        _focus: Option<(<State as SeatHandler>::PointerFocus, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let pos = self.pos_within_output(data, event.location);
        data.niri.layout.selection_motion(pos);
        data.niri.queue_redraw(&self.output);
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
            data.niri.layout.selection_cancel();
            handle.unset_grab(self, data, event.serial, event.time, true);
            return;
        }

        if !self.released {
            if event.button == self.start_data.button && event.state == ButtonState::Released {
                self.released = true;
                if !data.niri.layout.selection_release() {
                    handle.unset_grab(self, data, event.serial, event.time, true);
                }
                data.niri.queue_redraw(&self.output);
            }
            return;
        }

        if event.state == ButtonState::Pressed {
            let pos = self.pos_within_output(data, handle.current_location());
            data.niri.layout.selection_click(pos);
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(
        &mut self,
        data: &mut State,
        handle: &mut PointerInnerHandle<'_, State>,
        details: AxisFrame,
    ) {
        handle.axis(data, details);
    }

    fn frame(&mut self, data: &mut State, handle: &mut PointerInnerHandle<'_, State>) {
        handle.frame(data);
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
        data.niri.layout.selection_cancel();
        data.niri.queue_redraw_all();
    }
}
