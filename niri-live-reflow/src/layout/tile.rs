use core::f64;
use niri_config::{utils::MergeWith as _, Color, CornerRadius, GradientInterpolation};
use niri_ipc::WindowLayout;
use smithay::{
    backend::renderer::{
        element::{Element, Kind},
        gles::GlesRenderer,
    },
    utils::{Logical, Physical, Point, Rectangle, Scale, Size},
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use super::{
    focus_ring::{FocusRing, FocusRingRenderElement},
    opening_window::{OpenAnimation, OpeningWindowRenderElement},
    shadow::Shadow,
    window_chrome::{ChromeAction, ChromeRenderElement, WindowChrome},
    window_tint::{GlassFade, WindowTint},
    HitType, LayoutElement, LayoutElementRenderElement, LayoutElementRenderSnapshot, Options,
    SizeFrac, RESIZE_ANIMATION_THRESHOLD,
};
use crate::{
    animation::{Animation, Clock, Curve},
    layout::SizingMode,
    niri_render_elements,
    render_helpers::{
        background_effect::BackgroundEffectElement,
        border::BorderRenderElement,
        bottom_fade::BottomFadeRenderElement,
        clipped_surface::{ClippedSurfaceRenderElement, RoundedCornerDamage},
        damage::ExtraDamage,
        offscreen::{OffscreenBuffer, OffscreenRenderElement},
        renderer::NiriRenderer,
        resize::ResizeRenderElement,
        shadow::ShadowRenderElement,
        snapshot::RenderSnapshot,
        solid_color::{SolidColorBuffer, SolidColorRenderElement},
        xray::{Xray, XrayPos},
        RenderCtx, RenderTarget,
    },
    utils::{
        baba_is_float_offset, round_logical_in_physical, round_logical_in_physical_max1,
        transaction::Transaction, ResizeEdge,
    },
};

/// How a mapped window should animate its first appearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAnimKind {
    Tiled,
    Floating,
    Unminimize,
    /// Tiling → floating transition.
    FloatIn,
}

/// Floating window filling the work area (yellow) or the whole output (fullscreen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FloatingExpand {
    #[default]
    None,
    WorkArea,
    Output,
}

/// Toplevel window with decorations.
#[derive(Debug)]
pub struct Tile<W: LayoutElement> {
    /// The toplevel window itself.
    window: W,

    /// The border around the window.
    border: FocusRing,

    /// The focus ring around the window.
    focus_ring: FocusRing,

    /// The shadow around the window.
    shadow: Shadow,

    /// This tile's current sizing mode.
    ///
    /// This will update only when the `window` actually goes maximized or fullscreen, rather than
    /// right away, to avoid black backdrop flicker before the window has had a chance to resize.
    sizing_mode: SizingMode,

    /// The black backdrop for fullscreen windows.
    fullscreen_backdrop: SolidColorBuffer,
    /// Whether this tile is currently in the floating layout.
    is_floating: bool,
    /// Animates title-bar fill alpha: 0 = tiled (no fill), 1 = floating (solid).
    floating_bg_anim: Option<Animation>,

    /// Whether the tile should float upon unfullscreening.
    pub(super) restore_to_floating: bool,

    /// Floating yellow/green expand: fill the working area or the whole output.
    pub(super) floating_expand: FloatingExpand,
    pub(super) floating_expand_restore_pos: Option<Point<f64, SizeFrac>>,
    pub(super) floating_expand_restore_size: Option<Size<i32, Logical>>,

    /// The size that the window should assume when going floating.
    ///
    /// This is generally the last size the window had when it was floating. It can be unknown if
    /// the window starts out in the tiling layout or fullscreen.
    pub(super) floating_window_size: Option<Size<i32, Logical>>,

    /// The position that the tile should assume when going floating, relative to the floating
    /// space working area.
    ///
    /// This is generally the last position the tile had when it was floating. It can be unknown if
    /// the window starts out in the tiling layout.
    pub(super) floating_pos: Option<Point<f64, SizeFrac>>,

    /// Currently selected preset width index when this tile is floating.
    pub(super) floating_preset_width_idx: Option<usize>,

    /// Currently selected preset height index when this tile is floating.
    pub(super) floating_preset_height_idx: Option<usize>,

    /// The animation upon opening a window.
    open_animation: Option<OpenAnimation>,

    /// The animation of the window resizing.
    resize_animation: Option<ResizeAnimation>,

    /// The animation of a tile visually moving horizontally.
    move_x_animation: Option<MoveAnimation>,

    /// The animation of a tile visually moving vertically.
    move_y_animation: Option<MoveAnimation>,

    /// The animation of the tile's opacity.
    pub(super) alpha_animation: Option<AlphaAnimation>,

    /// Brief horizontal shake (refuse-to-join feedback).
    shake_animation: Option<Animation>,

    /// Offset during the initial interactive move rubberband.
    pub(super) interactive_move_offset: Point<f64, Logical>,

    /// Snapshot of the last render for use in the close animation.
    unmap_snapshot: Option<TileRenderSnapshot>,

    /// Extra damage for clipped surface corner radius changes.
    rounded_corner_damage: RoundedCornerDamage,

    /// Bottom clip line for the next render, in the coordinate space of the render location.
    ///
    /// Set by the owner right before rendering; `None` renders the tile unclipped.
    bottom_clip: Cell<Option<f64>>,
    /// Top clip line for the next render (floating groups going above the band).
    top_clip: Cell<Option<f64>>,
    /// Offscreen for fading the tile out above `bottom_clip`; dropped when not fading.
    bottom_fade_offscreen: RefCell<Option<OffscreenBuffer>>,

    /// Always-visible SSD title bar (inside the border, above the client).
    chrome: WindowChrome,
    /// Pointer is over the title bar.
    chrome_hover: bool,
    /// Ball under the pointer, if any (shows that ball's action icon).
    chrome_hover_ball: Option<usize>,
    /// Yellow (work-area) mode only: the title bar is revealed because the
    /// pointer touches the top of the tile.
    chrome_reveal: bool,
    /// Slides the revealed title bar in/out (0 = hidden, 1 = shown).
    chrome_reveal_anim: Option<Animation>,
    /// Floating tile rendered under the tiling layer: dimmed.
    sunk: bool,
    /// Animates the sunk dim (1 = normal, SUNK_ALPHA = dimmed).
    sunk_anim: Option<Animation>,
    /// Offscreen for the sunk dim when no other alpha animation runs.
    sunk_offscreen: RefCell<Option<OffscreenBuffer>>,

    /// Backdrop in the window's own color, when its rules ask for a tint.
    tint: WindowTint,
    /// Fade between the plain look and the tinted glass look.
    glass: GlassFade,

    /// The view size for the tile's workspace.
    ///
    /// Used as the fullscreen target size.
    view_size: Size<f64, Logical>,

    /// Scale of the output the tile is on (and rounds its sizes to).
    scale: f64,

    /// Clock for driving animations.
    pub(super) clock: Clock,

    /// Configurable properties of the layout.
    pub(super) options: Rc<Options>,
}

niri_render_elements! {
    TileRenderElement<R> => {
        LayoutElement = LayoutElementRenderElement<R>,
        FocusRing = FocusRingRenderElement,
        SolidColor = SolidColorRenderElement,
        Opening = OpeningWindowRenderElement,
        Resize = ResizeRenderElement,
        Border = BorderRenderElement,
        Shadow = ShadowRenderElement,
        Chrome = ChromeRenderElement,
        ClippedSurface = ClippedSurfaceRenderElement<R>,
        Offscreen = OffscreenRenderElement,
        BottomFade = BottomFadeRenderElement,
        ExtraDamage = ExtraDamage,
        BackgroundEffect = BackgroundEffectElement,
    }
}

pub type TileRenderSnapshot =
    RenderSnapshot<TileRenderElement<GlesRenderer>, TileRenderElement<GlesRenderer>>;

#[derive(Debug)]
struct ResizeAnimation {
    anim: Animation,
    size_from: Size<f64, Logical>,
    snapshot: LayoutElementRenderSnapshot,
    offscreen: OffscreenBuffer,
    tile_size_from: Size<f64, Logical>,
    // If the resize involved the fullscreen state at some point, this is the progress toward the
    // fullscreen state. Used for things like fullscreen backdrop alpha.
    //
    // Note that this can be set even if this specific resize is between two non-fullscreen states,
    // for example when issuing a new resize during an unfullscreen resize.
    fullscreen_progress: Option<Animation>,
    // Similar to above but for fullscreen-or-maximized.
    expanded_progress: Option<Animation>,
}

#[derive(Debug)]
struct MoveAnimation {
    anim: Animation,
    from: f64,
}

#[derive(Debug)]
pub(super) struct AlphaAnimation {
    pub(super) anim: Animation,
    /// Whether the animation should persist after it's done.
    ///
    /// This is used by things like interactive move which need to animate alpha to
    /// semitransparent, then hold it at semitransparent for a while, until the operation
    /// completes.
    pub(super) hold_after_done: bool,
    offscreen: OffscreenBuffer,
}

impl<W: LayoutElement> Tile<W> {
    pub fn new(
        window: W,
        view_size: Size<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        let rules = window.rules();
        let border_config = options.layout.border.merged_with(&rules.border);
        let focus_ring_config = options.layout.focus_ring.merged_with(&rules.focus_ring);
        let shadow_config = options.layout.shadow.merged_with(&rules.shadow);
        let sizing_mode = window.sizing_mode();
        let glass = GlassFade::new(rules.opacity, rules.background_effect);

        Self {
            window,
            border: FocusRing::new(border_config.into()),
            focus_ring: FocusRing::new(focus_ring_config),
            shadow: Shadow::new(shadow_config),
            sizing_mode,
            fullscreen_backdrop: SolidColorBuffer::new((0., 0.), [0., 0., 0., 1.]),
            is_floating: false,
            floating_bg_anim: None,
            restore_to_floating: false,
            floating_window_size: None,
            floating_pos: None,
            floating_preset_width_idx: None,
            floating_preset_height_idx: None,
            floating_expand: FloatingExpand::None,
            floating_expand_restore_pos: None,
            floating_expand_restore_size: None,
            open_animation: None,
            resize_animation: None,
            move_x_animation: None,
            move_y_animation: None,
            alpha_animation: None,
            shake_animation: None,
            interactive_move_offset: Point::from((0., 0.)),
            unmap_snapshot: None,
            rounded_corner_damage: Default::default(),
            bottom_clip: Cell::new(None),
            top_clip: Cell::new(None),
            bottom_fade_offscreen: RefCell::new(None),
            chrome: WindowChrome::new(),
            chrome_hover: false,
            chrome_hover_ball: None,
            chrome_reveal: false,
            chrome_reveal_anim: None,
            sunk: false,
            sunk_anim: None,
            sunk_offscreen: RefCell::new(None),
            tint: WindowTint::default(),
            glass,
            view_size,
            scale,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        // If preset widths or heights changed, clear our stored preset index.
        if self.options.layout.preset_column_widths != options.layout.preset_column_widths {
            self.floating_preset_width_idx = None;
        }
        if self.options.layout.preset_window_heights != options.layout.preset_window_heights {
            self.floating_preset_height_idx = None;
        }

        self.view_size = view_size;
        self.scale = scale;
        self.options = options;

        self.apply_border_config();

        let round_max1 = |logical| round_logical_in_physical_max1(self.scale, logical);

        let rules = self.window.rules();

        let mut focus_ring_config = self
            .options
            .layout
            .focus_ring
            .merged_with(&rules.focus_ring);
        focus_ring_config.width = round_max1(focus_ring_config.width);
        self.focus_ring.update_config(focus_ring_config);

        let shadow_config = self.options.layout.shadow.merged_with(&rules.shadow);
        self.shadow.update_config(shadow_config);

        self.window.update_config(self.options.blur);
        self.chrome.update_glass(self.options.blur);
    }

    pub fn update_shaders(&mut self) {
        self.border.update_shaders();
        self.focus_ring.update_shaders();
        self.shadow.update_shaders();
        self.chrome.update_shaders();
    }

    pub fn update_window(&mut self) {
        let prev_sizing_mode = self.sizing_mode;
        self.sizing_mode = self.window.sizing_mode();

        if let Some(animate_from) = self.window.take_animation_snapshot() {
            let params = if let Some(resize) = self.resize_animation.take() {
                // Compute like in animated_window_size(), but using the snapshot geometry (since
                // the current one is already overwritten).
                let mut size = animate_from.size;

                let val = resize.anim.value();
                let size_from = resize.size_from;
                let tile_size_from = resize.tile_size_from;

                size.w = size_from.w + (size.w - size_from.w) * val;
                size.h = size_from.h + (size.h - size_from.h) * val;

                let mut tile_size = animate_from.size;
                if prev_sizing_mode.is_fullscreen() {
                    tile_size.w = f64::max(tile_size.w, self.view_size.w);
                    tile_size.h = f64::max(tile_size.h, self.view_size.h);
                } else if prev_sizing_mode.is_normal() && !self.border.is_off() {
                    let width = self.border.width();
                    tile_size.w += width * 2.;
                    tile_size.h += width * 2.;
                }

                tile_size.w = tile_size_from.w + (tile_size.w - tile_size_from.w) * val;
                tile_size.h = tile_size_from.h + (tile_size.h - tile_size_from.h) * val;

                let fullscreen_from = resize
                    .fullscreen_progress
                    .map(|anim| anim.clamped_value().clamp(0., 1.))
                    .unwrap_or(if prev_sizing_mode.is_fullscreen() {
                        1.
                    } else {
                        0.
                    });

                let expanded_from = resize
                    .expanded_progress
                    .map(|anim| anim.clamped_value().clamp(0., 1.))
                    .unwrap_or(if prev_sizing_mode.is_normal() { 0. } else { 1. });

                // Also try to reuse the existing offscreen buffer if we have one.
                (
                    size,
                    tile_size,
                    fullscreen_from,
                    expanded_from,
                    resize.offscreen,
                )
            } else {
                let size = animate_from.size;

                // Compute like in tile_size().
                let mut tile_size = size;
                if prev_sizing_mode.is_fullscreen() {
                    tile_size.w = f64::max(tile_size.w, self.view_size.w);
                    tile_size.h = f64::max(tile_size.h, self.view_size.h);
                } else if prev_sizing_mode.is_normal() && !self.border.is_off() {
                    let width = self.border.width();
                    tile_size.w += width * 2.;
                    tile_size.h += width * 2.;
                }

                let fullscreen_from = if prev_sizing_mode.is_fullscreen() {
                    1.
                } else {
                    0.
                };

                let expanded_from = if prev_sizing_mode.is_normal() { 0. } else { 1. };

                (
                    size,
                    tile_size,
                    fullscreen_from,
                    expanded_from,
                    OffscreenBuffer::default(),
                )
            };
            let (size_from, tile_size_from, fullscreen_from, expanded_from, offscreen) = params;

            let change = self.window.size().to_f64().to_point() - size_from.to_point();
            let change = f64::max(change.x.abs(), change.y.abs());
            let tile_change = self.tile_size().to_f64().to_point() - tile_size_from.to_point();
            let tile_change = f64::max(tile_change.x.abs(), tile_change.y.abs());
            let change = f64::max(change, tile_change);
            if change > RESIZE_ANIMATION_THRESHOLD {
                let anim = Animation::new(
                    self.clock.clone(),
                    0.,
                    1.,
                    0.,
                    self.options.animations.window_resize.anim,
                );

                let fullscreen_to = if self.sizing_mode.is_fullscreen() {
                    1.
                } else {
                    0.
                };
                let expanded_to = if self.sizing_mode.is_normal() { 0. } else { 1. };
                let fullscreen_progress = (fullscreen_from != fullscreen_to)
                    .then(|| anim.restarted(fullscreen_from, fullscreen_to, 0.));
                let expanded_progress = (expanded_from != expanded_to)
                    .then(|| anim.restarted(expanded_from, expanded_to, 0.));

                self.resize_animation = Some(ResizeAnimation {
                    anim,
                    size_from,
                    snapshot: animate_from,
                    offscreen,
                    tile_size_from,
                    fullscreen_progress,
                    expanded_progress,
                });
            } else {
                self.resize_animation = None;
            }
        }

        self.apply_border_config();

        let round_max1 = |logical| round_logical_in_physical_max1(self.scale, logical);

        let rules = self.window.rules();

        let mut focus_ring_config = self
            .options
            .layout
            .focus_ring
            .merged_with(&rules.focus_ring);
        focus_ring_config.width = round_max1(focus_ring_config.width);
        self.focus_ring.update_config(focus_ring_config);

        let shadow_config = self.options.layout.shadow.merged_with(&rules.shadow);
        self.shadow.update_config(shadow_config);

        let window_size = self.window_size();
        let radius = self
            .window
            .geometry_corner_radius()
            .fit_to(window_size.w as f32, window_size.h as f32);
        self.rounded_corner_damage.set_corner_radius(radius);
    }

    pub fn advance_animations(&mut self) {
        if let Some(anim) = &mut self.chrome_reveal_anim {
            if anim.is_done() {
                self.chrome_reveal_anim = None;
            }
        }
        if let Some(anim) = &mut self.sunk_anim {
            if anim.is_done() {
                self.sunk_anim = None;
            }
        }
        if let Some(anim) = &mut self.floating_bg_anim {
            if anim.is_done() {
                self.floating_bg_anim = None;
            }
        }

        if let Some(open) = &mut self.open_animation {
            if open.is_done() {
                self.open_animation = None;
            }
        }

        if let Some(resize) = &mut self.resize_animation {
            if resize.anim.is_done() {
                self.resize_animation = None;
            }
        }

        if let Some(move_) = &mut self.move_x_animation {
            if move_.anim.is_done() {
                self.move_x_animation = None;
            }
        }
        if let Some(move_) = &mut self.move_y_animation {
            if move_.anim.is_done() {
                self.move_y_animation = None;
            }
        }
        if let Some(shake) = &mut self.shake_animation {
            if shake.is_done() {
                self.shake_animation = None;
            }
        }

        if let Some(alpha) = &mut self.alpha_animation {
            if !alpha.hold_after_done && alpha.anim.is_done() {
                self.alpha_animation = None;
            }
        }

        self.glass.advance();
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.are_transitions_ongoing()
            || self.glass.is_animating()
            || self.window.rules().baba_is_float == Some(true)
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        self.open_animation.is_some()
            || self.resize_animation.is_some()
            || self.move_x_animation.is_some()
            || self.move_y_animation.is_some()
            || self.shake_animation.is_some()
            || self.floating_bg_anim.is_some()
            || self.chrome_reveal_anim.is_some()
            || self.sunk_anim.is_some()
            || self
                .alpha_animation
                .as_ref()
                .is_some_and(|alpha| !alpha.anim.is_done())
    }

    /// Extra height reserved for the always-visible SSD title bar.
    fn chrome_extra_h(&self) -> f64 {
        // Yellow / orange: the client owns the whole tile. In yellow the title
        // bar is only an overlay revealed from the top edge.
        if self.floating_expand != FloatingExpand::None {
            return 0.;
        }
        if self.sizing_mode.is_normal() {
            WindowChrome::extra_height()
        } else {
            0.
        }
    }

    /// Whether a title bar can be shown at all: the reserved bar, or the
    /// overlay revealed from the top edge in yellow mode.
    fn chrome_active(&self) -> bool {
        self.chrome_extra_h() > 0.
            || (self.floating_expand == FloatingExpand::WorkArea && self.sizing_mode.is_normal())
    }

    /// Height of the top-edge strip that reveals the overlay title bar.
    const REVEAL_ZONE: f64 = 2.;

    /// Yellow mode: should the overlay title bar be shown for a pointer at
    /// `point` (tile coordinates)? Touching the top edge reveals it; it stays
    /// while the pointer is over the bar.
    pub fn wants_chrome_reveal(&self, point: Point<f64, Logical>) -> bool {
        if self.floating_expand != FloatingExpand::WorkArea || !self.sizing_mode.is_normal() {
            return false;
        }
        let size = self.tile_size();
        // Also count the gap above the tile (the pointer sits at the screen top).
        if point.x < 0. || point.x >= size.w || point.y < -Self::REVEAL_ABOVE {
            return false;
        }
        let limit = if self.chrome_reveal {
            super::window_chrome::BAR_H + 4.
        } else {
            Self::REVEAL_ZONE
        };
        point.y < limit
    }

    /// How far above the tile the pointer still reveals the bar.
    const REVEAL_ABOVE: f64 = 24.;
    /// Reveal slide duration in ms.
    const REVEAL_MS: u64 = 180;

    /// Leave yellow/orange: reserved title bar and border come back; the
    /// overlay reveal is dropped. Does not touch the window size.
    pub(super) fn clear_floating_expand(&mut self) {
        self.floating_expand = FloatingExpand::None;
        self.floating_expand_restore_pos = None;
        self.floating_expand_restore_size = None;
        self.chrome_reveal = false;
        self.chrome_reveal_anim = None;
        self.apply_border_config();
    }

    /// Whether this tile is in the yellow (work-area) floating mode.
    pub fn is_floating_work_area(&self) -> bool {
        self.floating_expand == FloatingExpand::WorkArea
    }

    /// Floating tile under the tiling layer: dim it. Returns true if changed.
    pub fn set_sunk(&mut self, sunk: bool) -> bool {
        if self.sunk == sunk {
            return false;
        }
        let from = f64::from(self.sunk_alpha());
        self.sunk = sunk;
        let to = if sunk { Self::SUNK_ALPHA } else { 1. };
        self.sunk_anim = Some(Animation::ease(
            self.clock.clone(),
            from,
            to,
            0.,
            160,
            crate::animation::Curve::EaseOutCubic,
        ));
        true
    }

    /// Opacity of floating windows sitting under the tiling layer.
    const SUNK_ALPHA: f64 = 0.65;

    fn sunk_alpha(&self) -> f32 {
        if let Some(anim) = &self.sunk_anim {
            anim.clamped_value().clamp(0., 1.) as f32
        } else if self.sunk {
            Self::SUNK_ALPHA as f32
        } else {
            1.
        }
    }

    /// Y offset of the overlay title bar while it slides in from the top edge.
    fn chrome_reveal_offset(&self) -> f64 {
        if self.floating_expand != FloatingExpand::WorkArea {
            return 0.;
        }
        -(1. - f64::from(self.chrome_reveal_alpha())) * super::window_chrome::BAR_H
    }

    /// Returns true if the reveal state changed.
    pub fn set_chrome_reveal(&mut self, reveal: bool) -> bool {
        let reveal = reveal && self.floating_expand == FloatingExpand::WorkArea;
        if self.chrome_reveal == reveal {
            return false;
        }
        let from = self.chrome_reveal_alpha();
        self.chrome_reveal = reveal;
        self.chrome_reveal_anim = Some(Animation::ease(
            self.clock.clone(),
            f64::from(from),
            if reveal { 1. } else { 0. },
            0.,
            Self::REVEAL_MS,
            crate::animation::Curve::EaseOutCubic,
        ));
        true
    }

    fn chrome_reveal_alpha(&self) -> f32 {
        if let Some(anim) = &self.chrome_reveal_anim {
            anim.clamped_value().clamp(0., 1.) as f32
        } else if self.chrome_reveal {
            1.
        } else {
            0.
        }
    }

    /// Windows-like outline for floating; niri layout.border for tiling.
    pub(super) fn apply_border_config(&mut self) {
        let round_max1 = |logical| round_logical_in_physical_max1(self.scale, logical);
        let rules = self.window.rules();
        let mut border_config = self.options.layout.border.merged_with(&rules.border);
        if self.floating_expand == FloatingExpand::Output {
            border_config.off = true;
        } else if self.is_floating {
            // Windows 11-style thin neutral frame around the SSD.
            border_config.off = false;
            border_config.width = round_max1(1.);
            border_config.active_color = Color::from_rgba8_unpremul(196, 196, 196, 255);
            border_config.inactive_color = Color::from_rgba8_unpremul(56, 56, 56, 255);
            border_config.urgent_color = Color::from_rgba8_unpremul(56, 56, 56, 255);
            border_config.active_gradient = None;
            border_config.inactive_gradient = None;
            border_config.urgent_gradient = None;
        } else {
            border_config.width = round_max1(border_config.width);
        }
        if self.floating_expand == FloatingExpand::WorkArea {
            border_config.off = true;
        }
        self.border.update_config(border_config.into());
    }

    /// Drive title-bar fill when the tile moves between tiled and floating.
    /// Tiled = transparent title bar; floating = solid fill (Noctalia bar color).
    /// Call as soon as floating state flips (held-drag float, click-to-tile, toggle).
    pub fn set_floating(&mut self, floating: bool) {
        if self.is_floating == floating {
            return;
        }
        self.is_floating = floating;
        self.apply_border_config();
        if !floating {
            self.floating_expand = FloatingExpand::None;
            self.floating_expand_restore_pos = None;
            self.floating_expand_restore_size = None;
            self.chrome_reveal = false;
            self.chrome_reveal_anim = None;
            self.sunk = false;
            self.sunk_anim = None;
        }
        let from = self
            .floating_bg_anim
            .as_ref()
            .map_or(if floating { 0. } else { 1. }, |a| a.clamped_value());
        let to = if floating { 1. } else { 0. };
        // Short ease so the fill appears/disappears as soon as the state flips,
        // not after the longer window-movement spring.
        self.floating_bg_anim = Some(Animation::ease(
            self.clock.clone(),
            from,
            to,
            0.,
            90,
            crate::animation::Curve::EaseOutCubic,
        ));
    }

    fn floating_bg_alpha(&self) -> f32 {
        if let Some(anim) = &self.floating_bg_anim {
            anim.clamped_value().clamp(0., 1.) as f32
        } else if self.is_floating {
            1.
        } else {
            0.
        }
    }

    /// Mark the window as a group member (dims title bar balls that do nothing there).
    pub fn set_chrome_in_group(&mut self, in_group: bool) {
        self.chrome.set_in_group(in_group);
    }

    /// Returns true if the hover state changed.
    pub fn set_chrome_hover(&mut self, hover: bool, ball: Option<usize>) -> bool {
        let changed = self.chrome_hover != hover || self.chrome_hover_ball != ball;
        self.chrome_hover = hover;
        self.chrome_hover_ball = ball;
        changed
    }

    pub fn chrome_hit(&self, point: Point<f64, Logical>) -> Option<ChromeAction> {
        if !self.chrome_active() {
            return None;
        }
        self.chrome.hit(point - self.bob_offset())
    }

    pub fn chrome_hit_index(&self, point: Point<f64, Logical>) -> Option<usize> {
        if !self.chrome_active() {
            return None;
        }
        self.chrome.hit_index(point - self.bob_offset())
    }

    pub fn chrome_consumes_click(&self, point: Point<f64, Logical>) -> bool {
        if !self.chrome_active() {
            return false;
        }
        self.chrome.consumes_click(point - self.bob_offset())
    }

    /// Resize edges for a point in the zone around the window frame (border
    /// plus `reach` outside of it), excluding the window itself and the label
    /// strip above it.
    pub fn frame_edges_at(&self, point: Point<f64, Logical>, reach: f64) -> ResizeEdge {
        // Yellow/orange have no frame: no edge resize, no resize cursor.
        if !self.sizing_mode.is_normal() || self.floating_expand != FloatingExpand::None {
            return ResizeEdge::empty();
        }

        let point = point - self.bob_offset();
        let tile = Rectangle::from_size(self.tile_size());
        let outer = Rectangle::new(
            tile.loc - Point::from((reach, reach)),
            tile.size + Size::from((reach * 2., reach * 2.)),
        );
        let win = Rectangle::new(self.window_loc(), self.window_size());
        if !outer.contains(point) || win.contains(point) || self.chrome.consumes_click(point) {
            return ResizeEdge::empty();
        }

        let frame_top = self.chrome_extra_h();
        let mut edges = ResizeEdge::empty();
        if point.x < win.loc.x {
            edges |= ResizeEdge::LEFT;
        } else if point.x >= win.loc.x + win.size.w {
            edges |= ResizeEdge::RIGHT;
        }
        // Beside the label strip only the side edges apply.
        if point.y < 0. || (point.y >= frame_top && point.y < win.loc.y) {
            edges |= ResizeEdge::TOP;
        } else if point.y >= win.loc.y + win.size.h {
            edges |= ResizeEdge::BOTTOM;
        }
        edges
    }

    pub fn update_render_elements(&mut self, is_active: bool, view_rect: Rectangle<f64, Logical>) {
        let rules = self.window.rules();
        self.glass
            .update(&self.clock, rules.opacity, rules.background_effect);
        let animated_tile_size = self.animated_tile_size();
        let expanded_progress = self.expanded_progress();

        let draw_border_with_background = rules
            .draw_border_with_background
            .unwrap_or_else(|| !self.window.has_ssd());
        let border_width = self.visual_border_width().unwrap_or(0.);

        // Do the inverse of tile_size() in order to handle the unfullscreen animation for windows
        // that were smaller than the fullscreen size, and therefore their animated_window_size() is
        // currently much smaller than the tile size.
        let mut border_window_size = animated_tile_size;
        border_window_size.w -= border_width * 2.;
        border_window_size.h -= border_width * 2.;

        // FIXME: this takes into account the animation from normal sizing mode to
        // maximized/fullscreen, but it doesn't take into account the corner radius animation from
        // the window itself.
        //
        // Currently, an easy way to see the problem is to start from a window with a nonzero
        // radius, then go from windowed fullscreen (that forces 0 radius) to regular fullscreen.
        // At the start of the animation, windowed fullscreen becomes false, but the window hasn't
        // animated to the normal fullscreen yet, so the radius here jumps to its nonzero value,
        // even though it should remain zero throughout.
        //
        // Later, when windows get the surface shape protocol with radii, this issue will happen
        // when that changes between animated commits.
        let radius = self
            .window
            .geometry_corner_radius()
            .expanded_by(border_width as f32)
            .scaled_by(1. - expanded_progress as f32);

        // Outer border/shadow/focus ring wrap the whole tile (title bar + client).
        // Title bar is the top band inside that frame; no separate "cap" above the border.
        let frame_size = animated_tile_size;
        let geo_radius = self
            .window
            .geometry_corner_radius()
            .scaled_by(1. - expanded_progress as f32);

        let chrome_alpha = if self.floating_expand == FloatingExpand::Output {
            0.
        } else if self.floating_expand == FloatingExpand::WorkArea {
            self.chrome_reveal_alpha()
        } else {
            1. - expanded_progress as f32
        };
        self.chrome.update(
            animated_tile_size,
            border_width,
            chrome_alpha,
            self.scale,
            self.window.title().unwrap_or_default(),
            self.window.app_id().unwrap_or_default(),
            is_active,
            self.floating_bg_alpha(),
            geo_radius.top_left.max(geo_radius.top_right),
        );
        self.border.update_render_elements(
            border_window_size,
            is_active,
            !draw_border_with_background,
            self.window.is_urgent(),
            Rectangle::new(
                view_rect.loc - Point::from((border_width, border_width)),
                view_rect.size,
            ),
            radius,
            self.scale,
            1. - expanded_progress as f32,
        );

        let radius = if self.visual_border_width().is_some() {
            radius
        } else {
            geo_radius
        };
        self.shadow.update_render_elements(
            frame_size,
            is_active,
            radius,
            self.scale,
            1. - expanded_progress as f32,
        );

        let draw_focus_ring_with_background = if self.border.is_off() {
            draw_border_with_background
        } else {
            false
        };
        let radius = radius.expanded_by(self.focus_ring.width() as f32);
        self.focus_ring.update_render_elements(
            frame_size,
            is_active,
            !draw_focus_ring_with_background,
            self.window.is_urgent(),
            view_rect,
            radius,
            self.scale,
            1. - expanded_progress as f32,
        );

        self.fullscreen_backdrop.resize(animated_tile_size);
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn render_offset(&self) -> Point<f64, Logical> {
        let mut offset = Point::from((0., 0.));

        if let Some(move_) = &self.move_x_animation {
            offset.x += move_.from * move_.anim.value();
        }
        if let Some(move_) = &self.move_y_animation {
            offset.y += move_.from * move_.anim.value();
        }
        if let Some(shake) = &self.shake_animation {
            let t = shake.value().clamp(0., 1.);
            // Three quick oscillations that fade out.
            offset.x += (t * std::f64::consts::PI * 6.).sin() * 10. * (1. - t);
        }

        offset += self.interactive_move_offset;

        offset
    }

    /// Horizontal shake used when a group join is refused.
    pub fn start_shake(&mut self) {
        self.shake_animation = Some(Animation::ease(
            self.clock.clone(),
            0.,
            1.,
            0.,
            320,
            crate::animation::Curve::Linear,
        ));
    }

    pub fn start_open_animation(&mut self) {
        self.start_open_animation_kind(OpenAnimKind::Tiled);
    }

    pub fn start_open_animation_kind(&mut self, kind: OpenAnimKind) {
        let anim = match kind {
            OpenAnimKind::Tiled => Animation::new(
                self.clock.clone(),
                0.,
                1.,
                0.,
                self.options.animations.window_open.anim,
            ),
            OpenAnimKind::Floating => {
                Animation::ease(self.clock.clone(), 0., 1., 0., 240, Curve::EaseOutCubic)
            }
            OpenAnimKind::Unminimize => {
                Animation::ease(self.clock.clone(), 0., 1., 0., 220, Curve::EaseOutExpo)
            }
            OpenAnimKind::FloatIn => {
                Animation::ease(self.clock.clone(), 0., 1., 0., 180, Curve::EaseOutCubic)
            }
        };
        let scale_from = match kind {
            OpenAnimKind::Tiled => 0.88,
            OpenAnimKind::Floating => 0.70,
            OpenAnimKind::Unminimize => 0.42,
            OpenAnimKind::FloatIn => 0.92,
        };
        self.open_animation = Some(OpenAnimation::with_scale(anim, scale_from));
    }

    pub fn resize_animation(&self) -> Option<&Animation> {
        self.resize_animation.as_ref().map(|resize| &resize.anim)
    }

    pub fn animate_move_from(&mut self, from: Point<f64, Logical>) {
        self.animate_move_x_from(from.x);
        self.animate_move_y_from(from.y);
    }

    pub fn animate_move_x_from(&mut self, from: f64) {
        self.animate_move_x_from_with_config(from, self.options.animations.window_movement.0);
    }

    pub fn animate_move_x_from_with_config(&mut self, from: f64, config: niri_config::Animation) {
        let current_offset = self.render_offset().x;

        // Preserve the previous config if ongoing.
        let anim = self.move_x_animation.take().map(|move_| move_.anim);
        let anim = anim
            .map(|anim| anim.restarted(1., 0., 0.))
            .unwrap_or_else(|| Animation::new(self.clock.clone(), 1., 0., 0., config));

        self.move_x_animation = Some(MoveAnimation {
            anim,
            from: from + current_offset,
        });
    }

    pub fn animate_move_y_from(&mut self, from: f64) {
        self.animate_move_y_from_with_config(from, self.options.animations.window_movement.0);
    }

    pub fn animate_move_y_from_with_config(&mut self, from: f64, config: niri_config::Animation) {
        let current_offset = self.render_offset().y;

        // Preserve the previous config if ongoing.
        let anim = self.move_y_animation.take().map(|move_| move_.anim);
        let anim = anim
            .map(|anim| anim.restarted(1., 0., 0.))
            .unwrap_or_else(|| Animation::new(self.clock.clone(), 1., 0., 0., config));

        self.move_y_animation = Some(MoveAnimation {
            anim,
            from: from + current_offset,
        });
    }

    pub fn offset_move_y_anim_current(&mut self, offset: f64) {
        if let Some(move_) = self.move_y_animation.as_mut() {
            // If the anim is almost done, there's little point trying to offset it; we can let
            // things jump. If it turns out like a bad idea, we could restart the anim instead.
            let value = move_.anim.value();
            if value > 0.001 {
                move_.from += offset / value;
            }
        }
    }

    pub fn stop_move_animations(&mut self) {
        self.move_x_animation = None;
        self.move_y_animation = None;
    }

    pub fn animate_alpha(&mut self, from: f64, to: f64, config: niri_config::Animation) {
        let from = from.clamp(0., 1.);
        let to = to.clamp(0., 1.);

        let (current, offscreen) = if let Some(alpha) = self.alpha_animation.take() {
            (alpha.anim.clamped_value(), alpha.offscreen)
        } else {
            (from, OffscreenBuffer::default())
        };

        self.alpha_animation = Some(AlphaAnimation {
            anim: Animation::new(self.clock.clone(), current, to, 0., config),
            hold_after_done: false,
            offscreen,
        });
    }

    pub fn ensure_alpha_animates_to_1(&mut self) {
        if let Some(alpha) = &self.alpha_animation {
            if alpha.anim.to() != 1. {
                // Cancel animation instead of starting a new one because the user likely wants to
                // see the tile right away.
                self.alpha_animation = None;
            }
        }
    }

    pub fn hold_alpha_animation_after_done(&mut self) {
        if let Some(alpha) = &mut self.alpha_animation {
            alpha.hold_after_done = true;
        }
    }

    pub fn window(&self) -> &W {
        &self.window
    }

    pub fn window_mut(&mut self) -> &mut W {
        &mut self.window
    }

    pub fn sizing_mode(&self) -> SizingMode {
        self.sizing_mode
    }

    fn fullscreen_progress(&self) -> f64 {
        if let Some(resize) = &self.resize_animation {
            if let Some(anim) = &resize.fullscreen_progress {
                return anim.clamped_value().clamp(0., 1.);
            }
        }

        if self.sizing_mode.is_fullscreen() {
            1.
        } else {
            0.
        }
    }

    fn expanded_progress(&self) -> f64 {
        if let Some(resize) = &self.resize_animation {
            if let Some(anim) = &resize.expanded_progress {
                return anim.clamped_value().clamp(0., 1.);
            }
        }

        if self.sizing_mode.is_normal() {
            0.
        } else {
            1.
        }
    }

    /// Returns `None` if the border is hidden and `Some(width)` if it should be shown.
    pub fn effective_border_width(&self) -> Option<f64> {
        if !self.sizing_mode.is_normal() {
            return None;
        }

        if self.border.is_off() {
            return None;
        }

        Some(self.border.width())
    }

    fn visual_border_width(&self) -> Option<f64> {
        if self.border.is_off() {
            return None;
        }

        let expanded_progress = self.expanded_progress();

        // Only hide the border when fully expanded to avoid jarring border appearance.
        if expanded_progress == 1. {
            return None;
        }

        // FIXME: would be cool to, like, gradually resize the border from full width to 0 during
        // fullscreening, but the rest of the code isn't quite ready for that yet. It needs to
        // handle things like computing intermediate tile size when an animated resize starts during
        // an animated unfullscreen resize.
        Some(self.border.width())
    }

    /// Returns the location of the window's visual geometry within this Tile.
    pub fn window_loc(&self) -> Point<f64, Logical> {
        let mut loc = Point::from((0., 0.));

        let window_size = self.animated_window_size();
        let target_size = self.animated_tile_size();

        // Center the window within its tile.
        //
        // - Without borders, the sizes match, so this difference is zero.
        // - Borders always match from all sides, so this difference is pre-rounded to physical.
        // - In fullscreen, if the window is smaller than the tile, then it gets centered, otherwise
        //   the tile size matches the window.
        // - During animations, the window remains centered within the tile; this is important for
        //   the to/from fullscreen animation.
        loc.x += (target_size.w - window_size.w) / 2.;
        let extra_h = self.chrome_extra_h();
        loc.y += extra_h + (target_size.h - extra_h - window_size.h) / 2.;

        // Round to physical pixels.
        loc = loc
            .to_physical_precise_round(self.scale)
            .to_logical(self.scale);

        loc
    }

    pub fn tile_size(&self) -> Size<f64, Logical> {
        let mut size = self.window_size();

        if self.sizing_mode.is_fullscreen() {
            // Normally we'd just return the fullscreen size here, but this makes things a bit
            // nicer if a fullscreen window is bigger than the fullscreen size for some reason.
            size.w = f64::max(size.w, self.view_size.w);
            size.h = f64::max(size.h, self.view_size.h);
            return size;
        }

        if let Some(width) = self.effective_border_width() {
            size.w += width * 2.;
            size.h += width * 2.;
        }

        size.h += self.chrome_extra_h();

        size
    }

    pub fn tile_expected_or_current_size(&self) -> Size<f64, Logical> {
        let mut size = self.window_expected_or_current_size();

        if self.sizing_mode.is_fullscreen() {
            // Normally we'd just return the fullscreen size here, but this makes things a bit
            // nicer if a fullscreen window is bigger than the fullscreen size for some reason.
            size.w = f64::max(size.w, self.view_size.w);
            size.h = f64::max(size.h, self.view_size.h);
            return size;
        }

        if let Some(width) = self.effective_border_width() {
            size.w += width * 2.;
            size.h += width * 2.;
        }

        size.h += self.chrome_extra_h();

        size
    }

    pub fn window_size(&self) -> Size<f64, Logical> {
        let mut size = self.window.size().to_f64();
        size = size
            .to_physical_precise_round(self.scale)
            .to_logical(self.scale);
        size
    }

    pub fn window_expected_or_current_size(&self) -> Size<f64, Logical> {
        let size = self.window.expected_size();
        let mut size = size.unwrap_or_else(|| self.window.size()).to_f64();
        size = size
            .to_physical_precise_round(self.scale)
            .to_logical(self.scale);
        size
    }

    pub fn animated_window_size(&self) -> Size<f64, Logical> {
        let mut size = self.window_size();

        if let Some(resize) = &self.resize_animation {
            let val = resize.anim.value();
            let size_from = resize.size_from.to_f64();

            size.w = f64::max(1., size_from.w + (size.w - size_from.w) * val);
            size.h = f64::max(1., size_from.h + (size.h - size_from.h) * val);
            size = size
                .to_physical_precise_round(self.scale)
                .to_logical(self.scale);
        }

        size
    }

    pub fn animated_tile_size(&self) -> Size<f64, Logical> {
        let mut size = self.tile_size();

        if let Some(resize) = &self.resize_animation {
            let val = resize.anim.value();
            let size_from = resize.tile_size_from.to_f64();

            size.w = f64::max(1., size_from.w + (size.w - size_from.w) * val);
            size.h = f64::max(1., size_from.h + (size.h - size_from.h) * val);
            size = size
                .to_physical_precise_round(self.scale)
                .to_logical(self.scale);
        }

        size
    }

    pub fn buf_loc(&self) -> Point<f64, Logical> {
        let mut loc = Point::from((0., 0.));
        loc += self.window_loc();
        loc += self.window.buf_loc().to_f64();
        loc
    }

    /// Returns a partially-filled [`WindowLayout`].
    ///
    /// Only the sizing properties that a [`Tile`] can fill are filled.
    pub fn ipc_layout_template(&self) -> WindowLayout {
        WindowLayout {
            pos_in_scrolling_layout: None,
            tile_size: self.tile_size().into(),
            window_size: self.window().size().into(),
            tile_pos_in_workspace_view: None,
            window_offset_in_tile: self.window_loc().into(),
        }
    }

    fn is_in_input_region(&self, mut point: Point<f64, Logical>) -> bool {
        point -= self.window_loc().to_f64();
        self.window.is_in_input_region(point)
    }

    fn is_in_activation_region(&self, point: Point<f64, Logical>) -> bool {
        let activation_region = Rectangle::from_size(self.tile_size());
        activation_region.contains(point)
    }

    pub fn hit(&self, point: Point<f64, Logical>) -> Option<HitType> {
        let offset = self.bob_offset();
        let point = point - offset;

        // Yellow: the revealed overlay bar sits on the client; its clicks are ours.
        if self.chrome_extra_h() <= 0.
            && self.chrome_active()
            && self.chrome.is_visible()
            && self.chrome.consumes_click(point)
        {
            return Some(HitType::Activate {
                is_tab_indicator: false,
            });
        }

        if self.is_in_input_region(point) {
            let win_pos = self.buf_loc() + offset;
            Some(HitType::Input { win_pos })
        } else if self.is_in_activation_region(point) {
            Some(HitType::Activate {
                is_tab_indicator: false,
            })
        } else {
            None
        }
    }

    pub fn request_tile_size(
        &mut self,
        mut size: Size<f64, Logical>,
        animate: bool,
        transaction: Option<Transaction>,
    ) {
        // Can't go through effective_border_width() because we might be fullscreen.
        if !self.border.is_off() {
            let width = self.border.width();
            size.w = f64::max(1., size.w - width * 2.);
            size.h = f64::max(1., size.h - width * 2.);
        }

        size.h = f64::max(1., size.h - self.chrome_extra_h());

        // The size request has to be i32 unfortunately, due to Wayland. We floor here instead of
        // round to avoid situations where proportionally-sized columns don't fit on the screen
        // exactly.
        self.window.request_size(
            size.to_i32_floor(),
            SizingMode::Normal,
            animate,
            transaction,
        );
    }

    /// Window size (as a floating tile) so the whole frame, border and title
    /// bar included, is `side` × `side`. Respects the window's min/max size.
    pub fn square_window_size(&self, side: f64) -> Size<i32, Logical> {
        let w = self.window_width_for_tile_width(side).floor().max(1.) as i32;
        let h = self.window_height_for_tile_height(side).floor().max(1.) as i32;
        let min = self.window.min_size();
        let max = self.window.max_size();
        Size::from((
            crate::utils::ensure_min_max_size(w, min.w, max.w),
            crate::utils::ensure_min_max_size(h, min.h, max.h),
        ))
    }

    pub fn tile_width_for_window_width(&self, size: f64) -> f64 {
        if self.border.is_off() {
            size
        } else {
            size + self.border.width() * 2.
        }
    }

    pub fn tile_height_for_window_height(&self, size: f64) -> f64 {
        let mut size = if self.border.is_off() {
            size
        } else {
            size + self.border.width() * 2.
        };
        size += self.chrome_extra_h();
        size
    }

    pub fn window_width_for_tile_width(&self, size: f64) -> f64 {
        if self.border.is_off() {
            size
        } else {
            size - self.border.width() * 2.
        }
    }

    pub fn window_height_for_tile_height(&self, size: f64) -> f64 {
        let mut size = if self.border.is_off() {
            size
        } else {
            size - self.border.width() * 2.
        };
        size -= self.chrome_extra_h();
        size
    }

    pub fn request_maximized(
        &mut self,
        size: Size<f64, Logical>,
        animate: bool,
        transaction: Option<Transaction>,
    ) {
        self.window.request_size(
            size.to_i32_round(),
            SizingMode::Maximized,
            animate,
            transaction,
        );
    }

    pub fn request_fullscreen(&mut self, animate: bool, transaction: Option<Transaction>) {
        self.window.request_size(
            self.view_size.to_i32_round(),
            SizingMode::Fullscreen,
            animate,
            transaction,
        );
    }

    pub fn min_size_nonfullscreen(&self) -> Size<f64, Logical> {
        let mut size = self.window.min_size().to_f64();

        // Can't go through effective_border_width() because we might be fullscreen.
        if !self.border.is_off() {
            let width = self.border.width();

            size.w = f64::max(1., size.w);
            size.h = f64::max(1., size.h);

            size.w += width * 2.;
            size.h += width * 2.;
        }

        if size.h > 0. {
            size.h += WindowChrome::extra_height();
        }

        size
    }

    pub fn max_size_nonfullscreen(&self) -> Size<f64, Logical> {
        let mut size = self.window.max_size().to_f64();

        // Can't go through effective_border_width() because we might be fullscreen.
        if !self.border.is_off() {
            let width = self.border.width();

            if size.w > 0. {
                size.w += width * 2.;
            }
            if size.h > 0. {
                size.h += width * 2.;
            }
        }

        if size.h > 0. {
            size.h += WindowChrome::extra_height();
        }

        size
    }

    pub fn bob_offset(&self) -> Point<f64, Logical> {
        if self.window.rules().baba_is_float != Some(true) {
            return Point::from((0., 0.));
        }

        let y = baba_is_float_offset(self.clock.now(), self.view_size.h);
        let y = round_logical_in_physical(self.scale, y);
        Point::from((0., y))
    }

    fn render_inner<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        location: Point<f64, Logical>,
        mut xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(TileRenderElement<R>),
    ) {
        let _span = tracy_client::span!("Tile::render_inner");

        let scale = Scale::from(self.scale);
        let fullscreen_progress = self.fullscreen_progress();
        let expanded_progress = self.expanded_progress();

        let win_alpha = if self.window.is_ignoring_opacity_window_rule() {
            1.
        } else {
            let alpha = self.glass.alpha();

            // Interpolate towards alpha = 1. at fullscreen.
            let p = fullscreen_progress as f32;
            alpha * (1. - p) + 1. * p
        };

        // This is here rather than in render_offset() because render_offset() is currently assumed
        // by the code to be temporary. So, for example, interactive move will try to "grab" the
        // tile at its current render offset and reset the render offset to zero by cancelling the
        // tile move animations. On the other hand, bob_offset() is not resettable, so adding it in
        // render_offset() would cause obvious animation glitches.
        //
        // This isn't to say that adding it here is perfect; indeed, it kind of breaks view_rect
        // passed to update_render_elements(). But, it works well enough for what it is.
        let bob_offset = self.bob_offset();
        let location = location + bob_offset;
        xray_pos = xray_pos.offset(bob_offset);

        let window_loc = self.window_loc();
        let window_size = self.window_size();
        let animated_window_size = self.animated_window_size();
        let window_render_loc = location + window_loc;
        let area = Rectangle::new(window_render_loc, animated_window_size);
        // Title-bar glass blur uses tile-space xray; client content uses window-space.
        let tile_xray = xray_pos;
        xray_pos = xray_pos.offset(window_loc);

        let rules = self.window.rules();

        // Clip to geometry including during the fullscreen animation to help with buggy clients
        // that submit a full-sized buffer before acking the fullscreen state (Firefox).
        let clip_to_geometry = fullscreen_progress < 1. && rules.clip_to_geometry == Some(true);
        let mut radius = self
            .window
            .geometry_corner_radius()
            .scaled_by(1. - expanded_progress as f32);
        // Title bar owns the top of the frame: client seam is square (no rounded
        // divider). Keep bottom corners for the outer frame look.
        if self.chrome_extra_h() > 0. {
            radius.top_left = 0.;
            radius.top_right = 0.;
        }

        // Popups go on top, whether it's resize or not.
        self.window.render_popups(
            ctx.r(),
            window_render_loc,
            scale,
            win_alpha,
            xray_pos,
            &mut |elem| push(elem.into()),
        );

        // First pushed element is on top. Title bar sits above the client buffer.
        if self.chrome_active() && self.chrome.is_visible() {
            let slide = Point::from((0., self.chrome_reveal_offset()));
            self.chrome.render(
                ctx.r(),
                location + slide,
                tile_xray.offset(slide),
                self.chrome_hover_ball,
                &mut |elem| push(elem.into()),
            );
        }

        // If we're resizing, try to render a shader, or a fallback.
        let mut pushed_resize = false;
        if let Some(resize) = &self.resize_animation {
            if ResizeRenderElement::has_shader(ctx.renderer) {
                let mut ctx = ctx.as_gles();

                if let Some(texture_from) = resize.snapshot.texture(ctx.r(), scale) {
                    let mut window_elements = Vec::new();
                    self.window.render_normal(
                        ctx.r(),
                        Point::from((0., 0.)),
                        scale,
                        1.,
                        &mut |elem| window_elements.push(elem),
                    );

                    let current = resize
                        .offscreen
                        .render(ctx.renderer, scale, &window_elements)
                        .map_err(|err| warn!("error rendering window to texture: {err:?}"))
                        .ok();

                    // Clip blocked-out resizes unconditionally because they use solid color render
                    // elements.
                    let clip_to_geometry =
                        if ctx.target.should_block_out(resize.snapshot.block_out_from)
                            && ctx.target.should_block_out(rules.block_out_from)
                        {
                            true
                        } else {
                            clip_to_geometry
                        };

                    if let Some((elem_current, _sync_point, mut data)) = current {
                        let texture_current = elem_current.texture().clone();
                        // The offset and size are computed in physical pixels and converted to
                        // logical with the same `scale`, so converting them back with rounding
                        // inside the geometry() call gives us the same physical result back.
                        let texture_current_geo = elem_current.geometry(scale);

                        let elem = ResizeRenderElement::new(
                            area,
                            scale,
                            texture_from.clone(),
                            resize.snapshot.size,
                            (texture_current, texture_current_geo),
                            window_size,
                            resize.anim.value() as f32,
                            resize.anim.clamped_value().clamp(0., 1.) as f32,
                            radius,
                            clip_to_geometry,
                            win_alpha,
                        );

                        // We're drawing the resize shader, not the offscreen directly.
                        data.id = elem.id().clone();

                        // This is not a problem for split popups as the code will look for them by
                        // original id when it doesn't find them on the offscreen.
                        self.window.set_offscreen_data(Some(data));
                        push(elem.into());
                        pushed_resize = true;
                    }
                }
            }

            if !pushed_resize {
                let fallback_buffer = SolidColorBuffer::new(area.size, [1., 0., 0., 1.]);
                let elem = SolidColorRenderElement::from_buffer(
                    &fallback_buffer,
                    area.loc,
                    win_alpha,
                    Kind::Unspecified,
                );
                push(elem.into());
                pushed_resize = true;
            }
        }

        // If we're not resizing, render the window itself.
        let has_border_shader = BorderRenderElement::has_shader(ctx.renderer);
        if !pushed_resize {
            let geo = Rectangle::new(window_render_loc, window_size);
            let radius = radius.fit_to(window_size.w as f32, window_size.h as f32);

            let clip_shader = ClippedSurfaceRenderElement::shader(ctx.renderer).cloned();
            let clip = |elem| match elem {
                LayoutElementRenderElement::Wayland(elem) => {
                    // If we should clip to geometry, render a clipped window.
                    if clip_to_geometry {
                        if let Some(shader) = clip_shader.clone() {
                            if ClippedSurfaceRenderElement::will_clip(&elem, scale, geo, radius) {
                                let clipped = ClippedSurfaceRenderElement::new(
                                    elem,
                                    scale,
                                    geo,
                                    shader.clone(),
                                    radius,
                                );
                                return clipped.into();
                            }
                        }
                    }

                    // Otherwise, render it normally.
                    LayoutElementRenderElement::Wayland(elem).into()
                }
                LayoutElementRenderElement::SolidColor(elem) => {
                    // In this branch we're rendering a blocked-out window with a solid
                    // color. We need to render it with a rounded corner shader even if
                    // clip_to_geometry is false, because in this case we're assuming that
                    // the unclipped window CSD already has corners rounded to the
                    // user-provided radius, so our blocked-out rendering should match that
                    // radius.
                    if radius != CornerRadius::default() && has_border_shader {
                        return BorderRenderElement::new(
                            geo.size,
                            Rectangle::from_size(geo.size),
                            GradientInterpolation::default(),
                            Color::from_color32f(elem.color()),
                            Color::from_color32f(elem.color()),
                            0.,
                            Rectangle::from_size(geo.size),
                            0.,
                            radius,
                            scale.x as f32,
                            1.,
                        )
                        .with_location(geo.loc)
                        .into();
                    }

                    // Otherwise, render the solid color as is.
                    LayoutElementRenderElement::SolidColor(elem).into()
                }
                elem @ LayoutElementRenderElement::BackgroundEffect(_) => {
                    // This is only used on popups for now. If subsurface blur is implemented, this
                    // will need to be handled somehow.
                    error!("background effect clipping is unimplemented");
                    elem.into()
                }
            };

            if clip_to_geometry && clip_shader.is_some() {
                let damage = self.rounded_corner_damage.render(geo);
                push(damage.into());
            }

            self.window
                .render_normal(ctx.r(), window_render_loc, scale, win_alpha, &mut |elem| {
                    push(clip(elem))
                });
        }

        if fullscreen_progress > 0. {
            let alpha = fullscreen_progress as f32;

            // During the un/fullscreen animation, render a border element in order to use the
            // animated corner radius.
            if fullscreen_progress < 1. && has_border_shader {
                let border_width = self.visual_border_width().unwrap_or(0.);
                let radius = self
                    .window
                    .geometry_corner_radius()
                    .expanded_by(border_width as f32)
                    .scaled_by(1. - expanded_progress as f32);

                let size = self.fullscreen_backdrop.size();
                let color = self.fullscreen_backdrop.color();
                let elem = BorderRenderElement::new(
                    size,
                    Rectangle::from_size(size),
                    GradientInterpolation::default(),
                    Color::from_color32f(color),
                    Color::from_color32f(color),
                    0.,
                    Rectangle::from_size(size),
                    0.,
                    radius,
                    scale.x as f32,
                    alpha,
                )
                .with_location(location);
                push(elem.into());
            } else {
                let elem = SolidColorRenderElement::from_buffer(
                    &self.fullscreen_backdrop,
                    location,
                    alpha,
                    Kind::Unspecified,
                );
                push(elem.into());
            }
        }

        // Border wraps the whole tile (title bar + client), so origin is the tile loc.
        let frame_loc = location;

        if let Some(width) = self.visual_border_width() {
            self.border.render(
                ctx.renderer,
                frame_loc + Point::from((width, width)),
                &mut |elem| push(elem.into()),
            );
        }

        // Hide the focus ring when maximized/fullscreened. It's not normally visible anyway due to
        // being outside the monitor or obscured by a solid colored bar, but it is visible under
        // semitransparent bars in maximized state (which is a bit weird) and in the overview (also
        // a bit weird).
        if focus_ring && expanded_progress < 1. {
            self.focus_ring
                .render(ctx.renderer, frame_loc, &mut |elem| push(elem.into()));
        }

        if expanded_progress < 1. {
            self.shadow
                .render(ctx.renderer, frame_loc, &mut |elem| push(elem.into()));
        }

        match self.glass.tint() {
            Some(_) if ctx.target.should_block_out(rules.block_out_from) => (),
            Some(alpha) => {
                let alpha = alpha * (1. - fullscreen_progress);
                if let Some(elem) = self.tint.render(
                    ctx.as_gles(),
                    &self.window,
                    alpha,
                    area,
                    window_size,
                    radius,
                    self.scale,
                    self.clock.now_unadjusted(),
                ) {
                    push(elem.into());
                }
            }
            None => self.tint.reset(),
        }

        let surface_anim_scale = animated_window_size / window_size;
        let (effect, effect_alpha) = self.glass.effect(rules.background_effect);
        self.window.render_background_effect(
            ctx.as_gles(),
            area,
            self.scale,
            clip_to_geometry,
            surface_anim_scale,
            radius,
            effect,
            effect_alpha,
            xray_pos,
            &mut |elem| push(elem.into()),
        );
    }

    /// Sets the bottom clip line used by the next [`Tile::render`] call.
    ///
    /// `clip_y` is in the same coordinate space as the location passed to `render`. A tile that
    /// crosses the line fades out over up to [`BOTTOM_FADE_BAND`] above it and vanishes below.
    pub fn set_bottom_clip(&self, clip_y: Option<f64>) {
        self.bottom_clip.set(clip_y);
    }

    /// Sets the top clip line used by the next [`Tile::render`] call (same space as `render`).
    pub fn set_top_clip(&self, clip_y: Option<f64>) {
        self.top_clip.set(clip_y);
    }

    pub fn bottom_clip(&self) -> Option<f64> {
        self.bottom_clip.get()
    }

    pub fn top_clip(&self) -> Option<f64> {
        self.top_clip.get()
    }

    pub fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(TileRenderElement<R>),
    ) {
        let _span = tracy_client::span!("Tile::render");

        let clip_bottom = self.bottom_clip.get();
        let clip_top = self.top_clip.get();
        let tile_h = self.animated_tile_size().h;
        let tile_bottom = location.y + tile_h;
        let bottom_band = clip_bottom.map_or(0., |clip_y| bottom_fade_band(tile_bottom, clip_y));
        let top_band = clip_top.map_or(0., |clip_y| top_fade_band(location.y, clip_y));
        let crosses_bottom =
            clip_bottom.is_some_and(|c| crosses_bottom_clip(location.y, tile_bottom, c));
        let crosses_top = clip_top.is_some_and(|c| crosses_top_clip(location.y, tile_bottom, c));

        if clip_bottom.is_some_and(|c| location.y >= c)
            || clip_top.is_some_and(|c| location.y + tile_h <= c)
        {
            self.window().set_offscreen_data(None);
            self.bottom_fade_offscreen.replace(None);
            return;
        }

        // Soft fade when band > 0; hard cut (band 0) still needs the fade shader path.
        let need_fade = crosses_bottom || crosses_top;
        if need_fade {
            if !self.render_edge_faded(
                ctx.r(),
                location,
                xray_pos,
                focus_ring,
                clip_bottom
                    .filter(|_| crosses_bottom)
                    .map(|c| (c, bottom_band)),
                clip_top.filter(|_| crosses_top).map(|c| (c, top_band)),
                push,
            ) {
                self.render_unclipped(ctx, location, xray_pos, focus_ring, push);
            }
        } else {
            self.bottom_fade_offscreen.replace(None);
            self.render_unclipped(ctx, location, xray_pos, focus_ring, push);
        }
    }

    /// Renders the tile into an offscreen and pushes it with vertical alpha ramps.
    #[allow(clippy::too_many_arguments)]
    fn render_edge_faded<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        focus_ring: bool,
        bottom: Option<(f64, f64)>,
        top: Option<(f64, f64)>,
        push: &mut dyn FnMut(TileRenderElement<R>),
    ) -> bool {
        let mut ctx = ctx.as_gles();
        let Some(program) = BottomFadeRenderElement::shader(ctx.renderer) else {
            return false;
        };

        let scale = Scale::from(self.scale);
        let origin = Point::new(0., 0.);
        let mut elements = Vec::new();

        // Fold a plain alpha animation (and the sunk dim) into the fade element instead of
        // nesting two offscreens.
        let sunk_alpha = self.sunk_alpha();
        let tile_alpha = match &self.alpha_animation {
            Some(alpha) if self.open_animation.is_none() => {
                self.window().set_offscreen_data(None);
                self.render_inner(ctx.r(), origin, xray_pos, focus_ring, &mut |elem| {
                    elements.push(elem)
                });
                alpha.anim.clamped_value() as f32 * sunk_alpha
            }
            None if self.open_animation.is_none() && sunk_alpha < 0.999 => {
                self.window().set_offscreen_data(None);
                self.render_inner(ctx.r(), origin, xray_pos, focus_ring, &mut |elem| {
                    elements.push(elem)
                });
                sunk_alpha
            }
            _ => {
                self.render_unclipped(ctx.r(), origin, xray_pos, focus_ring, &mut |elem| {
                    elements.push(elem)
                });
                1.
            }
        };

        let mut buffer = self.bottom_fade_offscreen.borrow_mut();
        let buffer = buffer.get_or_insert_with(OffscreenBuffer::default);
        match buffer.render(ctx.renderer, scale, &elements) {
            Ok((elem, _sync, data)) => {
                let offset = elem.offset();
                let elem = elem.with_alpha(tile_alpha).with_offset(location + offset);
                let elem =
                    BottomFadeRenderElement::new_edges(elem, program, bottom, top, self.scale);

                self.window().set_offscreen_data(Some(data));
                push(elem.into());
                true
            }
            Err(err) => {
                warn!("error rendering tile to offscreen for edge fade: {err:?}");
                false
            }
        }
    }

    fn render_unclipped<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        location: Point<f64, Logical>,
        xray_pos: XrayPos,
        focus_ring: bool,
        push: &mut dyn FnMut(TileRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);

        let sunk_alpha = self.sunk_alpha();
        let tile_alpha = self
            .alpha_animation
            .as_ref()
            .map_or(1., |alpha| alpha.anim.clamped_value()) as f32
            * sunk_alpha;

        let mut pushed = false;
        self.window().set_offscreen_data(None);

        if let Some(open) = &self.open_animation {
            let mut ctx = ctx.as_gles();
            let mut elements = Vec::new();
            self.render_inner(
                ctx.r(),
                Point::new(0., 0.),
                xray_pos,
                focus_ring,
                &mut |elem| elements.push(elem),
            );
            match open.render(
                ctx.renderer,
                &elements,
                self.animated_tile_size(),
                location,
                scale,
                tile_alpha,
            ) {
                Ok((elem, data)) => {
                    self.window().set_offscreen_data(Some(data));
                    push(elem.into());
                    pushed = true;
                }
                Err(err) => {
                    warn!("error rendering window opening animation: {err:?}");
                }
            }
        } else if self.alpha_animation.is_some() || sunk_alpha < 0.999 {
            let mut ctx = ctx.as_gles();
            let mut elements = Vec::new();
            self.render_inner(
                ctx.r(),
                Point::new(0., 0.),
                xray_pos,
                focus_ring,
                &mut |elem| elements.push(elem),
            );
            let mut sunk_buffer = self.sunk_offscreen.borrow_mut();
            let offscreen: &OffscreenBuffer = match &self.alpha_animation {
                Some(alpha) => &alpha.offscreen,
                None => sunk_buffer.get_or_insert_with(OffscreenBuffer::default),
            };
            match offscreen.render(ctx.renderer, scale, &elements) {
                Ok((elem, _sync, data)) => {
                    let offset = elem.offset();
                    let elem = elem.with_alpha(tile_alpha).with_offset(location + offset);

                    self.window().set_offscreen_data(Some(data));
                    push(elem.into());
                    pushed = true;
                }
                Err(err) => {
                    warn!("error rendering tile to offscreen for alpha animation: {err:?}");
                }
            }
        }

        if !pushed {
            self.render_inner(ctx, location, xray_pos, focus_ring, &mut |elem| push(elem));
        }
    }

    pub fn store_unmap_snapshot_if_empty(
        &mut self,
        renderer: &mut GlesRenderer,
        xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
    ) {
        if self.unmap_snapshot.is_some() {
            return;
        }

        self.unmap_snapshot =
            Some(self.render_snapshot(renderer, xray, xray_has_blocked_out_layers, xray_pos));
    }

    fn render_snapshot(
        &self,
        renderer: &mut GlesRenderer,
        mut xray: Option<&mut Xray>,
        xray_has_blocked_out_layers: bool,
        xray_pos: XrayPos,
    ) -> TileRenderSnapshot {
        let _span = tracy_client::span!("Tile::render_snapshot");

        // Snapshots render at the origin, where the bottom clip line has no meaning.
        let bottom_clip = self.bottom_clip.take();
        let top_clip = self.top_clip.take();

        let mut contents = Vec::new();
        self.render(
            RenderCtx {
                target: RenderTarget::Output,
                renderer,
                xray: xray.as_deref(),
            },
            Point::from((0., 0.)),
            xray_pos,
            false,
            &mut |elem| contents.push(elem),
        );

        let mut contents_with_blocked_out_bg = None;

        // Do a bit of pointer surgery on Xray.
        //
        // The idea is to avoid the combinatorial combination of rendering snapshots for target
        // (Output, Screencast) × Xray target (Output, Screencast, ScreenCapture).
        //
        // Our main goals:
        // - Everything must look unblocked for RenderTarget::Output.
        // - If anything is potentially blocked-out, it must not show up on any screen capture.
        //
        // Right above we rendered a fully-unblocked snapshot for the Output, so that's covered.
        //
        // Next, *only if Xray has any blocked-out surfaces* (which is a rare case), we will render
        // a snapshot where the window itself is unblocked, but the Xray background is blocked. To
        // do this, we swap the Output target buffers in Xray with the Screencast target buffers
        // (which were prepared for us higher up the stack).
        //
        // Finally, we render a fully blocked-out snapshot. If Xray has blocked-out surfaces, then
        // Xray's Screencast buffers are already filled-in, but if not, then we swap in the Output
        // buffers, to avoid an extra render. This is safe since we know there are no blocked
        // surfaces there.
        let output_idx = RenderTarget::Output as usize;
        let screencast_idx = RenderTarget::Screencast as usize;
        let mut screencast_background = None;
        let mut screencast_backdrop = None;
        let mut output_background = None;
        let mut output_backdrop = None;
        if let Some(xray) = &mut xray {
            screencast_background = Some(Rc::clone(&xray.background[screencast_idx]));
            screencast_backdrop = Some(Rc::clone(&xray.backdrop[screencast_idx]));
            output_background = Some(Rc::clone(&xray.background[output_idx]));
            output_backdrop = Some(Rc::clone(&xray.backdrop[output_idx]));

            if xray_has_blocked_out_layers {
                xray.background[output_idx] = screencast_background.clone().unwrap();
                xray.backdrop[output_idx] = screencast_backdrop.clone().unwrap();

                let mut contents = Vec::new();
                self.render(
                    RenderCtx {
                        target: RenderTarget::Output,
                        renderer,
                        xray: Some(xray),
                    },
                    Point::from((0., 0.)),
                    xray_pos,
                    false,
                    &mut |elem| contents.push(elem),
                );
                contents_with_blocked_out_bg = Some(contents);
            } else {
                xray.background[screencast_idx] = output_background.clone().unwrap();
                xray.backdrop[screencast_idx] = output_backdrop.clone().unwrap();
            }
        }

        // A bit of a hack to render blocked out as for screencast, but I think it's fine here.
        let mut blocked_out_contents = Vec::new();
        self.render(
            RenderCtx {
                target: RenderTarget::Screencast,
                renderer,
                xray: xray.as_deref(),
            },
            Point::from((0., 0.)),
            xray_pos,
            false,
            &mut |elem| blocked_out_contents.push(elem),
        );

        // Put everything back to normal.
        if let Some(xray) = &mut xray {
            if xray_has_blocked_out_layers {
                xray.background[output_idx] = output_background.take().unwrap();
                xray.backdrop[output_idx] = output_backdrop.take().unwrap();
            } else {
                xray.background[screencast_idx] = screencast_background.take().unwrap();
                xray.backdrop[screencast_idx] = screencast_backdrop.take().unwrap();
            }
        }

        self.bottom_clip.set(bottom_clip);
        self.top_clip.set(top_clip);

        RenderSnapshot {
            contents,
            contents_with_blocked_out_bg,
            blocked_out_contents,
            block_out_from: self.window.rules().block_out_from,
            size: self.animated_tile_size(),
            texture: Default::default(),
            texture_with_blocked_out_bg: Default::default(),
            blocked_out_texture: Default::default(),
        }
    }

    pub fn take_unmap_snapshot(&mut self) -> Option<TileRenderSnapshot> {
        self.unmap_snapshot.take()
    }

    pub fn border(&self) -> &FocusRing {
        &self.border
    }

    pub fn focus_ring(&self) -> &FocusRing {
        &self.focus_ring
    }

    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }

    #[cfg(test)]
    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    #[cfg(test)]
    pub fn verify_invariants(&self) {
        use approx::assert_abs_diff_eq;

        assert_eq!(self.sizing_mode, self.window.sizing_mode());

        let scale = self.scale;
        let size = self.tile_size();
        let rounded = size.to_physical_precise_round(scale).to_logical(scale);
        assert_abs_diff_eq!(size.w, rounded.w, epsilon = 1e-5);
        assert_abs_diff_eq!(size.h, rounded.h, epsilon = 1e-5);
    }
}

/// Height of the band above the bottom clip line over which crossing windows fade out.
///
/// `0` = hard cut at the clip line (no soft gradient). Positive values soften the edge.
pub const BOTTOM_FADE_BAND: f64 = 0.;

/// Bottom clip line for floating/dragged windows: top of the bottom bar band.
///
/// This is the working-area bottom (after exclusive zone + struts) — i.e. the bar top —
/// **not** minus layout gaps. Tiled windows already keep `gaps` above this line via layout;
/// floating windows that cross it are hard-cut here so they meet the bar edge.
pub fn bottom_rail_y(working_area: Rectangle<f64, Logical>, _gaps: f64) -> f64 {
    working_area.loc.y + working_area.size.h
}

/// Physical crop covering everything above [`bottom_rail_y`].
pub fn bottom_rail_clip_bounds(
    working_area: Rectangle<f64, Logical>,
    gaps: f64,
    scale: impl Into<Scale<f64>>,
) -> Rectangle<i32, Physical> {
    bottom_clip_bounds_at(bottom_rail_y(working_area, gaps), scale)
}

/// Physical crop covering everything above the logical `y`.
///
/// Horizontal extent is effectively infinite so `CropRenderElement` does not
/// cut pixel shaders (same workaround as `Monitor::render_workspaces`).
pub fn bottom_clip_bounds_at(y: f64, scale: impl Into<Scale<f64>>) -> Rectangle<i32, Physical> {
    let scale = scale.into();
    let bottom = Point::<f64, Logical>::from((0., y))
        .to_physical_precise_round::<_, i32>(scale)
        .y
        .max(0);
    Rectangle::new(
        Point::from((-i32::MAX / 2, -i32::MAX / 2)),
        Size::from((i32::MAX, bottom.saturating_add(i32::MAX / 2))),
    )
}

/// Fade band for a tile whose visual bottom is at `tile_bottom`.
///
/// Grows with the overshoot past `clip_y` up to [`BOTTOM_FADE_BAND`], so a window that just
/// touches the line (like a normal tiled window) does not fade, and crossing it does not pop.
/// When [`BOTTOM_FADE_BAND`] is `0`, the band stays `0` (hard cut via the fade shader).
pub fn bottom_fade_band(tile_bottom: f64, clip_y: f64) -> f64 {
    (tile_bottom - clip_y).clamp(0., BOTTOM_FADE_BAND)
}

/// Fade band for content whose visual top is at `tile_top`, crossing a top clip line upward.
pub fn top_fade_band(tile_top: f64, clip_y: f64) -> f64 {
    (clip_y - tile_top).clamp(0., BOTTOM_FADE_BAND)
}

/// Whether a rect crosses the bottom clip line (needs hard cut or soft fade).
pub fn crosses_bottom_clip(top: f64, bottom: f64, clip_y: f64) -> bool {
    top < clip_y && bottom > clip_y + 0.5
}

/// Whether a rect crosses the top clip line.
pub fn crosses_top_clip(top: f64, bottom: f64, clip_y: f64) -> bool {
    top < clip_y - 0.5 && bottom > clip_y
}

/// Alpha at logical `y` for a fade of height `band` ending at `clip_y`.
pub fn bottom_fade_alpha(y: f64, clip_y: f64, band: f64) -> f64 {
    if band <= 0. {
        return if y < clip_y { 1. } else { 0. };
    }
    ((clip_y - y) / band).clamp(0., 1.)
}

/// Physical crop covering everything between `top_y` and `bottom_y` (logical).
pub fn vertical_clip_bounds_at(
    top_y: f64,
    bottom_y: f64,
    scale: impl Into<Scale<f64>>,
) -> Rectangle<i32, Physical> {
    let scale = scale.into();
    let top = Point::<f64, Logical>::from((0., top_y))
        .to_physical_precise_round::<_, i32>(scale)
        .y;
    let bottom = Point::<f64, Logical>::from((0., bottom_y))
        .to_physical_precise_round::<_, i32>(scale)
        .y
        .max(top);
    Rectangle::new(
        Point::from((-i32::MAX / 2, top)),
        Size::from((i32::MAX, bottom.saturating_sub(top).max(0))),
    )
}

#[cfg(test)]
mod bottom_rail_clip {
    use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

    use super::{
        bottom_fade_alpha, bottom_fade_band, bottom_rail_clip_bounds, bottom_rail_y, top_fade_band,
        BOTTOM_FADE_BAND,
    };

    const GAPS: f64 = 12.;

    #[test]
    fn clip_line_is_bar_top() {
        let wa = Rectangle::new(Point::from((12., 0.)), Size::from((1280., 672.)));
        // Clip at working-area bottom (bar top), not gaps above it.
        assert_eq!(bottom_rail_y(wa, GAPS), 672.);
        let clip = bottom_rail_clip_bounds(wa, GAPS, 1.);
        assert_eq!(clip.loc.y.saturating_add(clip.size.h), 672);
        assert!(clip.size.w > 10_000, "horizontal crop must stay wide");
        assert!(clip.loc.x < 0);

        // Tiled windows still end gaps above the clip line.
        let tiled_bottom = wa.loc.y + GAPS + (wa.size.h - 2. * GAPS);
        assert_eq!(tiled_bottom, 660.);
        assert!(tiled_bottom < bottom_rail_y(wa, GAPS));
    }

    #[test]
    fn clip_rect_respects_scale() {
        let wa = Rectangle::new(Point::from((0., 0.)), Size::from((1280., 672.)));
        let clip = bottom_rail_clip_bounds(wa, GAPS, 2.);
        assert_eq!(clip.loc.y.saturating_add(clip.size.h), 1344);
    }

    #[test]
    fn window_overlapping_rail_is_cut_not_moved() {
        let wa: Rectangle<f64, Logical> =
            Rectangle::new(Point::from((0., 0.)), Size::from((1280., 672.)));
        let clip = bottom_rail_clip_bounds(wa, GAPS, 1.);
        // Clip line is wa.bottom (672), not gaps above it.
        assert_eq!(clip.loc.y.saturating_add(clip.size.h), 672);

        let below = Rectangle::<i32, Physical>::new(Point::from((100, 672)), Size::from((200, 60)));
        assert!(
            below.intersection(clip).is_none(),
            "pixels in the bar band must be fully clipped"
        );

        let overlapping =
            Rectangle::<i32, Physical>::new(Point::from((100, 600)), Size::from((200, 120)));
        let vis = overlapping
            .intersection(clip)
            .expect("visible part remains");
        assert_eq!(vis.loc.x, 100, "clip must not shift X");
        assert_eq!(vis.loc.y, 600, "clip must not move the window up");
        assert_eq!(vis.size.h, 72, "only the overlap below the bar top is cut");
    }

    #[test]
    fn fade_alpha_ramps_over_band() {
        let clip_y = 660.;
        // Soft-fade behaviour with an explicit band (independent of BOTTOM_FADE_BAND).
        let band = 36.;
        assert_eq!(bottom_fade_alpha(0., clip_y, band), 1.);
        assert_eq!(bottom_fade_alpha(clip_y - band, clip_y, band), 1.);
        assert_eq!(bottom_fade_alpha(clip_y - band / 2., clip_y, band), 0.5);
        assert_eq!(bottom_fade_alpha(clip_y, clip_y, band), 0.);
        assert_eq!(bottom_fade_alpha(clip_y + 10., clip_y, band), 0.);
        // Hard cut when band is 0.
        assert_eq!(bottom_fade_alpha(clip_y - 1., clip_y, 0.), 1.);
        assert_eq!(bottom_fade_alpha(clip_y, clip_y, 0.), 0.);
    }

    #[test]
    fn fade_band_grows_with_overshoot() {
        let clip_y = 660.;
        assert_eq!(bottom_fade_band(clip_y - 20., clip_y), 0.);
        assert_eq!(
            bottom_fade_band(clip_y, clip_y),
            0.,
            "touching the line must not fade"
        );
        // With BOTTOM_FADE_BAND == 0 (hard cut), overshoot never grows a soft band.
        assert_eq!(bottom_fade_band(clip_y + 10., clip_y), BOTTOM_FADE_BAND);
        assert_eq!(bottom_fade_band(clip_y + 500., clip_y), BOTTOM_FADE_BAND);
    }

    #[test]
    fn top_fade_band_grows_with_overshoot() {
        let clip_y = 24.;
        assert_eq!(top_fade_band(clip_y + 20., clip_y), 0.);
        assert_eq!(top_fade_band(clip_y, clip_y), 0.);
        assert_eq!(top_fade_band(clip_y - 10., clip_y), BOTTOM_FADE_BAND);
        assert_eq!(top_fade_band(clip_y - 500., clip_y), BOTTOM_FADE_BAND);
    }
}
