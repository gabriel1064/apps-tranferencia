use niri_config::{Color, CornerRadius};
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::focus_ring::{FocusRing, FocusRingRenderElement};
use crate::render_helpers::renderer::NiriRenderer;

pub const MOVE_ANCHOR_DIAMETER: f64 = 14.;
const ZONE_GUIDE_WIDTH: f64 = 2.;

#[derive(Debug)]
pub struct InsertHintElement {
    inner: FocusRing,
}

pub type InsertHintRenderElement = FocusRingRenderElement;

fn hint_fill_ring(config: niri_config::InsertHint) -> niri_config::FocusRing {
    niri_config::FocusRing {
        off: config.off,
        width: 0.,
        active_color: config.color,
        inactive_color: config.color,
        urgent_color: config.color,
        active_gradient: config.gradient,
        inactive_gradient: config.gradient,
        urgent_gradient: config.gradient,
    }
}

fn zone_guide_ring(config: niri_config::InsertHint) -> niri_config::FocusRing {
    let mut color = config.color;
    color.a = color.a.max(0.85);
    niri_config::FocusRing {
        off: config.off,
        width: ZONE_GUIDE_WIDTH,
        active_color: color,
        inactive_color: color,
        urgent_color: color,
        active_gradient: config.gradient,
        inactive_gradient: config.gradient,
        urgent_gradient: config.gradient,
    }
}

fn move_anchor_ring() -> niri_config::FocusRing {
    let color = Color::from_rgba8_unpremul(255, 255, 255, 240);
    niri_config::FocusRing {
        off: false,
        width: 0.,
        active_color: color,
        inactive_color: color,
        urgent_color: color,
        active_gradient: None,
        inactive_gradient: None,
        urgent_gradient: None,
    }
}

impl InsertHintElement {
    pub fn new(config: niri_config::InsertHint) -> Self {
        Self {
            inner: FocusRing::new(hint_fill_ring(config)),
        }
    }

    pub fn new_zone_guide(config: niri_config::InsertHint) -> Self {
        Self {
            inner: FocusRing::new(zone_guide_ring(config)),
        }
    }

    pub fn new_move_anchor() -> Self {
        Self {
            inner: FocusRing::new(move_anchor_ring()),
        }
    }

    pub fn update_config(&mut self, config: niri_config::InsertHint) {
        self.inner.update_config(hint_fill_ring(config));
    }

    pub fn update_zone_guide_config(&mut self, config: niri_config::InsertHint) {
        self.inner.update_config(zone_guide_ring(config));
    }

    pub fn update_shaders(&mut self) {
        self.inner.update_shaders();
    }

    pub fn update_render_elements(
        &mut self,
        size: Size<f64, Logical>,
        view_rect: Rectangle<f64, Logical>,
        radius: CornerRadius,
        scale: f64,
    ) {
        self.inner
            .update_render_elements(size, true, false, false, view_rect, radius, scale, 1.);
    }

    pub fn update_border_render_elements(
        &mut self,
        size: Size<f64, Logical>,
        view_rect: Rectangle<f64, Logical>,
        radius: CornerRadius,
        scale: f64,
    ) {
        self.inner
            .update_render_elements(size, true, true, false, view_rect, radius, scale, 1.);
    }

    pub fn render(
        &self,
        renderer: &mut impl NiriRenderer,
        location: Point<f64, Logical>,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        self.inner.render(renderer, location, push)
    }
}
