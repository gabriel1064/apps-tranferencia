//! Group chrome: an L of glass bars around a window group (floating or tiled).
//!
//! The side bar sits left of the member cells, [`GROUP_BAR_W`] wide, from below the title bars
//! of the top row down to the bottom of the bottom bar. The bottom bar, [`GROUP_BOTTOM_H`]
//! tall, runs below the cells from the side bar to the cells' right edge. Both keep a `gap`
//! from the cells. The side bar carries the same five balls as the window title bar, stacked
//! top to bottom in the title bar's left-to-right order, acting on the whole group. Clicking or
//! dragging either bar's body moves the whole group; dragging the L's outer edges (left of the
//! side bar, bottom of the bottom bar) resizes it.

use std::cell::{Cell, RefCell};
use std::iter::zip;

use niri_config::{CornerRadius, GradientInterpolation};
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Rectangle, Scale, Size};

use super::floating_group::union_all;
use super::focus_ring::FocusRingRenderElement;
use super::tile::{bottom_fade_band, crosses_bottom_clip, crosses_top_clip, top_fade_band};
use super::window_chrome::{
    ball_action, render_ball, render_icon, ChromeAction, ChromeRenderElement, WindowChrome, BALL,
    BALL_COUNT, BAR_H, GAP, SHADOW_M,
};
use crate::niri_render_elements;
use crate::render_helpers::background_effect::{
    BackgroundEffect, BackgroundEffectElement, RenderParams,
};
use crate::render_helpers::border::BorderRenderElement;
use crate::render_helpers::bottom_fade::BottomFadeRenderElement;
use crate::render_helpers::offscreen::OffscreenBuffer;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::utils::ResizeEdge;

niri_render_elements! {
    GroupChromePaint => {
        Chrome = ChromeRenderElement,
        Tint = FocusRingRenderElement,
    }
}

niri_render_elements! {
    GroupChromeLayer => {
        Chrome = ChromeRenderElement,
        Tint = FocusRingRenderElement,
        Blur = BackgroundEffectElement,
        Fade = BottomFadeRenderElement,
    }
}

/// Width of the group side bar (same thickness as the window title bar).
pub const GROUP_BAR_W: f64 = BAR_H;

/// Height of the group bottom bar (half the side bar thickness).
pub const GROUP_BOTTOM_H: f64 = 13.;

/// How far from the L's outer edges (both sides) a press resizes the whole group.
pub const GROUP_EDGE_REACH: f64 = 5.;

type Rect = Rectangle<f64, Logical>;

/// What a pointer hit on a group bar landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBarHit {
    Ball(ChromeAction),
    Body,
    /// Outer edge of the L: dragging resizes the whole group.
    Edge(ResizeEdge),
}

/// Space the chrome takes besides the member cells: on the left (side bar and gap) and at the
/// bottom (gap and bottom bar).
pub fn group_extra(gap: f64) -> Size<f64, Logical> {
    Size::from((GROUP_BAR_W + gap, gap + GROUP_BOTTOM_H))
}

/// Side bar rect for the member cells' box: from below the top row's title bars down to the
/// bottom of the bottom bar.
pub fn side_bar_rect(cells_box: Rect, gap: f64) -> Rect {
    let top = WindowChrome::extra_height().min(cells_box.size.h);
    let extra = group_extra(gap);
    Rectangle::new(
        Point::from((cells_box.loc.x - extra.w, cells_box.loc.y + top)),
        Size::from((GROUP_BAR_W, cells_box.size.h - top + extra.h)),
    )
}

/// Width of the bottom bar for the member cells' box: from the side bar to the cells' right
/// edge.
pub fn bottom_bar_w(cells_box: Rect, gap: f64) -> f64 {
    cells_box.size.w + gap
}

/// Bottom bar rect relative to the origin of a side bar of `height`.
fn bottom_bar_rel(height: f64, bottom_w: f64) -> Rect {
    Rectangle::new(
        Point::from((GROUP_BAR_W, height - GROUP_BOTTOM_H)),
        Size::from((bottom_w, GROUP_BOTTOM_H)),
    )
}

/// Group bounding box: the member cells plus the side bar on their left and the bottom bar
/// below them.
pub fn group_box(cells: &[Rect], gap: f64) -> Rect {
    let cells_box = union_all(cells);
    let extra = group_extra(gap);
    Rectangle::new(
        Point::from((cells_box.loc.x - extra.w, cells_box.loc.y)),
        cells_box.size + extra,
    )
}

/// Box available to the member cells inside a group box.
pub fn content_box(group_box: Rect, gap: f64) -> Rect {
    let extra = group_extra(gap);
    Rectangle::new(
        Point::from((group_box.loc.x + extra.w, group_box.loc.y)),
        Size::from((
            (group_box.size.w - extra.w).max(1.),
            (group_box.size.h - extra.h).max(1.),
        )),
    )
}

/// Ball locations relative to the bar origin for a bar of `height`: centered horizontally,
/// stacked and centered vertically.
pub fn ball_locs(height: f64) -> [Point<f64, Logical>; BALL_COUNT] {
    let total = BALL_COUNT as f64 * BALL + (BALL_COUNT as f64 - 1.) * GAP;
    let x = ((GROUP_BAR_W - BALL) / 2.).max(0.);
    let y0 = ((height - total) / 2.).max(0.);
    std::array::from_fn(|i| Point::from((x, y0 + i as f64 * (BALL + GAP))))
}

/// Ball index under `point` (relative to the bar origin).
pub fn hit_index_at(height: f64, point: Point<f64, Logical>) -> Option<usize> {
    ball_locs(height)
        .iter()
        .position(|loc| Rectangle::new(*loc, Size::from((BALL, BALL))).contains(point))
}

/// Hit on the L of a side bar of `height` and a bottom bar `bottom_w` wide at `point`
/// (relative to the side bar origin).
pub fn hit_at(height: f64, bottom_w: f64, point: Point<f64, Logical>) -> Option<GroupBarHit> {
    let r = GROUP_EDGE_REACH;
    let on_left = point.x >= -r && point.x < r && point.y >= 0. && point.y < height + r;
    let on_bottom = point.y >= height - r
        && point.y < height + r
        && point.x >= -r
        && point.x < GROUP_BAR_W + bottom_w;
    let mut edges = ResizeEdge::empty();
    if on_left {
        edges |= ResizeEdge::LEFT;
    }
    if on_bottom {
        edges |= ResizeEdge::BOTTOM;
    }
    if !edges.is_empty() {
        return Some(GroupBarHit::Edge(edges));
    }

    let bar = Rectangle::from_size(Size::from((GROUP_BAR_W, height)));
    if bar.contains(point) {
        return Some(match hit_index_at(height, point) {
            Some(i) => GroupBarHit::Ball(ball_action(i)),
            None => GroupBarHit::Body,
        });
    }
    bottom_bar_rel(height, bottom_w)
        .contains(point)
        .then_some(GroupBarHit::Body)
}

/// Content size for an automatic layout of members with tile `sizes`: width is the widest row
/// (side by side members of a row plus the gap), height fills `max.h`; clamped to `max`.
pub fn auto_content_size(
    sizes: &[Size<f64, Logical>],
    gap: f64,
    max: Size<f64, Logical>,
) -> Size<f64, Logical> {
    let row = |a: usize, b: usize| sizes[a].w + gap + sizes[b].w;
    let w = match sizes.len() {
        0 => 1.,
        1 => sizes[0].w,
        2 => row(0, 1),
        3 => row(0, 1).max(sizes[2].w),
        _ => row(0, 1).max(row(2, 3)),
    };
    Size::from((w.clamp(1., max.w.max(1.)), max.h.max(1.)))
}

/// Automatic cells inside `content`: 2 side by side, 3 = two on top and a full-width strip
/// below, 4 = 2x2 grid.
pub fn auto_cells(n: usize, content: Rect, gap: f64) -> Vec<Rect> {
    let Rectangle { loc, size } = content;
    let half_w = ((size.w - gap) / 2.).max(1.);
    let half_h = ((size.h - gap) / 2.).max(1.);
    let rect = |x: f64, y: f64, w: f64, h: f64| {
        Rectangle::new(Point::from((loc.x + x, loc.y + y)), Size::from((w, h)))
    };
    let right = size.w - half_w;
    let bottom = size.h - half_h;
    match n {
        0 => Vec::new(),
        1 => vec![content],
        2 => vec![
            rect(0., 0., half_w, size.h),
            rect(right, 0., half_w, size.h),
        ],
        3 => vec![
            rect(0., 0., half_w, half_h),
            rect(right, 0., half_w, half_h),
            rect(0., bottom, size.w, half_h),
        ],
        _ => vec![
            rect(0., 0., half_w, half_h),
            rect(right, 0., half_w, half_h),
            rect(0., bottom, half_w, half_h),
            rect(right, bottom, half_w, half_h),
        ],
    }
}

#[derive(Debug, Clone)]
struct Textures {
    scale: f64,
    balls: Vec<TextureBuffer<GlesTexture>>,
    icons: Vec<TextureBuffer<GlesTexture>>,
}

/// Render state of one group's L: the side bar with its balls and the bottom bar, both glass.
#[derive(Debug)]
pub struct GroupChrome {
    /// Side bar height.
    height: f64,
    /// Bottom bar width.
    bottom_w: f64,
    /// Outer corner radius of the L.
    radius: f32,
    scale: f64,
    hover_ball: Option<usize>,
    textures: RefCell<Option<Textures>>,
    side_glass: BackgroundEffect,
    bottom_glass: BackgroundEffect,
    glass_off: bool,
    glass_color: niri_config::Color,
    /// Bottom clip line for the next faded render (same space as the render location).
    bottom_clip: Cell<Option<f64>>,
    /// Top clip line for the next faded render.
    top_clip: Cell<Option<f64>>,
    fade_offscreen: RefCell<Option<OffscreenBuffer>>,
}

impl GroupChrome {
    pub fn new() -> Self {
        Self {
            height: 0.,
            bottom_w: 0.,
            radius: 0.,
            scale: 1.,
            hover_ball: None,
            textures: RefCell::new(None),
            side_glass: BackgroundEffect::new(),
            bottom_glass: BackgroundEffect::new(),
            glass_off: false,
            glass_color: niri_config::Color::from_rgba8_unpremul(0, 0, 0, 0),
            bottom_clip: Cell::new(None),
            top_clip: Cell::new(None),
            fade_offscreen: RefCell::new(None),
        }
    }

    /// Sets the clip lines used by the next [`Self::render_layers`] call.
    pub fn set_clips(&self, bottom: Option<f64>, top: Option<f64>) {
        self.bottom_clip.set(bottom);
        self.top_clip.set(top);
    }

    /// Lay out the L: a side bar of `height`, a bottom bar `bottom_w` wide, outer corners
    /// rounded by `radius`.
    pub fn update(&mut self, height: f64, bottom_w: f64, radius: f32, scale: f64) {
        self.height = height.max(0.);
        self.bottom_w = bottom_w.max(0.);
        self.radius = radius.max(0.);
        self.scale = scale;
    }

    /// Glass of the L from the group border config and the global blur config. Call after
    /// [`Self::update`].
    pub fn update_glass(&mut self, config: &niri_config::GroupBorder, blur: niri_config::Blur) {
        self.glass_off = config.off;
        self.glass_color = config.background_color;
        let effect = niri_config::BackgroundEffect {
            blur: Some(config.background_blur && !config.off),
            xray: Some(false),
            ..Default::default()
        };
        let (side, bottom) = self.radii();
        self.side_glass.update_config(blur);
        self.side_glass
            .update_render_elements(side, effect, false, 1.);
        self.bottom_glass.update_config(blur);
        self.bottom_glass
            .update_render_elements(bottom, effect, false, 1.);
    }

    /// Corner radii of the side and bottom bars: the corner where they join stays square.
    fn radii(&self) -> (CornerRadius, CornerRadius) {
        let r = self.radius;
        let side = CornerRadius {
            top_left: r,
            top_right: r,
            bottom_right: 0.,
            bottom_left: r,
        };
        let bottom = CornerRadius {
            top_left: 0.,
            top_right: r,
            bottom_right: r,
            bottom_left: 0.,
        };
        (
            side.fit_to(GROUP_BAR_W as f32, self.height as f32),
            bottom.fit_to(self.bottom_w as f32, GROUP_BOTTOM_H as f32),
        )
    }

    /// Side and bottom bar rects for a side bar origin at `location`.
    fn bar_rects(&self, location: Point<f64, Logical>) -> [Rect; 2] {
        let side = Rectangle::new(location, Size::from((GROUP_BAR_W, self.height)));
        let mut bottom = bottom_bar_rel(self.height, self.bottom_w);
        bottom.loc += location;
        [side, bottom]
    }

    /// Side and bottom bar rects for a side bar origin at `location`, skipping empty ones.
    pub fn rects(&self, location: Point<f64, Logical>) -> Vec<Rect> {
        self.bar_rects(location)
            .into_iter()
            .filter(|r| r.size.w >= 1. && r.size.h >= 1.)
            .collect()
    }

    /// Render the tint of the L glass; it goes above [`Self::render_glass_blur`].
    pub fn render_glass_tint(
        &self,
        renderer: &mut impl NiriRenderer,
        location: Point<f64, Logical>,
        push: &mut dyn FnMut(FocusRingRenderElement),
    ) {
        if self.glass_off || self.glass_color.a <= 0. || !BorderRenderElement::has_shader(renderer)
        {
            return;
        }
        let (side_r, bottom_r) = self.radii();
        for (rect, radius) in zip(self.bar_rects(location), [side_r, bottom_r]) {
            if rect.size.w < 1. || rect.size.h < 1. {
                continue;
            }
            let size = rect.size;
            let elem = BorderRenderElement::new(
                size,
                Rectangle::from_size(size),
                GradientInterpolation::default(),
                self.glass_color,
                self.glass_color,
                0.,
                Rectangle::from_size(size),
                0.,
                radius,
                self.scale as f32,
                1.,
            )
            .with_location(rect.loc);
            push(FocusRingRenderElement::Gradient(elem));
        }
    }

    /// Render the blur of the L glass for a side bar origin at `location`; `xray_pos` is for
    /// that origin. When `opaque_y` is set, only the vertical span that stays fully opaque under
    /// the edge fades is painted (blur cannot alpha-ramp).
    pub fn render_glass_blur(
        &self,
        mut ctx: RenderCtx<impl NiriRenderer>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        opaque_y: Option<(f64, f64)>,
        push: &mut dyn FnMut(BackgroundEffectElement),
    ) {
        if self.glass_off {
            return;
        }
        for (mut rect, glass) in zip(
            self.bar_rects(location),
            [&self.side_glass, &self.bottom_glass],
        ) {
            if rect.size.w < 1. || rect.size.h < 1. {
                continue;
            }
            if let Some((top, bottom)) = opaque_y {
                let y0 = rect.loc.y.max(top);
                let y1 = (rect.loc.y + rect.size.h).min(bottom);
                if y1 - y0 < 1. {
                    continue;
                }
                rect = Rectangle::new(
                    Point::from((rect.loc.x, y0)),
                    Size::from((rect.size.w, y1 - y0)),
                );
            }
            let params = RenderParams {
                geometry: rect,
                subregion: None,
                clip: Some((rect, CornerRadius::default())),
                scale: self.scale,
            };
            let xray_pos = xray_pos.offset(rect.loc - location);
            glass.render(ctx.as_gles(), None, params, xray_pos, push);
        }
    }

    pub fn update_shaders(&mut self) {
        *self.textures.borrow_mut() = None;
    }

    pub fn height(&self) -> f64 {
        self.height
    }

    /// Returns true if the hovered ball changed.
    pub fn set_hover(&mut self, ball: Option<usize>) -> bool {
        let changed = self.hover_ball != ball;
        self.hover_ball = ball;
        changed
    }

    pub fn hit(&self, point: Point<f64, Logical>) -> Option<GroupBarHit> {
        hit_at(self.height, self.bottom_w, point)
    }

    pub fn hit_index(&self, point: Point<f64, Logical>) -> Option<usize> {
        hit_index_at(self.height, point)
    }

    fn textures<R: NiriRenderer>(&self, renderer: &mut R) -> Option<Textures> {
        let mut cache = self.textures.borrow_mut();
        if let Some(t) = &*cache {
            if t.scale == self.scale {
                return Some(t.clone());
            }
        }

        let scale = self.scale;
        let gles = renderer.as_gles_renderer();
        let render = |gles: &mut GlesRenderer| -> anyhow::Result<Textures> {
            let mut balls = Vec::with_capacity(BALL_COUNT);
            let mut icons = Vec::with_capacity(BALL_COUNT);
            for i in 0..BALL_COUNT {
                balls.push(render_ball(gles, scale, i)?);
                icons.push(render_icon(gles, scale, i)?);
            }
            Ok(Textures {
                scale,
                balls,
                icons,
            })
        };
        let textures = render(gles)
            .map_err(|err| warn!("error rendering group chrome textures: {err:?}"))
            .ok()?;
        *cache = Some(textures.clone());
        Some(textures)
    }

    /// Render the side bar balls with the bar origin at `location`. First pushed is drawn on
    /// top.
    pub fn render(
        &self,
        renderer: &mut impl NiriRenderer,
        location: Point<f64, Logical>,
        push: &mut dyn FnMut(ChromeRenderElement),
    ) {
        if self.height < BAR_H {
            return;
        }
        let Some(textures) = self.textures(renderer) else {
            return;
        };
        let elem = |buffer: TextureBuffer<GlesTexture>, loc, size| {
            let elem = TextureRenderElement::from_texture_buffer(
                buffer,
                loc,
                1.,
                None,
                Some(size),
                Kind::Unspecified,
            );
            ChromeRenderElement::from(PrimaryGpuTextureRenderElement(elem))
        };

        let locs = ball_locs(self.height);
        if let Some(i) = self.hover_ball.filter(|i| *i < BALL_COUNT) {
            push(elem(
                textures.icons[i].clone(),
                location + locs[i],
                Size::from((BALL, BALL)),
            ));
        }
        let m = Point::from((SHADOW_M, SHADOW_M));
        let size = BALL + SHADOW_M * 2.;
        for (i, loc) in locs.iter().enumerate() {
            push(elem(
                textures.balls[i].clone(),
                location + *loc - m,
                Size::from((size, size)),
            ));
        }
    }

    /// Renders balls, tint and blur. Applies the same progressive edge fade as tiles when the L
    /// crosses [`Self::set_clips`] lines (tint and balls via `BottomFade`; blur stays in the
    /// opaque span only).
    pub fn render_layers(
        &self,
        mut ctx: RenderCtx<impl NiriRenderer>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        push: &mut dyn FnMut(GroupChromeLayer),
    ) {
        let clip_bottom = self.bottom_clip.get();
        let clip_top = self.top_clip.get();
        let chrome_bottom = location.y + self.height;
        let chrome_top = location.y;

        if clip_bottom.is_some_and(|c| chrome_top >= c)
            || clip_top.is_some_and(|c| chrome_bottom <= c)
        {
            self.fade_offscreen.replace(None);
            return;
        }

        let bottom_band = clip_bottom.map_or(0., |c| bottom_fade_band(chrome_bottom, c));
        let top_band = clip_top.map_or(0., |c| top_fade_band(chrome_top, c));
        let crosses_bottom =
            clip_bottom.is_some_and(|c| crosses_bottom_clip(chrome_top, chrome_bottom, c));
        let crosses_top = clip_top.is_some_and(|c| crosses_top_clip(chrome_top, chrome_bottom, c));
        let need_fade = crosses_bottom || crosses_top;

        let opaque_y = if need_fade {
            let mut top = chrome_top;
            let mut bottom = chrome_bottom;
            if let Some(c) = clip_top {
                top = top.max(c + top_band);
            }
            if let Some(c) = clip_bottom {
                bottom = bottom.min(c - bottom_band);
            }
            (bottom > top + 0.5).then_some((top, bottom))
        } else {
            None
        };

        // Blur cannot alpha-ramp; keep it in the fully opaque span when fading.
        self.render_glass_blur(ctx.r(), location, xray_pos, opaque_y, &mut |elem| {
            push(elem.into())
        });

        if need_fade {
            if self.render_paint_faded(
                ctx.r(),
                location,
                clip_bottom
                    .filter(|_| crosses_bottom)
                    .map(|c| (c, bottom_band)),
                clip_top.filter(|_| crosses_top).map(|c| (c, top_band)),
                &mut |elem| push(elem.into()),
            ) {
                return;
            }
        } else {
            self.fade_offscreen.replace(None);
        }

        self.render(ctx.renderer, location, &mut |elem| push(elem.into()));
        self.render_glass_tint(ctx.renderer, location, &mut |elem| push(elem.into()));
    }

    fn render_paint_faded(
        &self,
        mut ctx: RenderCtx<impl NiriRenderer>,
        location: Point<f64, Logical>,
        bottom: Option<(f64, f64)>,
        top: Option<(f64, f64)>,
        push: &mut dyn FnMut(BottomFadeRenderElement),
    ) -> bool {
        let ctx = ctx.as_gles();
        let Some(program) = BottomFadeRenderElement::shader(ctx.renderer) else {
            return false;
        };

        let scale = Scale::from(self.scale);
        let origin = Point::new(0., 0.);
        let mut elements: Vec<GroupChromePaint> = Vec::new();
        self.render(ctx.renderer, origin, &mut |elem| elements.push(elem.into()));
        self.render_glass_tint(ctx.renderer, origin, &mut |elem| elements.push(elem.into()));
        if elements.is_empty() {
            return true;
        }

        let mut buffer = self.fade_offscreen.borrow_mut();
        let buffer = buffer.get_or_insert_with(OffscreenBuffer::default);
        match buffer.render(ctx.renderer, scale, &elements) {
            Ok((elem, _sync, _data)) => {
                let offset = elem.offset();
                let elem = elem.with_offset(location + offset);
                push(BottomFadeRenderElement::new_edges(
                    elem, program, bottom, top, self.scale,
                ));
                true
            }
            Err(err) => {
                warn!("error rendering group chrome to offscreen for edge fade: {err:?}");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn l_bars_frame_the_cells_left_and_below() {
        let gap = 12.;
        let cells = [rect(138., 50., 200., 300.), rect(350., 50., 200., 300.)];
        let cells_box = union_all(&cells);
        let gb = group_box(&cells, gap);
        assert_eq!(gb, rect(100., 50., 450., 300. + gap + GROUP_BOTTOM_H));
        assert_eq!(content_box(gb, gap), cells_box);

        let title = WindowChrome::extra_height();
        let bar = side_bar_rect(cells_box, gap);
        assert_eq!(
            bar,
            rect(
                100.,
                50. + title,
                GROUP_BAR_W,
                300. - title + gap + GROUP_BOTTOM_H
            )
        );
        assert_eq!(bottom_bar_w(cells_box, gap), 424.);
        let bottom = bottom_bar_rel(bar.size.h, 424.);
        assert_eq!(bottom.loc.x + bar.loc.x, 126.);
        assert_eq!(bottom.loc.y + bar.loc.y, 50. + 300. + gap);
        assert_eq!(bottom.loc.x + bar.loc.x + bottom.size.w, 550.);
    }

    #[test]
    fn balls_are_vertical_and_centered() {
        let h = 300.;
        let locs = ball_locs(h);
        let total = 5. * BALL + 4. * GAP;
        assert!((locs[0].y - (h - total) / 2.).abs() < 1e-9);
        for (i, loc) in locs.iter().enumerate() {
            assert!((loc.x - (GROUP_BAR_W - BALL) / 2.).abs() < 1e-9);
            if i > 0 {
                assert!((loc.y - locs[i - 1].y - (BALL + GAP)).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn ball_hit_maps_to_title_bar_order() {
        let h = 300.;
        let locs = ball_locs(h);
        let expected = [
            ChromeAction::Minimize,
            ChromeAction::Half,
            ChromeAction::FullViewport,
            ChromeAction::Fullscreen,
            ChromeAction::Close,
        ];
        for (i, loc) in locs.iter().enumerate() {
            let p = *loc + Point::from((BALL / 2., BALL / 2.));
            assert_eq!(hit_index_at(h, p), Some(i));
            assert_eq!(hit_at(h, 400., p), Some(GroupBarHit::Ball(expected[i])));
        }
        assert_eq!(
            hit_at(h, 400., Point::from((5., 5.))),
            Some(GroupBarHit::Body)
        );
        assert_eq!(hit_at(h, 400., Point::from((GROUP_BAR_W + 1., 150.))), None);
        let on_bottom = Point::from((GROUP_BAR_W + 200., h - GROUP_BOTTOM_H / 2.));
        assert_eq!(hit_at(h, 400., on_bottom), Some(GroupBarHit::Body));
        let past_bottom = Point::from((GROUP_BAR_W + 401., h - GROUP_BOTTOM_H / 2.));
        assert_eq!(hit_at(h, 400., past_bottom), None);

        let edge = |x, y| hit_at(h, 400., Point::from((x, y)));
        assert_eq!(edge(-3., 150.), Some(GroupBarHit::Edge(ResizeEdge::LEFT)));
        assert_eq!(edge(2., 150.), Some(GroupBarHit::Edge(ResizeEdge::LEFT)));
        assert_eq!(
            edge(GROUP_BAR_W + 200., h + 3.),
            Some(GroupBarHit::Edge(ResizeEdge::BOTTOM))
        );
        assert_eq!(
            edge(GROUP_BAR_W + 200., h - 2.),
            Some(GroupBarHit::Edge(ResizeEdge::BOTTOM))
        );
        assert_eq!(
            edge(-2., h + 2.),
            Some(GroupBarHit::Edge(ResizeEdge::LEFT | ResizeEdge::BOTTOM))
        );
        assert_eq!(edge(-3., -3.), None);
    }

    #[test]
    fn auto_layouts() {
        let content = rect(0., 0., 416., 316.);
        let two = auto_cells(2, content, 16.);
        assert_eq!(
            two,
            vec![rect(0., 0., 200., 316.), rect(216., 0., 200., 316.)]
        );
        let three = auto_cells(3, content, 16.);
        assert_eq!(three[2], rect(0., 166., 416., 150.));
        assert_eq!(three[0], rect(0., 0., 200., 150.));
        let four = auto_cells(4, content, 16.);
        assert_eq!(four[3], rect(216., 166., 200., 150.));

        let sizes = [Size::from((300., 200.)), Size::from((400., 200.))];
        let s = auto_content_size(&sizes, 16., Size::from((600., 688.)));
        assert_eq!(s, Size::from((600., 688.)));
        let s = auto_content_size(&sizes, 16., Size::from((2000., 688.)));
        assert_eq!(s, Size::from((716., 688.)));
    }
}
