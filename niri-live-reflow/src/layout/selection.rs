//! Rubber-band selection on empty desktop and its "Criar grupo" button.

use std::cell::RefCell;

use pangocairo::cairo::{self, ImageSurface};
use pangocairo::pango::FontDescription;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::utils::{Logical, Point, Rectangle, Size};

use super::window_chrome::texture_from_surface;
use crate::niri_render_elements;
use crate::render_helpers::primary_gpu_texture::PrimaryGpuTextureRenderElement;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::solid_color::{SolidColorBuffer, SolidColorRenderElement};
use crate::render_helpers::texture::{TextureBuffer, TextureRenderElement};

type Rect = Rectangle<f64, Logical>;

/// Size of the "Criar grupo" button.
pub const BUTTON_W: f64 = 124.;
pub const BUTTON_H: f64 = 32.;
/// Offset of the button from the pointer.
const BUTTON_OFFSET: f64 = 14.;
const BORDER_W: f64 = 2.;
const HIGHLIGHT_W: f64 = 3.;
const FILL: [f32; 4] = [0.36, 0.62, 1.0, 0.16];
const BORDER: [f32; 4] = [0.45, 0.70, 1.0, 0.95];
const HIGHLIGHT: [f32; 4] = [0.45, 0.70, 1.0, 0.85];

niri_render_elements! {
    SelectionRenderElement => {
        SolidColor = SolidColorRenderElement,
        Texture = PrimaryGpuTextureRenderElement,
    }
}

/// Normalized rect spanned by two corner points.
pub fn selection_rect(a: Point<f64, Logical>, b: Point<f64, Logical>) -> Rect {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = a.x.max(b.x);
    let y1 = a.y.max(b.y);
    Rectangle::new(Point::from((x0, y0)), Size::from((x1 - x0, y1 - y0)))
}

/// Whether `a` and `b` touch (share any area or edge).
pub fn rects_touch(a: Rect, b: Rect) -> bool {
    a.loc.x <= b.loc.x + b.size.w
        && b.loc.x <= a.loc.x + a.size.w
        && a.loc.y <= b.loc.y + b.size.h
        && b.loc.y <= a.loc.y + a.size.h
}

/// Button rect next to `cursor`, kept inside `view`.
pub fn button_rect(cursor: Point<f64, Logical>, view: Size<f64, Logical>) -> Rect {
    let size = Size::from((BUTTON_W, BUTTON_H));
    let mut x = cursor.x + BUTTON_OFFSET;
    let mut y = cursor.y + BUTTON_OFFSET;
    if x + size.w > view.w {
        x = cursor.x - BUTTON_OFFSET - size.w;
    }
    if y + size.h > view.h {
        y = cursor.y - BUTTON_OFFSET - size.h;
    }
    Rectangle::new(Point::from((x.max(0.), y.max(0.))), size)
}

/// What the overlay draws, in workspace view coordinates.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SelectionView {
    pub rect: Option<Rect>,
    pub highlights: Vec<Rect>,
    pub button: Option<(Rect, bool)>,
}

#[derive(Debug, Default)]
pub struct SelectionOverlay {
    view: SelectionView,
    scale: f64,
    buffers: Vec<(SolidColorBuffer, Point<f64, Logical>)>,
    button_textures: RefCell<Option<(f64, [TextureBuffer<GlesTexture>; 2])>>,
}

impl SelectionOverlay {
    pub fn update(&mut self, view: SelectionView, scale: f64) {
        if self.view == view && self.scale == scale {
            return;
        }
        self.view = view;
        self.scale = scale;

        let mut rects: Vec<(Rect, [f32; 4])> = Vec::new();
        let outline = |r: Rect, w: f64, color: [f32; 4], out: &mut Vec<(Rect, [f32; 4])>| {
            let Rectangle { loc, size } = r;
            let rect = |x: f64, y: f64, w: f64, h: f64| {
                Rectangle::new(Point::from((x, y)), Size::from((w.max(0.), h.max(0.))))
            };
            out.push((rect(loc.x, loc.y, size.w, w), color));
            out.push((rect(loc.x, loc.y + size.h - w, size.w, w), color));
            out.push((rect(loc.x, loc.y + w, w, size.h - w * 2.), color));
            out.push((
                rect(loc.x + size.w - w, loc.y + w, w, size.h - w * 2.),
                color,
            ));
        };
        for r in &self.view.highlights {
            outline(*r, HIGHLIGHT_W, HIGHLIGHT, &mut rects);
        }
        if let Some(r) = self.view.rect {
            outline(r, BORDER_W, BORDER, &mut rects);
            rects.push((r, FILL));
        }

        self.buffers.truncate(rects.len());
        for (i, (r, color)) in rects.into_iter().enumerate() {
            if let Some((buf, loc)) = self.buffers.get_mut(i) {
                buf.resize(r.size);
                buf.set_color(color);
                *loc = r.loc;
            } else {
                self.buffers
                    .push((SolidColorBuffer::new(r.size, color), r.loc));
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.view == SelectionView::default()
    }

    /// First pushed is drawn on top.
    pub fn render<R: NiriRenderer>(
        &self,
        renderer: &mut R,
        push: &mut dyn FnMut(SelectionRenderElement),
    ) {
        if let Some((rect, hovered)) = self.view.button {
            if let Some(textures) = self.button_textures(renderer) {
                let buffer = textures[usize::from(hovered)].clone();
                let elem = TextureRenderElement::from_texture_buffer(
                    buffer,
                    rect.loc,
                    1.,
                    None,
                    Some(rect.size),
                    Kind::Unspecified,
                );
                push(PrimaryGpuTextureRenderElement(elem).into());
            }
        }
        for (buf, loc) in &self.buffers {
            let elem = SolidColorRenderElement::from_buffer(buf, *loc, 1., Kind::Unspecified);
            push(elem.into());
        }
    }

    fn button_textures<R: NiriRenderer>(
        &self,
        renderer: &mut R,
    ) -> Option<[TextureBuffer<GlesTexture>; 2]> {
        let mut cache = self.button_textures.borrow_mut();
        if let Some((scale, textures)) = &*cache {
            if *scale == self.scale {
                return Some(textures.clone());
            }
        }
        let gles = renderer.as_gles_renderer();
        let scale = self.scale;
        let render = |gles: &mut GlesRenderer| -> anyhow::Result<[TextureBuffer<GlesTexture>; 2]> {
            Ok([
                render_button(gles, scale, false)?,
                render_button(gles, scale, true)?,
            ])
        };
        let textures = render(gles)
            .map_err(|err| warn!("error rendering selection button: {err:?}"))
            .ok()?;
        *cache = Some((scale, textures.clone()));
        Some(textures)
    }
}

fn render_button(
    renderer: &mut GlesRenderer,
    scale: f64,
    hovered: bool,
) -> anyhow::Result<TextureBuffer<GlesTexture>> {
    let (w, h) = (BUTTON_W, BUTTON_H);
    let px_w = (w * scale).round().max(1.) as i32;
    let px_h = (h * scale).round().max(1.) as i32;
    let surface = ImageSurface::create(cairo::Format::ARgb32, px_w, px_h)?;
    let cr = cairo::Context::new(&surface)?;
    cr.scale(f64::from(px_w) / w, f64::from(px_h) / h);

    let r = 8.;
    let deg = std::f64::consts::PI / 180.;
    cr.new_sub_path();
    cr.arc(w - r - 1., r + 1., r, -90. * deg, 0.);
    cr.arc(w - r - 1., h - r - 1., r, 0., 90. * deg);
    cr.arc(r + 1., h - r - 1., r, 90. * deg, 180. * deg);
    cr.arc(r + 1., r + 1., r, 180. * deg, 270. * deg);
    cr.close_path();
    if hovered {
        cr.set_source_rgba(0.30, 0.56, 0.98, 0.97);
    } else {
        cr.set_source_rgba(0.12, 0.14, 0.18, 0.94);
    }
    cr.fill_preserve()?;
    cr.set_source_rgba(0.45, 0.70, 1.0, 0.9);
    cr.set_line_width(1.5);
    cr.stroke()?;

    let font = FontDescription::from_string("sans SemiBold 13px");
    let layout = pangocairo::functions::create_layout(&cr);
    layout.set_font_description(Some(&font));
    layout.set_text("Criar grupo");
    let (tw, th) = layout.pixel_size();
    cr.move_to((w - f64::from(tw)) / 2., (h - f64::from(th)) / 2.);
    cr.set_source_rgba(1., 1., 1., 0.96);
    pangocairo::functions::show_layout(&cr, &layout);

    drop(cr);
    texture_from_surface(renderer, surface, px_w, px_h, scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_from_any_corners() {
        let r = selection_rect(Point::from((50., 80.)), Point::from((10., 20.)));
        assert_eq!(
            r,
            Rectangle::new(Point::from((10., 20.)), Size::from((40., 60.)))
        );
    }

    #[test]
    fn touching_counts() {
        let a = Rectangle::new(Point::from((0., 0.)), Size::from((10., 10.)));
        let b = Rectangle::new(Point::from((10., 0.)), Size::from((10., 10.)));
        let c = Rectangle::new(Point::from((11., 0.)), Size::from((10., 10.)));
        assert!(rects_touch(a, b));
        assert!(!rects_touch(a, c));
    }

    #[test]
    fn button_stays_in_view() {
        let view = Size::from((1000., 800.));
        let r = button_rect(Point::from((990., 790.)), view);
        assert!(r.loc.x + r.size.w <= view.w);
        assert!(r.loc.y + r.size.h <= view.h);
        let r = button_rect(Point::from((100., 100.)), view);
        assert_eq!(r.loc, Point::from((114., 114.)));
    }
}
