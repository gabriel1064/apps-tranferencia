//! Backdrop layer in a window's own background color, drawn between its blur and its buffer.

use std::{
    cell::{Cell, RefCell},
    time::Duration,
};

use niri_config::{BackgroundEffect, Color, CornerRadius, GradientInterpolation};
use smithay::{
    backend::{allocator::Fourcc, renderer::gles::GlesRenderer},
    utils::{Logical, Physical, Point, Rectangle, Scale, Size, Transform},
};

use super::LayoutElement;
use crate::{
    animation::{Animation, Clock, Curve},
    render_helpers::{border::BorderRenderElement, render_to_vec, RenderCtx},
};

/// Longest side of the downscaled window render used to find its background color.
const SAMPLE_PX: f64 = 96.;
/// Re-sample this often while the tint stays on, so loading or changing content catches up.
const RESAMPLE_EVERY: Duration = Duration::from_secs(5);
/// Share of each side treated as the window's frame, which counts double in the vote.
const EDGE_FRAC: f64 = 0.15;
/// Cross-fade between the plain and the glass look when a tint rule starts or stops matching.
const FADE_MS: u64 = 320;

/// Animated switch between a window's plain look and its tinted glass look.
///
/// Remembers the glass values while they don't match so fading out can still show them.
#[derive(Debug)]
pub struct GlassFade {
    on: bool,
    anim: Option<Animation>,
    plain_alpha: f32,
    glass_alpha: f32,
    tint: f64,
    effect: BackgroundEffect,
}

impl GlassFade {
    pub fn new(opacity: Option<f32>, effect: BackgroundEffect) -> Self {
        let mut rv = Self {
            on: false,
            anim: None,
            plain_alpha: 1.,
            glass_alpha: 1.,
            tint: 0.,
            effect: BackgroundEffect::default(),
        };
        rv.store(opacity, effect);
        rv.on = is_glass(&effect);
        rv
    }

    fn store(&mut self, opacity: Option<f32>, effect: BackgroundEffect) {
        let alpha = opacity.unwrap_or(1.).clamp(0., 1.);
        if is_glass(&effect) {
            self.glass_alpha = alpha;
            self.tint = effect.tint.unwrap_or(0.);
            self.effect = effect;
        } else {
            self.plain_alpha = alpha;
        }
    }

    pub fn update(&mut self, clock: &Clock, opacity: Option<f32>, effect: BackgroundEffect) {
        self.store(opacity, effect);

        let on = is_glass(&effect);
        if on == self.on {
            return;
        }

        let from = self.progress();
        self.on = on;
        let to = if on { 1. } else { 0. };
        self.anim = Some(Animation::ease(
            clock.clone(),
            from,
            to,
            0.,
            FADE_MS,
            Curve::EaseOutCubic,
        ));
    }

    pub fn advance(&mut self) {
        if self.anim.as_ref().is_some_and(|anim| anim.is_done()) {
            self.anim = None;
        }
    }

    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
    }

    fn progress(&self) -> f64 {
        match &self.anim {
            Some(anim) => anim.clamped_value().clamp(0., 1.),
            None if self.on => 1.,
            None => 0.,
        }
    }

    /// Window opacity, between the plain and the glass rule values.
    pub fn alpha(&self) -> f32 {
        let p = self.progress() as f32;
        self.plain_alpha + (self.glass_alpha - self.plain_alpha) * p
    }

    /// Tint opacity, or `None` while fully plain.
    pub fn tint(&self) -> Option<f64> {
        let p = self.progress();
        (p > 0.).then_some(self.tint * p)
    }

    /// Background effect to render and its opacity over the sharp background.
    ///
    /// The glass blur fades with the switch instead of popping, which windows with their own
    /// translucent background would show.
    pub fn effect(&self, current: BackgroundEffect) -> (BackgroundEffect, f32) {
        let p = self.progress();
        match (self.on, p > 0.) {
            (true, _) => (current, p as f32),
            (false, true) => (self.effect, p as f32),
            (false, false) => (current, 1.),
        }
    }
}

fn is_glass(effect: &BackgroundEffect) -> bool {
    effect.tint.is_some_and(|t| t > 0.)
}

#[derive(Debug, Default)]
pub struct WindowTint {
    color: Cell<Option<Color>>,
    sampled_at: Cell<Option<Duration>>,
    elem: RefCell<BorderRenderElement>,
}

impl WindowTint {
    /// Forget the sampled color so the next time the tint turns on it samples fresh.
    pub fn reset(&self) {
        self.color.set(None);
        self.sampled_at.set(None);
    }

    fn needs_sample(&self, now: Duration) -> bool {
        match self.sampled_at.get() {
            None => true,
            Some(at) => now.saturating_sub(at) >= RESAMPLE_EVERY,
        }
    }

    /// Returns the tint element for a window at `area`, sampling its color when due.
    #[allow(clippy::too_many_arguments)]
    pub fn render<W: LayoutElement>(
        &self,
        mut ctx: RenderCtx<GlesRenderer>,
        window: &W,
        alpha: f64,
        area: Rectangle<f64, Logical>,
        window_size: Size<f64, Logical>,
        radius: CornerRadius,
        scale: f64,
        now: Duration,
    ) -> Option<BorderRenderElement> {
        if self.needs_sample(now) {
            self.sampled_at.set(Some(now));
            if let Some(color) = sample_background(ctx.r(), window, window_size) {
                self.color.set(Some(color));
            }
        }

        let color = self.color.get()?;
        let size = area.size;
        let geo = Rectangle::from_size(size);
        let radius = radius.fit_to(size.w as f32, size.h as f32);

        let mut elem = self.elem.borrow_mut();
        elem.update(
            size,
            geo,
            GradientInterpolation::default(),
            color,
            color,
            0.,
            geo,
            0.,
            radius,
            scale as f32,
            alpha.clamp(0., 1.) as f32,
        );
        Some(elem.clone().with_location(area.loc))
    }
}

/// Renders the window small and returns its most common color, favoring its outer frame.
fn sample_background<W: LayoutElement>(
    mut ctx: RenderCtx<GlesRenderer>,
    window: &W,
    size: Size<f64, Logical>,
) -> Option<Color> {
    let longest = size.w.max(size.h);
    if longest < 1. {
        return None;
    }

    let scale = Scale::from((SAMPLE_PX / longest).min(1.));
    let px: Size<i32, Physical> = size.to_physical_precise_round(scale);
    let (w, h) = (px.w.max(1), px.h.max(1));

    let mut elements = Vec::new();
    window.render_normal(ctx.r(), Point::from((0., 0.)), scale, 1., &mut |elem| {
        elements.push(elem)
    });
    if elements.is_empty() {
        return None;
    }

    let bytes = match render_to_vec(
        ctx.renderer,
        Size::from((w, h)),
        scale,
        Transform::Normal,
        Fourcc::Abgr8888,
        elements.into_iter().rev(),
    ) {
        Ok(bytes) => bytes,
        Err(err) => {
            warn!("error sampling window background color: {err:?}");
            return None;
        }
    };

    dominant_color(&bytes, w as usize, h as usize)
}

/// Most frequent color (4 bits per channel buckets) of premultiplied RGBA pixels.
fn dominant_color(bytes: &[u8], w: usize, h: usize) -> Option<Color> {
    if bytes.len() < w * h * 4 {
        return None;
    }

    let edge_x = ((w as f64 * EDGE_FRAC).ceil() as usize).max(1);
    let edge_y = ((h as f64 * EDGE_FRAC).ceil() as usize).max(1);

    let mut votes = vec![0u32; 4096];
    let mut sums = vec![[0f64; 3]; 4096];

    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 4;
            let a = bytes[i + 3] as f64 / 255.;
            if a < 0.1 {
                continue;
            }
            let rgb = [0, 1, 2].map(|c| (bytes[i + c] as f64 / 255. / a).min(1.));
            let bucket = rgb
                .iter()
                .fold(0usize, |acc, c| (acc << 4) | ((c * 15.).round() as usize));

            let on_edge = x < edge_x || y < edge_y || x >= w - edge_x || y >= h - edge_y;
            let weight = if on_edge { 2 } else { 1 };

            votes[bucket] += weight;
            for c in 0..3 {
                sums[bucket][c] += rgb[c] * weight as f64;
            }
        }
    }

    let (bucket, &count) = votes.iter().enumerate().max_by_key(|(_, n)| **n)?;
    if count == 0 {
        return None;
    }

    let [r, g, b] = sums[bucket].map(|s| (s / count as f64) as f32);
    Some(Color::new_unpremul(r, g, b, 1.))
}
