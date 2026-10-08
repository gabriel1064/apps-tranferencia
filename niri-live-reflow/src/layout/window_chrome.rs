//! SSD title bar as the top band of the window frame: app name + window title
//! and Mac-style control balls. Tiled windows get a frosted-glass title fill;
//! floating windows get an opaque solid fill. Both sit inside the outer border
//! (rounded only on the top corners) with a straight hairline divider above the
//! client — no rounded seam in the middle.
//! Click-drag on the strip moves the window (same as Super+click).

use std::{cell::RefCell, collections::HashMap, env, fs, path::PathBuf};

use niri_config::{Color, CornerRadius};
use pangocairo::{
    cairo::{self, ImageSurface},
    pango::{EllipsizeMode, FontDescription},
};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::Kind,
            gles::{GlesRenderer, GlesTexture},
        },
    },
    utils::{Logical, Point, Rectangle, Size, Transform},
};

use crate::{
    niri_render_elements,
    render_helpers::{
        background_effect::{BackgroundEffect, BackgroundEffectElement, RenderParams},
        border::BorderRenderElement,
        primary_gpu_texture::PrimaryGpuTextureRenderElement,
        renderer::NiriRenderer,
        solid_color::{SolidColorBuffer, SolidColorRenderElement},
        texture::{TextureBuffer, TextureRenderElement},
        xray::XrayPos,
        RenderCtx,
    },
};

pub const BALL_COUNT: usize = 5;
pub(super) const BALL: f64 = 10.;
pub(super) const GAP: f64 = 5.;
const LEFT_PAD: f64 = 6.;
const RIGHT_PAD: f64 = 6.;
const TEXT_PAD: f64 = 14.;
/// Window title is cut to this many characters (plus "…").
const TITLE_MAX_CHARS: usize = 24;
/// Height of the title-bar band inside the window frame (above the client).
pub const BAR_H: f64 = 26.;
/// Empty space between the title row and the window's top edge.
const TITLE_GAP: f64 = 2.;
/// Straight hairline between title fill and client (no rounded corners).
const DIVIDER_H: f64 = 1.;
pub(super) const SHADOW_M: f64 = 6.;
const SHADOW_SPREAD: f64 = 5.;
const GLASS_COLOR_ALPHA: f64 = 0.88;
const GLASS_SHEEN: f64 = 0.38;
/// Floating title-bar fill: opaque charcoal (#131313).
const TITLE_SOLID: [f32; 4] = [19. / 255., 19. / 255., 19. / 255., 1.];
/// Tiled title-bar glass tint: same charcoal, a bit more solid than the old floating glass.
const TITLE_GLASS: [f32; 4] = [19. / 255., 19. / 255., 19. / 255., 0.92];
/// Subtle straight divider on top of the client edge.
const DIVIDER_COLOR: [f32; 4] = [1., 1., 1., 0.14];
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromeAction {
    Close,
    Minimize,
    Half,
    FullViewport,
    Fullscreen,
}

/// Left → right, 30° hue steps from green to red.
const BALLS: [(ChromeAction, [f32; 4]); BALL_COUNT] = [
    (ChromeAction::Minimize, [0.16, 0.78, 0.25, 1.0]),
    (ChromeAction::Half, [0.64, 0.82, 0.16, 1.0]),
    (ChromeAction::FullViewport, [0.97, 0.78, 0.00, 1.0]),
    (ChromeAction::Fullscreen, [1.00, 0.50, 0.10, 1.0]),
    (ChromeAction::Close, [1.00, 0.31, 0.29, 1.0]),
];

pub(super) fn ball_action(index: usize) -> ChromeAction {
    BALLS[index].0
}

fn ball_color(index: usize) -> [f32; 4] {
    BALLS[index].1
}

niri_render_elements! {
    ChromeRenderElement => {
        SolidColor = SolidColorRenderElement,
        Border = BorderRenderElement,
        Icon = PrimaryGpuTextureRenderElement,
        Blur = BackgroundEffectElement,
    }
}

#[derive(Debug, Clone)]
struct ChromeTextures {
    scale: f32,
    bar_w: i32,
    bar_h: i32,
    radius: f32,
    title: String,
    app_id: String,
    active: bool,
    bar: Option<TextureBuffer<GlesTexture>>,
    balls: Vec<TextureBuffer<GlesTexture>>,
    icons: Vec<TextureBuffer<GlesTexture>>,
}

#[derive(Debug)]
pub struct WindowChrome {
    balls: [SolidColorBuffer; BALL_COUNT],
    ball_borders: [BorderRenderElement; BALL_COUNT],
    /// Fallback sharp fill when the border shader is unavailable.
    title_bg: SolidColorBuffer,
    /// Rounded title-bar tint (top corners only).
    title_fill: BorderRenderElement,
    /// Frosted blur under the title-bar tint (tiled / glass mode only).
    title_glass: BackgroundEffect,
    /// Straight 1px divider between title bar and client.
    divider: SolidColorBuffer,
    textures: RefCell<Option<ChromeTextures>>,
    island_loc: Point<f64, Logical>,
    island_size: Size<f64, Logical>,
    ball_locs: [Point<f64, Logical>; BALL_COUNT],
    ball_visible: [bool; BALL_COUNT],
    ball_alpha: f32,
    alpha: f32,
    /// 0 = tiled (frosted glass), 1 = floating (opaque solid).
    bg_alpha: f32,
    scale: f32,
    /// Inner top corner radius for the title fill (matches geometry radius).
    fill_radius: f32,
    title: String,
    app_id: String,
    is_active: bool,
    /// Window is a group member: FullViewport has no meaning there.
    in_group: bool,
}

/// Opacity factor of a ball whose action does nothing in the current context.
const INERT_BALL_ALPHA: f32 = 0.3;

impl WindowChrome {
    pub fn new() -> Self {
        Self {
            balls: std::array::from_fn(|_| {
                SolidColorBuffer::new(Size::from((BALL, BALL)), [1., 0., 0., 1.])
            }),
            ball_borders: std::array::from_fn(|_| BorderRenderElement::empty()),
            title_bg: SolidColorBuffer::new(Size::from((1., 1.)), TITLE_SOLID),
            title_fill: BorderRenderElement::empty(),
            title_glass: BackgroundEffect::new(),
            divider: SolidColorBuffer::new(Size::from((1., DIVIDER_H)), DIVIDER_COLOR),
            textures: RefCell::new(None),
            island_loc: Default::default(),
            island_size: Default::default(),
            ball_locs: Default::default(),
            ball_visible: [false; BALL_COUNT],
            ball_alpha: 0.,
            alpha: 1.,
            bg_alpha: 0.,
            scale: 1.,
            fill_radius: 0.,
            title: String::new(),
            app_id: String::new(),
            is_active: false,
            in_group: false,
        }
    }

    pub fn extra_height() -> f64 {
        BAR_H
    }

    pub fn set_in_group(&mut self, in_group: bool) {
        self.in_group = in_group;
    }

    /// Whether ball `i` does nothing (drawn dimmed, no hover icon).
    fn ball_inert(&self, i: usize) -> bool {
        self.in_group && ball_action(i) == ChromeAction::FullViewport
    }

    fn ball_alpha_of(&self, i: usize) -> f32 {
        if self.ball_inert(i) {
            self.ball_alpha * INERT_BALL_ALPHA
        } else {
            self.ball_alpha
        }
    }

    fn balls_width() -> f64 {
        BALL_COUNT as f64 * BALL + (BALL_COUNT as f64 - 1.) * GAP
    }

    pub fn is_visible(&self) -> bool {
        self.alpha > 0.01 && self.island_size.w >= 1. && self.island_size.h >= 1.
    }

    pub fn update_shaders(&mut self) {
        for b in &mut self.ball_borders {
            b.damage_all();
        }
        self.title_fill.damage_all();
        self.title_glass.damage();
        *self.textures.borrow_mut() = None;
    }

    /// Wire frosted blur from the global blur config. Call when options change.
    pub fn update_glass(&mut self, blur: niri_config::Blur) {
        self.title_glass.update_config(blur);
        let glass = (1. - self.bg_alpha).clamp(0., 1.) * self.alpha;
        let effect = niri_config::BackgroundEffect {
            blur: Some(glass > 0.001),
            xray: Some(false),
            ..Default::default()
        };
        let radius = CornerRadius {
            top_left: self.fill_radius,
            top_right: self.fill_radius,
            bottom_right: 0.,
            bottom_left: 0.,
        }
        .fit_to(self.island_size.w as f32, self.island_size.h as f32);
        self.title_glass
            .update_render_elements(radius, effect, false, glass);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        tile_size: Size<f64, Logical>,
        border_width: f64,
        alpha: f32,
        scale: f64,
        title: String,
        app_id: String,
        is_active: bool,
        bg_alpha: f32,
        // Inner geometry corner radius (title fill uses top corners only).
        corner_radius: f32,
    ) {
        self.alpha = alpha;
        self.bg_alpha = bg_alpha.clamp(0., 1.);
        self.scale = scale as f32;
        self.title = title;
        self.app_id = app_id;
        self.is_active = is_active;

        if alpha < 0.01 || tile_size.w < 8. || tile_size.h < BAR_H {
            self.island_size = Size::from((0., 0.));
            self.ball_alpha = 0.;
            self.ball_visible = [false; BALL_COUNT];
            return;
        }

        // Title band sits inside the outer border (extension of the window frame).
        let bw = border_width.max(0.);
        let bar_w = (tile_size.w - bw * 2.).max(1.);
        self.island_size = Size::from((bar_w, BAR_H));
        self.island_loc = Point::from((bw, bw));
        self.fill_radius = corner_radius.max(0.);
        self.title_bg.resize(self.island_size);
        self.divider.resize(Size::from((bar_w, DIVIDER_H)));

        // bg_alpha 0 = tiled glass, 1 = floating solid.
        let solid = self.bg_alpha;
        let tint = [
            TITLE_GLASS[0] + (TITLE_SOLID[0] - TITLE_GLASS[0]) * solid,
            TITLE_GLASS[1] + (TITLE_SOLID[1] - TITLE_GLASS[1]) * solid,
            TITLE_GLASS[2] + (TITLE_SOLID[2] - TITLE_GLASS[2]) * solid,
            (TITLE_GLASS[3] + (TITLE_SOLID[3] - TITLE_GLASS[3]) * solid) * self.alpha,
        ];
        let fill_color = Color::from_array_unpremul(tint);
        let fill_geo = Rectangle::from_size(self.island_size);
        let fill_radius = CornerRadius {
            top_left: self.fill_radius,
            top_right: self.fill_radius,
            bottom_right: 0.,
            bottom_left: 0.,
        }
        .fit_to(self.island_size.w as f32, self.island_size.h as f32);
        self.title_fill.update(
            self.island_size,
            fill_geo,
            niri_config::GradientInterpolation::default(),
            fill_color,
            fill_color,
            0.,
            fill_geo,
            0.,
            fill_radius,
            self.scale,
            1.,
        );
        let glass = (1. - solid).clamp(0., 1.) * self.alpha;
        let glass_effect = niri_config::BackgroundEffect {
            blur: Some(glass > 0.001),
            xray: Some(false),
            ..Default::default()
        };
        self.title_glass
            .update_render_elements(fill_radius, glass_effect, false, glass);
        self.title_bg.set_color([
            tint[0] * tint[3],
            tint[1] * tint[3],
            tint[2] * tint[3],
            tint[3],
        ]);

        self.ball_alpha = alpha;
        let y = self.island_loc.y + ((BAR_H - TITLE_GAP - BALL) * 0.5).max(0.);
        let mut x = (self.island_loc.x + bar_w - RIGHT_PAD - Self::balls_width())
            .max(self.island_loc.x + LEFT_PAD);
        for i in 0..BALL_COUNT {
            self.ball_visible[i] = true;
            self.ball_locs[i] = Point::from((x, y));

            let mut bc = Color::from_array_unpremul(ball_color(i));
            bc.a *= self.ball_alpha_of(i);
            self.balls[i].resize(Size::from((BALL, BALL)));
            self.balls[i].set_color(bc.to_array_premul());

            let ball_size = Size::from((BALL, BALL));
            let ball_geo = Rectangle::from_size(ball_size);
            let ball_radius = CornerRadius::from((BALL as f32) / 2.);
            self.ball_borders[i].update(
                ball_size,
                ball_geo,
                niri_config::GradientInterpolation::default(),
                bc,
                bc,
                0.,
                ball_geo,
                0.,
                ball_radius,
                self.scale,
                1.,
            );

            x += BALL + GAP;
        }
    }

    pub fn hit_index(&self, point: Point<f64, Logical>) -> Option<usize> {
        if !self.is_visible() {
            return None;
        }
        for (i, loc) in self.ball_locs.iter().enumerate() {
            if !self.ball_visible[i] {
                continue;
            }
            let r = Rectangle::new(*loc, Size::from((BALL, BALL)));
            if r.contains(point) {
                return Some(i);
            }
        }
        None
    }

    pub fn hit(&self, point: Point<f64, Logical>) -> Option<ChromeAction> {
        self.hit_index(point).map(ball_action)
    }

    pub fn consumes_click(&self, point: Point<f64, Logical>) -> bool {
        self.is_visible() && self.island_rect().contains(point)
    }

    fn island_rect(&self) -> Rectangle<f64, Logical> {
        Rectangle::new(self.island_loc, self.island_size)
    }

    fn textures<R: NiriRenderer>(&self, renderer: &mut R) -> Option<ChromeTextures> {
        let mut cache = self.textures.borrow_mut();
        let log_w = self.island_size.w + SHADOW_M * 2.;
        let log_h = self.island_size.h + SHADOW_M * 2.;
        let px_w = (log_w * f64::from(self.scale)).round().max(1.) as i32;
        let px_h = (log_h * f64::from(self.scale)).round().max(1.) as i32;

        if let Some(textures) = &*cache {
            if textures.scale == self.scale
                && textures.bar_w == px_w
                && textures.bar_h == px_h
                && textures.radius == self.fill_radius
                && textures.title == self.title
                && textures.app_id == self.app_id
                && textures.active == self.is_active
            {
                return Some(textures.clone());
            }
        }

        let scale = f64::from(self.scale);
        let gles = renderer.as_gles_renderer();
        let reuse_balls = cache
            .as_ref()
            .is_some_and(|t| t.scale == self.scale && t.balls.len() == BALL_COUNT);

        let mut render = || -> anyhow::Result<ChromeTextures> {
            let (balls, icons) = if reuse_balls {
                let t = cache.as_ref().unwrap();
                (t.balls.clone(), t.icons.clone())
            } else {
                let mut balls = Vec::with_capacity(BALL_COUNT);
                let mut icons = Vec::with_capacity(BALL_COUNT);
                for i in 0..BALL_COUNT {
                    balls.push(render_ball(gles, scale, i)?);
                    icons.push(render_icon(gles, scale, i)?);
                }
                (balls, icons)
            };

            // Island is already inside the border; no extra side inset for text.
            let bar = render_bar(
                gles,
                scale,
                self.island_size.w,
                self.island_size.h,
                0.,
                &self.app_id,
                &self.title,
                self.is_active,
            )
            .ok();

            Ok(ChromeTextures {
                scale: self.scale,
                bar_w: px_w,
                bar_h: px_h,
                radius: self.fill_radius,
                title: self.title.clone(),
                app_id: self.app_id.clone(),
                active: self.is_active,
                bar,
                balls,
                icons,
            })
        };
        let textures = render()
            .map_err(|err| warn!("error rendering chrome textures: {err:?}"))
            .ok()?;
        *cache = Some(textures.clone());
        Some(textures)
    }

    pub fn render(
        &self,
        mut ctx: RenderCtx<impl NiriRenderer>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        hovered_ball: Option<usize>,
        push: &mut dyn FnMut(ChromeRenderElement),
    ) {
        if !self.is_visible() {
            return;
        }

        let textures = self.textures(ctx.renderer);
        let texture_elem = |buffer: TextureBuffer<GlesTexture>, loc, size, alpha| {
            let elem = TextureRenderElement::from_texture_buffer(
                buffer,
                loc,
                alpha,
                None,
                Some(size),
                Kind::Unspecified,
            );
            ChromeRenderElement::from(PrimaryGpuTextureRenderElement(elem))
        };

        // Icons first: first pushed is drawn on top.
        if let (Some(i), Some(textures)) = (hovered_ball, &textures) {
            if i < BALL_COUNT && self.ball_visible[i] && !self.ball_inert(i) {
                push(texture_elem(
                    textures.icons[i].clone(),
                    location + self.ball_locs[i],
                    Size::from((BALL, BALL)),
                    self.ball_alpha,
                ));
            }
        }

        let has_shader = BorderRenderElement::has_shader(ctx.renderer);

        for i in 0..BALL_COUNT {
            if !self.ball_visible[i] {
                continue;
            }
            let loc = location + self.ball_locs[i];
            if let Some(textures) = &textures {
                let m = Point::from((SHADOW_M, SHADOW_M));
                let size = BALL + SHADOW_M * 2.;
                push(texture_elem(
                    textures.balls[i].clone(),
                    loc - m,
                    Size::from((size, size)),
                    self.ball_alpha_of(i),
                ));
            } else if has_shader {
                push(self.ball_borders[i].clone().with_location(loc).into());
            } else {
                push(
                    SolidColorRenderElement::from_buffer(
                        &self.balls[i],
                        loc,
                        1.,
                        Kind::Unspecified,
                    )
                    .into(),
                );
            }
        }

        if let Some(textures) = &textures {
            if let Some(bar) = &textures.bar {
                let m = Point::from((SHADOW_M, SHADOW_M));
                let size = Size::from((
                    self.island_size.w + SHADOW_M * 2.,
                    self.island_size.h + SHADOW_M * 2.,
                ));
                push(texture_elem(
                    bar.clone(),
                    location + self.island_loc - m,
                    size,
                    self.alpha,
                ));
            }
        }

        // Title-bar fill + divider always (when chrome is visible).
        // Floating (bg_alpha→1): opaque solid. Tiled (bg_alpha→0): frosted glass.
        // First pushed draws on top: divider above tint above blur.
        if self.alpha > 0.001 && self.island_size.w >= 1. && self.island_size.h >= 1. {
            let fill_loc = location + self.island_loc;
            // Hairline on the seam title ↔ client (no rounded corners).
            let div_loc = fill_loc + Point::from((0., self.island_size.h - DIVIDER_H));
            push(
                SolidColorRenderElement::from_buffer(
                    &self.divider,
                    div_loc,
                    self.alpha,
                    Kind::Unspecified,
                )
                .into(),
            );
            if has_shader {
                push(self.title_fill.clone().with_location(fill_loc).into());
            } else {
                push(
                    SolidColorRenderElement::from_buffer(
                        &self.title_bg,
                        fill_loc,
                        1.,
                        Kind::Unspecified,
                    )
                    .into(),
                );
            }

            let glass = (1. - self.bg_alpha).clamp(0., 1.) * self.alpha;
            if glass > 0.001 {
                let fill_radius = CornerRadius {
                    top_left: self.fill_radius,
                    top_right: self.fill_radius,
                    bottom_right: 0.,
                    bottom_left: 0.,
                }
                .fit_to(self.island_size.w as f32, self.island_size.h as f32);
                let geo = Rectangle::new(fill_loc, self.island_size);
                let params = RenderParams {
                    geometry: geo,
                    subregion: None,
                    clip: Some((geo, fill_radius)),
                    scale: self.scale as f64,
                };
                let glass_xray = xray_pos.offset(self.island_loc);
                self.title_glass.render(
                    ctx.as_gles(),
                    None,
                    params,
                    glass_xray,
                    &mut |elem| push(elem.into()),
                );
            }
        }
    }
}

impl Default for WindowChrome {
    fn default() -> Self {
        Self::new()
    }
}

fn pretty_app_name(app_id: &str) -> String {
    if is_browser_web_app(app_id) {
        if let Some(name) = cached_desktop_name(app_id) {
            return name;
        }
    }
    let last = app_id.rsplit('.').next().unwrap_or(app_id);
    let mut chars = last.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => last.to_string(),
    }
}

/// Chromium-family PWAs: `thorium-<id>-Default`, `chrome-<id>-Profile_1`, `crx_<id>`,
/// and `--app=URL` windows: `thorium-<host>__<path>-Default`.
fn is_browser_web_app(app_id: &str) -> bool {
    app_id.contains("__")
        || app_id
            .split(['-', '_'])
            .any(|part| part.len() == 32 && part.bytes().all(|b| (b'a'..=b'p').contains(&b)))
}

fn cached_desktop_name(app_id: &str) -> Option<String> {
    thread_local! {
        static NAMES: RefCell<HashMap<String, Option<String>>> = RefCell::new(HashMap::new());
    }
    NAMES.with_borrow_mut(|names| {
        names
            .entry(app_id.to_owned())
            .or_insert_with(|| desktop_name(app_id))
            .clone()
    })
}

fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match env::var_os("XDG_DATA_HOME") {
        Some(home) if !home.is_empty() => dirs.push(PathBuf::from(home)),
        _ => {
            if let Some(home) = env::var_os("HOME") {
                dirs.push(PathBuf::from(home).join(".local/share"));
            }
        }
    }
    let data_dirs = env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    dirs.extend(
        data_dirs
            .split(':')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from),
    );
    dirs.into_iter().map(|d| d.join("applications")).collect()
}

/// `Name=` from the `[Desktop Entry]` group, matched by file name or `StartupWMClass`.
fn desktop_name(app_id: &str) -> Option<String> {
    let dirs = application_dirs();
    for dir in &dirs {
        if let Ok(text) = fs::read_to_string(dir.join(format!("{app_id}.desktop"))) {
            if let Some((name, _)) = parse_desktop_entry(&text) {
                return Some(name);
            }
        }
    }
    for dir in &dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "desktop") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            if let Some((name, Some(wm_class))) = parse_desktop_entry(&text) {
                if wm_class == app_id {
                    return Some(name);
                }
            }
        }
    }
    None
}

fn parse_desktop_entry(text: &str) -> Option<(String, Option<String>)> {
    let mut in_entry = false;
    let mut name = None;
    let mut wm_class = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            if in_entry {
                break;
            }
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name = Some(v.trim().to_owned());
        } else if let Some(v) = line.strip_prefix("StartupWMClass=") {
            wm_class = Some(v.trim().to_owned());
        }
    }
    name.filter(|n| !n.is_empty()).map(|n| (n, wm_class))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Floating label: app name + title (background is drawn separately when floating).
fn render_bar(
    renderer: &mut GlesRenderer,
    scale: f64,
    bar_w: f64,
    bar_h: f64,
    inset: f64,
    app_id: &str,
    title: &str,
    active: bool,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let logical_w = bar_w + SHADOW_M * 2.;
    let logical_h = bar_h + SHADOW_M * 2.;
    let px_w = (logical_w * scale).round().max(1.) as i32;
    let px_h = (logical_h * scale).round().max(1.) as i32;
    let surface = ImageSurface::create(cairo::Format::ARgb32, px_w, px_h)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(f64::from(px_w) / logical_w, f64::from(px_h) / logical_h);

    let x = SHADOW_M;
    let y = SHADOW_M;

    let text_x = x + inset + TEXT_PAD;
    let text_right = x + bar_w - inset - RIGHT_PAD - WindowChrome::balls_width() - GAP * 2.;
    let text_w = (text_right - text_x).max(0.);
    if text_w > 12. {
        let app = pretty_app_name(app_id);
        let mut title = title.trim();
        // Browser web apps title their windows "App - Page"; drop the repeat.
        const SEPS: [char; 7] = [' ', '-', '–', '—', '|', '·', ':'];
        if let Some(rest) = title.strip_prefix(app.as_str()) {
            if rest.is_empty() || rest.starts_with(SEPS) {
                title = rest.trim_start_matches(SEPS).trim();
            }
        }
        let short_title;
        if title.chars().count() > TITLE_MAX_CHARS {
            let cut: String = title.chars().take(TITLE_MAX_CHARS).collect();
            short_title = format!("{}…", cut.trim_end());
            title = &short_title;
        }
        let markup = if title.is_empty() || title == app {
            format!("<span weight=\"bold\">{}</span>", xml_escape(&app))
        } else {
            format!(
                "<span weight=\"bold\">{}</span> <span alpha=\"38%\">│</span> \
                 <span alpha=\"78%\">{}</span>",
                xml_escape(&app),
                xml_escape(title)
            )
        };

        // Cairo is already in logical pixels.
        let font = FontDescription::from_string("Cascadia Mono SemiBold 14.5px");

        cr.save()?;
        cr.rectangle(text_x - SHADOW_M, 0., text_w + SHADOW_M * 2., logical_h);
        cr.clip();

        let layout = pangocairo::functions::create_layout(&cr);
        layout.context().set_round_glyph_positions(false);
        layout.set_font_description(Some(&font));
        layout.set_markup(&markup);
        layout.set_width((text_w.floor() as i32) * pangocairo::pango::SCALE);
        layout.set_ellipsize(EllipsizeMode::End);
        layout.set_single_paragraph_mode(true);

        let (_tw, th) = layout.pixel_size();
        let text_y = y + ((bar_h - TITLE_GAP - f64::from(th)) * 0.5).max(0.);

        cr.new_path();
        cr.move_to(text_x, text_y);
        if active {
            cr.set_source_rgba(1., 1., 1., 0.96);
        } else {
            cr.set_source_rgba(1., 1., 1., 0.70);
        }
        pangocairo::functions::show_layout(&cr, &layout);
        cr.restore()?;
    }

    drop(cr);
    texture_from_surface(renderer, surface, px_w, px_h, scale)
}

pub(super) fn render_ball(
    renderer: &mut GlesRenderer,
    scale: f64,
    index: usize,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let logical = BALL + SHADOW_M * 2.;
    let px = (logical * scale).round().max(1.) as i32;
    let surface = ImageSurface::create(cairo::Format::ARgb32, px, px)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(f64::from(px) / logical, f64::from(px) / logical);

    let [r, g, b, _] = ball_color(index);
    let (r, g, b) = (f64::from(r), f64::from(g), f64::from(b));

    let c = logical / 2.;
    let rb = BALL / 2.;
    let full = std::f64::consts::TAU;

    let layers = 6;
    for i in (1..=layers).rev() {
        let t = f64::from(i) / f64::from(layers);
        let grow = SHADOW_SPREAD * t;
        let alpha = 0.07 * (-(t * 1.6).powi(2)).exp();
        cr.save()?;
        cr.translate(c, c + grow * 0.35 + 0.6);
        cr.scale(rb + grow, rb + grow * 0.6);
        cr.arc(0., 0., 1., 0., full);
        cr.restore()?;
        cr.set_source_rgba(0., 0., 0., alpha);
        cr.fill()?;
    }

    cr.save()?;
    cr.arc(c, c, rb, 0., full);
    cr.clip();
    cr.set_source_rgba(r, g, b, GLASS_COLOR_ALPHA);
    cr.paint()?;
    let a = GLASS_COLOR_ALPHA;
    let sheen = cairo::LinearGradient::new(0., c - rb, 0., c + rb);
    sheen.add_color_stop_rgba(0.0, 1., 1., 1., GLASS_SHEEN * a);
    sheen.add_color_stop_rgba(0.45, 1., 1., 1., 0.06 * a);
    sheen.add_color_stop_rgba(1.0, 0., 0., 0., 0.12 * a);
    cr.set_source(&sheen)?;
    cr.paint()?;
    cr.restore()?;

    drop(cr);
    texture_from_surface(renderer, surface, px, px, scale)
}

pub(super) fn texture_from_surface(
    renderer: &mut GlesRenderer,
    surface: ImageSurface,
    w: i32,
    h: i32,
    scale: f64,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let data = surface
        .take_data()
        .map_err(|err| anyhow::anyhow!("error taking chrome surface data: {err:?}"))?;
    let buffer = TextureBuffer::from_memory(
        renderer,
        &data,
        Fourcc::Argb8888,
        (w, h),
        false,
        scale,
        Transform::Normal,
        Vec::new(),
    )?;
    Ok(buffer)
}

pub(super) fn render_icon(
    renderer: &mut GlesRenderer,
    scale: f64,
    index: usize,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let px = (BALL * scale).round().max(1.) as i32;
    let surface = ImageSurface::create(cairo::Format::ARgb32, px, px)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(f64::from(px) / 10., f64::from(px) / 10.);
    cr.set_source_rgba(0., 0., 0., 0.6);
    cr.set_line_width(1.3);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);

    match ball_action(index) {
        ChromeAction::Close => {
            cr.move_to(3.2, 3.2);
            cr.line_to(6.8, 6.8);
            cr.move_to(6.8, 3.2);
            cr.line_to(3.2, 6.8);
            cr.stroke()?;
        }
        ChromeAction::Minimize => {
            cr.move_to(2.8, 5.);
            cr.line_to(7.2, 5.);
            cr.stroke()?;
        }
        ChromeAction::Half => {
            cr.set_line_width(1.);
            cr.rectangle(2.5, 3., 5., 4.);
            cr.stroke()?;
            cr.rectangle(2.5, 3., 2.5, 4.);
            cr.fill()?;
        }
        ChromeAction::FullViewport => {
            cr.move_to(3.2, 5.);
            cr.line_to(6.8, 5.);
            cr.stroke()?;
            cr.move_to(2., 5.);
            cr.line_to(3.8, 3.5);
            cr.line_to(3.8, 6.5);
            cr.close_path();
            cr.move_to(8., 5.);
            cr.line_to(6.2, 3.5);
            cr.line_to(6.2, 6.5);
            cr.close_path();
            cr.fill()?;
        }
        ChromeAction::Fullscreen => {
            cr.move_to(2.8, 2.8);
            cr.line_to(6., 2.8);
            cr.line_to(2.8, 6.);
            cr.close_path();
            cr.move_to(7.2, 7.2);
            cr.line_to(4., 7.2);
            cr.line_to(7.2, 4.);
            cr.close_path();
            cr.fill()?;
        }
    }
    drop(cr);
    texture_from_surface(renderer, surface, px, px, scale)
}
