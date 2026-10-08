use smithay::{
    backend::renderer::{
        element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
        gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform},
        utils::{CommitCounter, DamageSet, OpaqueRegions},
        Texture as _,
    },
    utils::{user_data::UserDataMap, Buffer, Physical, Rectangle, Scale, Transform},
};

use super::offscreen::OffscreenRenderElement;
use crate::{
    backend::tty::{TtyFrame, TtyRenderer, TtyRendererError},
    render_helpers::{renderer::AsGlesFrame as _, shaders::Shaders},
};

/// Offscreened contents that fade out towards horizontal clip lines.
#[derive(Debug, Clone)]
pub struct BottomFadeRenderElement {
    inner: OffscreenRenderElement,
    program: BottomFadeShader,
    /// Bottom fade start and end in normalized texture coordinates.
    fade_y: (f32, f32),
    /// Top fade start and end in normalized texture coordinates.
    fade_top: (f32, f32),
}

#[derive(Debug, Clone)]
pub struct BottomFadeShader(GlesTexProgram);

/// Sentinel tex coords that disable the bottom fade (`x >= y` and end outside `[0, 1]`).
const FADE_BOTTOM_DISABLED: (f32, f32) = (2., 1.);
/// Sentinel tex coords that disable the top fade (`x >= y` and end below 0).
const FADE_TOP_DISABLED: (f32, f32) = (-1., -2.);

impl BottomFadeRenderElement {
    /// `clip_y` and `band` are logical, in the same space as `inner`'s offset.
    ///
    /// Alpha is 1 above `clip_y - band` and ramps down to 0 at `clip_y`.
    pub fn new(
        inner: OffscreenRenderElement,
        program: BottomFadeShader,
        clip_y: f64,
        band: f64,
        scale: f64,
    ) -> Self {
        Self::new_edges(inner, program, Some((clip_y, band)), None, scale)
    }

    /// Optional bottom and/or top fades. Each pair is `(clip_y, band)` in the same logical
    /// space as `inner`'s offset.
    pub fn new_edges(
        inner: OffscreenRenderElement,
        program: BottomFadeShader,
        bottom: Option<(f64, f64)>,
        top: Option<(f64, f64)>,
        scale: f64,
    ) -> Self {
        let tex_h = f64::from(inner.texture().size().h.max(1));
        let to_tex = |y: f64| (y - inner.offset().y) * scale / tex_h;

        let fade_y = bottom
            .map(|(clip_y, band)| {
                let end = to_tex(clip_y);
                let start = end - band.max(0.) * scale / tex_h;
                (start as f32, end as f32)
            })
            .unwrap_or(FADE_BOTTOM_DISABLED);

        let fade_top = top
            .map(|(clip_y, band)| {
                let start = to_tex(clip_y);
                let end = start + band.max(0.) * scale / tex_h;
                (start as f32, end as f32)
            })
            .unwrap_or(FADE_TOP_DISABLED);

        Self {
            inner,
            program,
            fade_y,
            fade_top,
        }
    }

    pub fn shader(renderer: &mut GlesRenderer) -> Option<BottomFadeShader> {
        let program = Shaders::get(renderer).bottom_fade.clone();
        program.map(BottomFadeShader)
    }
}

impl Element for BottomFadeRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<GlesRenderer> for BottomFadeRenderElement {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let uniforms = vec![
            Uniform::new("fade_y", self.fade_y),
            Uniform::new("fade_top", self.fade_top),
        ];
        frame.override_default_tex_program(self.program.0.clone(), uniforms);
        let res = RenderElement::<GlesRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
        frame.clear_tex_program_override();
        res
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

impl<'render> RenderElement<TtyRenderer<'render>> for BottomFadeRenderElement {
    fn draw(
        &self,
        frame: &mut TtyFrame<'render, '_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), TtyRendererError<'render>> {
        let gles_frame = frame.as_gles_frame();
        RenderElement::<GlesRenderer>::draw(
            &self,
            gles_frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )?;
        Ok(())
    }

    fn underlying_storage(
        &self,
        _renderer: &mut TtyRenderer<'render>,
    ) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
