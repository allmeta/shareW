//! Cairo rasterization, geometry, and the annotation data model.
//!
//! Everything works in device pixels. `scale` only sizes the UI chrome (toolbar, popups, text)
//! so it stays legible on HiDPI; annotation/selection coordinates are raw device pixels.

use std::f64::consts::{FRAC_PI_2, PI, TAU};

use cairo::{Context, Filter, FontSlant, FontWeight, ImageSurface, LineCap, LineJoin, Matrix};

// Annotation drawing.
pub const DEFAULT_THICKNESS: f64 = 4.;
pub const MIN_THICKNESS: f64 = 1.;
pub const MAX_THICKNESS: f64 = 100.;
pub const HIGHLIGHT_THICKNESS: f64 = 24.;
pub const HIGHLIGHT_ALPHA: f64 = 0.4;
pub const BLUR_THICKNESS: f64 = 24.;
pub const BLUR_BLOCK: i32 = 10;
pub const DEFAULT_COLOR: [f64; 4] = [0.9, 0.1, 0.1, 1.];
pub const HIGHLIGHT_COLOR: [f64; 4] = [1., 0.85, 0.1, 1.];

// Selection.
const SELECTION_BORDER: i32 = 2;
// Flameshot-style fade: the dimmed area is darkened by this much. With no region selected the whole
// screen is dimmed; once a region is selected only the area outside it stays dimmed.
const DIM: f64 = 0.4;

// Quick-access color swatches shown under the color wheel.
pub const PRESET_COLORS: [[f64; 4]; 8] = [
    [0.9, 0.1, 0.1, 1.],
    [0.95, 0.55, 0.1, 1.],
    [1., 0.85, 0.1, 1.],
    [0.2, 0.7, 0.2, 1.],
    [0.2, 0.5, 0.95, 1.],
    [0.6, 0.3, 0.85, 1.],
    [0.05, 0.05, 0.05, 1.],
    [0.95, 0.95, 0.95, 1.],
];
const PRESET_GAP: i32 = 4;
const PRESET_TOP_GAP: i32 = 10;

// Toolbar layout (logical px).
const TOOLBAR_BUTTON: i32 = 40;
const TOOLBAR_THICK_W: i32 = 54;
const TOOLBAR_GAP: i32 = 4;
const TOOLBAR_SEP: i32 = 18;
const TOOLBAR_PAD: i32 = 6;
const TOOLBAR_TOP: i32 = 8;
const TOOLBAR_RADIUS: i32 = 8;
const TOOLBAR_FONT_PX: f64 = 13.;

// Color wheel popup (logical px).
const WHEEL_RADIUS: i32 = 90;
const WHEEL_PAD: i32 = 12;

// Thickness slider popup (logical px).
const SLIDER_WIDTH: i32 = 170;
const SLIDER_PAD: i32 = 12;
const SLIDER_TRACK_H: i32 = 6;
const SLIDER_HANDLE_R: i32 = 8;
const SLIDER_TEXT_H: i32 = 22;
const SLIDER_FONT_PX: f64 = 13.;
const POPUP_GAP: i32 = 6;

pub const TOOLBAR_ITEMS: [ToolbarItem; 10] = [
    ToolbarItem::Tool(Tool::Crop),
    ToolbarItem::Clear,
    ToolbarItem::Tool(Tool::Rectangle),
    ToolbarItem::Tool(Tool::Line),
    ToolbarItem::Tool(Tool::Arrow),
    ToolbarItem::Tool(Tool::Freehand),
    ToolbarItem::Tool(Tool::Highlight),
    ToolbarItem::Tool(Tool::Blur),
    ToolbarItem::Color,
    ToolbarItem::Thickness,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Crop,
    Rectangle,
    Line,
    Arrow,
    Freehand,
    Highlight,
    Blur,
}

impl Tool {
    pub fn is_drawing(self) -> bool {
        !matches!(self, Tool::Crop)
    }
    pub fn is_highlight(self) -> bool {
        matches!(self, Tool::Highlight)
    }
    pub fn is_freehand(self) -> bool {
        matches!(self, Tool::Freehand | Tool::Highlight | Tool::Blur)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolbarItem {
    Tool(Tool),
    Clear,
    Color,
    Thickness,
}

#[derive(Debug, Clone)]
pub struct Annotation {
    pub tool: Tool,
    pub start: (i32, i32),
    pub end: (i32, i32),
    pub points: Vec<(i32, i32)>,
    pub color: [f64; 4],
    pub thickness: f64,
}

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }
}

fn rnd(scale: f64, v: i32) -> i32 {
    (v as f64 * scale).round() as i32
}

pub fn rect_from_corner_points(a: (i32, i32), b: (i32, i32)) -> Rect {
    let x1 = a.0.min(b.0);
    let y1 = a.1.min(b.1);
    let x2 = a.0.max(b.0);
    let y2 = a.1.max(b.1);
    // +1 because the corners are inclusive.
    Rect {
        x: x1,
        y: y1,
        w: x2 - x1 + 1,
        h: y2 - y1 + 1,
    }
}

pub fn hsv_to_rgb(h: f64, s: f64, v: f64) -> [f64; 3] {
    let h = ((h % 1.) + 1.) % 1. * 6.;
    let i = h.floor();
    let f = h - i;
    let p = v * (1. - s);
    let q = v * (1. - s * f);
    let t = v * (1. - s * (1. - f));
    let (r, g, b) = match i as i32 % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [r, g, b]
}

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// Per-button x offset (from the inner content origin) and width, plus button height, padding,
/// and total inner width. All physical px.
fn toolbar_button_offsets(scale: f64) -> (i32, i32, i32, Vec<(ToolbarItem, i32, i32)>) {
    let btn = rnd(scale, TOOLBAR_BUTTON);
    let thick_w = rnd(scale, TOOLBAR_THICK_W);
    let gap = rnd(scale, TOOLBAR_GAP);
    let sep = rnd(scale, TOOLBAR_SEP);
    let pad = rnd(scale, TOOLBAR_PAD);

    let item_w = |item: ToolbarItem| {
        if item == ToolbarItem::Thickness {
            thick_w
        } else {
            btn
        }
    };

    let mut offsets = Vec::with_capacity(TOOLBAR_ITEMS.len());
    let mut x = 0;
    for (i, item) in TOOLBAR_ITEMS.iter().enumerate() {
        if i > 0 {
            x += gap;
        }
        if i == 1 {
            x += sep;
        }
        let w = item_w(*item);
        offsets.push((*item, x, w));
        x += w;
    }
    (btn, pad, x, offsets)
}

/// Lays the toolbar out centered at the top of `view` (the active output, in canvas coords).
pub fn toolbar_layout(view: Rect, scale: f64) -> (Rect, Vec<(ToolbarItem, Rect)>) {
    let top = rnd(scale, TOOLBAR_TOP);
    let (btn, pad, inner_w, offsets) = toolbar_button_offsets(scale);

    let w = inner_w + pad * 2;
    let h = btn + pad * 2;
    let x = view.x + ((view.w - w) / 2).max(0);
    let y = view.y + top;

    let bounds = Rect { x, y, w, h };
    let by = y + pad;
    let buttons = offsets
        .into_iter()
        .map(|(item, ox, bw)| {
            (
                item,
                Rect {
                    x: x + pad + ox,
                    y: by,
                    w: bw,
                    h: btn,
                },
            )
        })
        .collect();
    (bounds, buttons)
}

fn toolbar_button_rect(view: Rect, scale: f64, item: ToolbarItem) -> Rect {
    let (_, buttons) = toolbar_layout(view, scale);
    buttons
        .into_iter()
        .find(|(it, _)| *it == item)
        .map(|(_, r)| r)
        .unwrap_or(Rect {
            x: 0,
            y: 0,
            w: 0,
            h: 0,
        })
}

/// Positions a popup of width `w` centered under `button`, clamped within `view`.
fn popup_position(view: Rect, scale: f64, button: Rect, w: i32) -> (i32, i32) {
    let gap = rnd(scale, POPUP_GAP);
    let min_x = view.x;
    let max_x = view.x + (view.w - w).max(0);
    let x = (button.x + button.w / 2 - w / 2).clamp(min_x, max_x);
    let y = button.y + button.h + gap;
    (x, y)
}

fn preset_swatch_metrics(scale: f64, radius: i32) -> (i32, i32) {
    let gap = rnd(scale, PRESET_GAP);
    let n = PRESET_COLORS.len() as i32;
    let inner = radius * 2;
    let sw = ((inner - gap * (n - 1)) / n).max(1);
    (sw, gap)
}

pub fn preset_swatch_rects(bounds: Rect, scale: f64) -> Vec<Rect> {
    let pad = rnd(scale, WHEEL_PAD);
    let radius = rnd(scale, WHEEL_RADIUS);
    let top_gap = rnd(scale, PRESET_TOP_GAP);
    let (sw, gap) = preset_swatch_metrics(scale, radius);
    let n = PRESET_COLORS.len() as i32;
    let row_w = sw * n + gap * (n - 1);
    let x0 = bounds.x + (bounds.w - row_w) / 2;
    let y = bounds.y + pad + radius * 2 + top_gap;
    (0..n)
        .map(|i| Rect {
            x: x0 + i * (sw + gap),
            y,
            w: sw,
            h: sw,
        })
        .collect()
}

/// Returns (bounds, center, radius).
pub fn color_wheel_layout(view: Rect, scale: f64) -> (Rect, (i32, i32), i32) {
    let radius = rnd(scale, WHEEL_RADIUS);
    let pad = rnd(scale, WHEEL_PAD);
    let top_gap = rnd(scale, PRESET_TOP_GAP);
    let (sw, _) = preset_swatch_metrics(scale, radius);

    let w = radius * 2 + pad * 2;
    let h = pad + radius * 2 + top_gap + sw + pad;
    let button = toolbar_button_rect(view, scale, ToolbarItem::Color);
    let (x, y) = popup_position(view, scale, button, w);

    let bounds = Rect { x, y, w, h };
    let center = (x + pad + radius, y + pad + radius);
    (bounds, center, radius)
}

pub fn wheel_color_at(point: (i32, i32), center: (i32, i32), radius: i32) -> Option<[f64; 3]> {
    let dx = (point.0 - center.0) as f64;
    let dy = (point.1 - center.1) as f64;
    let dist = (dx * dx + dy * dy).sqrt();
    if dist > radius as f64 {
        return None;
    }
    let hue = dy.atan2(dx) / TAU;
    let sat = (dist / radius as f64).clamp(0., 1.);
    Some(hsv_to_rgb(hue, sat, 1.))
}

/// Returns (bounds, track_x0, track_x1, track_cy).
pub fn thickness_slider_layout(view: Rect, scale: f64) -> (Rect, i32, i32, i32) {
    let pad = rnd(scale, SLIDER_PAD);
    let handle_r = rnd(scale, SLIDER_HANDLE_R);
    let text_h = rnd(scale, SLIDER_TEXT_H);
    let inner_w = rnd(scale, SLIDER_WIDTH);

    let w = inner_w + pad * 2;
    let h = pad * 2 + handle_r * 2 + text_h;
    let button = toolbar_button_rect(view, scale, ToolbarItem::Thickness);
    let (x, y) = popup_position(view, scale, button, w);

    let track_x0 = x + pad + handle_r;
    let track_x1 = x + w - pad - handle_r;
    let track_cy = y + pad + handle_r;
    (Rect { x, y, w, h }, track_x0, track_x1, track_cy)
}

pub fn thickness_from_slider_x(x: i32, track_x0: i32, track_x1: i32) -> f64 {
    let frac = ((x - track_x0) as f64 / (track_x1 - track_x0).max(1) as f64).clamp(0., 1.);
    MIN_THICKNESS + frac * (MAX_THICKNESS - MIN_THICKNESS)
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

fn rounded_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.).min(h / 2.);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.);
    cr.arc(x + w - r, y + h - r, r, 0., FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3. * FRAC_PI_2);
    cr.close_path();
}

pub fn build_blur_mosaic(src: &ImageSurface, w: i32, h: i32, block: i32) -> anyhow::Result<ImageSurface> {
    let small = ImageSurface::create(cairo::Format::ARgb32, (w / block).max(1), (h / block).max(1))?;
    let cr = Context::new(&small)?;
    cr.scale(1. / block as f64, 1. / block as f64);
    cr.set_source_surface(src, 0., 0.)?;
    cr.paint()?;
    drop(cr);
    Ok(small)
}

pub fn blur_block(scale: f64) -> i32 {
    rnd(scale, BLUR_BLOCK).max(1)
}

fn stroke_freehand(cr: &Context, a: &Annotation, lw: f64) -> anyhow::Result<()> {
    cr.set_line_cap(LineCap::Round);
    cr.set_line_join(LineJoin::Round);
    cr.set_line_width(lw);
    if a.points.len() < 2 {
        let p = a.points.first().copied().unwrap_or(a.start);
        cr.arc(p.0 as f64, p.1 as f64, lw / 2., 0., TAU);
        cr.fill()?;
    } else {
        let mut iter = a.points.iter();
        let first = iter.next().unwrap();
        cr.move_to(first.0 as f64, first.1 as f64);
        for p in iter {
            cr.line_to(p.0 as f64, p.1 as f64);
        }
        cr.stroke()?;
    }
    Ok(())
}

fn draw_annotation(
    cr: &Context,
    a: &Annotation,
    scale: f64,
    blur: Option<(&ImageSurface, i32)>,
) -> anyhow::Result<()> {
    let lw = a.thickness * scale;
    let [r, g, b, _] = a.color;

    cr.set_line_cap(LineCap::Round);
    cr.set_line_join(LineJoin::Round);
    cr.set_line_width(lw);

    match a.tool {
        Tool::Crop => {}
        Tool::Rectangle => {
            let rect = rect_from_corner_points(a.start, a.end);
            cr.set_line_join(LineJoin::Miter);
            cr.rectangle(rect.x as f64, rect.y as f64, rect.w as f64, rect.h as f64);
            cr.set_source_rgba(r, g, b, 1.);
            cr.stroke()?;
        }
        Tool::Line => {
            cr.move_to(a.start.0 as f64, a.start.1 as f64);
            cr.line_to(a.end.0 as f64, a.end.1 as f64);
            cr.set_source_rgba(r, g, b, 1.);
            cr.stroke()?;
        }
        Tool::Arrow => {
            let (sx, sy) = (a.start.0 as f64, a.start.1 as f64);
            let (ex, ey) = (a.end.0 as f64, a.end.1 as f64);
            let dx = ex - sx;
            let dy = ey - sy;
            let len = (dx * dx + dy * dy).sqrt();
            cr.set_source_rgba(r, g, b, 1.);
            if len > 0. {
                let (ux, uy) = (dx / len, dy / len);
                let head = (lw * 3.5).max(12. * scale);
                let ang = 0.5_f64;
                let (ca, sa) = (ang.cos(), ang.sin());
                let back = head * ca;
                if len > back {
                    cr.set_line_cap(LineCap::Butt);
                    cr.move_to(sx, sy);
                    cr.line_to(ex - ux * back, ey - uy * back);
                    cr.stroke()?;
                }
                let b1 = (-ux * ca + uy * sa, -ux * sa - uy * ca);
                let b2 = (-ux * ca - uy * sa, ux * sa - uy * ca);
                cr.move_to(ex, ey);
                cr.line_to(ex + head * b1.0, ey + head * b1.1);
                cr.line_to(ex + head * b2.0, ey + head * b2.1);
                cr.close_path();
                cr.fill()?;
            }
        }
        Tool::Freehand => {
            cr.set_source_rgba(r, g, b, 1.);
            stroke_freehand(cr, a, lw)?;
        }
        Tool::Highlight => {
            cr.set_source_rgba(r, g, b, HIGHLIGHT_ALPHA);
            stroke_freehand(cr, a, lw)?;
        }
        Tool::Blur => {
            if let Some((mosaic, block)) = blur {
                let pattern = cairo::SurfacePattern::create(mosaic);
                pattern.set_filter(Filter::Nearest);
                let s = block as f64;
                pattern.set_matrix(Matrix::new(1. / s, 0., 0., 1. / s, 0., 0.));
                cr.set_source(&pattern)?;
                stroke_freehand(cr, a, lw)?;
            }
        }
    }
    Ok(())
}

pub fn draw_annotations(
    cr: &Context,
    scale: f64,
    annotations: &[Annotation],
    draw: Option<&Annotation>,
    blur: Option<(&ImageSurface, i32)>,
) -> anyhow::Result<()> {
    for a in annotations {
        draw_annotation(cr, a, scale, blur)?;
    }
    if let Some(d) = draw {
        draw_annotation(cr, d, scale, blur)?;
    }
    Ok(())
}

/// Dims the whole frozen screen (used when no region is selected yet).
pub fn draw_dim_full(cr: &Context, screen_w: i32, screen_h: i32) -> anyhow::Result<()> {
    cr.set_source_rgba(0., 0., 0., DIM);
    cr.rectangle(0., 0., screen_w as f64, screen_h as f64);
    cr.fill()?;
    Ok(())
}

/// Dims everything outside the selection, leaving the selected region at full brightness.
pub fn draw_dim_outside(cr: &Context, sel: Rect, screen_w: i32, screen_h: i32) -> anyhow::Result<()> {
    let (sw, sh) = (screen_w as f64, screen_h as f64);
    let (x, y, w, h) = (sel.x as f64, sel.y as f64, sel.w as f64, sel.h as f64);
    cr.set_source_rgba(0., 0., 0., DIM);
    cr.rectangle(0., 0., sw, y); // top
    cr.rectangle(0., y + h, sw, sh - (y + h)); // bottom
    cr.rectangle(0., y, x, h); // left
    cr.rectangle(x + w, y, sw - (x + w), h); // right
    cr.fill()?;
    Ok(())
}

pub fn draw_selection_border(cr: &Context, sel: Rect, scale: f64) -> anyhow::Result<()> {
    let bw = (SELECTION_BORDER as f64 * scale).max(1.);
    cr.set_source_rgb(1., 1., 1.);
    cr.set_line_width(bw);
    cr.set_line_join(LineJoin::Miter);
    cr.rectangle(
        sel.x as f64 - bw / 2.,
        sel.y as f64 - bw / 2.,
        sel.w as f64 + bw,
        sel.h as f64 + bw,
    );
    cr.stroke()?;
    Ok(())
}

pub fn draw_toolbar(
    cr: &Context,
    scale: f64,
    view: Rect,
    active_tool: Tool,
    color: [f64; 4],
    active_thickness: f64,
) -> anyhow::Result<()> {
    let radius = rnd(scale, TOOLBAR_RADIUS) as f64;
    let (btn, pad, _, offsets) = toolbar_button_offsets(scale);
    let (bounds, _) = toolbar_layout(view, scale);

    cr.save()?;
    cr.translate(bounds.x as f64, bounds.y as f64);

    rounded_rect(cr, 0., 0., bounds.w as f64, bounds.h as f64, radius);
    cr.set_source_rgba(0.1, 0.1, 0.1, 0.92);
    cr.fill()?;

    let line = (2. * scale).max(1.5);
    let bh = btn as f64;

    for (item, ox, bw_i) in &offsets {
        let bw = *bw_i as f64;
        let bx = (pad + ox) as f64;
        let by = pad as f64;

        if matches!(item, ToolbarItem::Tool(t) if *t == active_tool) {
            rounded_rect(cr, bx, by, bw, bh, radius * 0.6);
            cr.set_source_rgba(0.25, 0.45, 0.85, 1.);
            cr.fill()?;
        }

        cr.set_source_rgb(0.95, 0.95, 0.95);
        cr.set_line_width(line);
        cr.set_line_cap(LineCap::Round);
        cr.set_line_join(LineJoin::Round);

        let m = bh * 0.28;
        let x0 = bx + m;
        let y0 = by + m;
        let x1 = bx + bw - m;
        let y1 = by + bh - m;

        match item {
            ToolbarItem::Tool(Tool::Crop) => {
                cr.set_line_cap(LineCap::Butt);
                cr.set_line_join(LineJoin::Miter);
                cr.set_line_width(scale.max(1.));
                cr.set_dash(&[2. * scale, 2. * scale], 0.);
                cr.rectangle(x0, y0, x1 - x0, y1 - y0);
                cr.stroke()?;
                cr.set_dash(&[], 0.);
            }
            ToolbarItem::Tool(Tool::Rectangle) => {
                cr.rectangle(x0, y0, x1 - x0, y1 - y0);
                cr.stroke()?;
            }
            ToolbarItem::Tool(Tool::Line) => {
                cr.move_to(x0, y1);
                cr.line_to(x1, y0);
                cr.stroke()?;
            }
            ToolbarItem::Tool(Tool::Arrow) => {
                cr.move_to(x0, y1);
                cr.line_to(x1, y0);
                cr.stroke()?;
                let head = (x1 - x0) * 0.4;
                cr.move_to(x1 - head, y0);
                cr.line_to(x1, y0);
                cr.line_to(x1, y0 + head);
                cr.stroke()?;
            }
            ToolbarItem::Tool(Tool::Freehand) => {
                let w = x1 - x0;
                let h = y1 - y0;
                cr.move_to(x0, y0 + h * 0.7);
                cr.curve_to(
                    x0 + w * 0.25,
                    y0 - h * 0.1,
                    x0 + w * 0.4,
                    y0 + h * 1.1,
                    x0 + w * 0.6,
                    y0 + h * 0.5,
                );
                cr.curve_to(
                    x0 + w * 0.75,
                    y0 + h * 0.1,
                    x0 + w * 0.9,
                    y0 + h * 0.2,
                    x1,
                    y0 + h * 0.3,
                );
                cr.stroke()?;
            }
            ToolbarItem::Tool(Tool::Highlight) => {
                cr.set_source_rgba(1., 0.85, 0.1, 0.6);
                cr.set_line_width(bh * 0.28);
                cr.move_to(x0, y1);
                cr.line_to(x1, y0);
                cr.stroke()?;
            }
            ToolbarItem::Tool(Tool::Blur) => {
                cr.set_source_rgb(0.95, 0.95, 0.95);
                let w = x1 - x0;
                let h = y1 - y0;
                let dot = (w.min(h) * 0.1).max(scale);
                for iy in 0..3 {
                    for ix in 0..3 {
                        let cx = x0 + w * (0.2 + 0.3 * ix as f64);
                        let cy = y0 + h * (0.2 + 0.3 * iy as f64);
                        cr.arc(cx, cy, dot, 0., TAU);
                        cr.fill()?;
                    }
                }
            }
            ToolbarItem::Clear => {
                let w = x1 - x0;
                let h = y1 - y0;
                let lid_y = y0 + h * 0.2;
                cr.move_to(x0, lid_y);
                cr.line_to(x1, lid_y);
                cr.stroke()?;
                let hw = w * 0.32;
                cr.move_to(bx + bw / 2. - hw / 2., lid_y);
                cr.line_to(bx + bw / 2. - hw / 2., y0);
                cr.line_to(bx + bw / 2. + hw / 2., y0);
                cr.line_to(bx + bw / 2. + hw / 2., lid_y);
                cr.stroke()?;
                cr.move_to(x0 + w * 0.12, lid_y);
                cr.line_to(x0 + w * 0.2, y1);
                cr.line_to(x1 - w * 0.2, y1);
                cr.line_to(x1 - w * 0.12, lid_y);
                cr.stroke()?;
                for t in [0.35, 0.5, 0.65] {
                    let tx = x0 + w * t;
                    cr.move_to(tx, lid_y + h * 0.12);
                    cr.line_to(tx, y1 - h * 0.1);
                    cr.stroke()?;
                }
            }
            ToolbarItem::Color => {
                let cx = bx + bw / 2.;
                let cy = by + bh / 2.;
                let rr = (bh - m * 2.) / 2.;
                cr.arc(cx, cy, rr, 0., TAU);
                cr.set_source_rgba(color[0], color[1], color[2], 1.);
                cr.fill_preserve()?;
                cr.set_source_rgb(0.95, 0.95, 0.95);
                cr.set_line_width(line);
                cr.stroke()?;
            }
            ToolbarItem::Thickness => {
                cr.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
                cr.set_font_size(TOOLBAR_FONT_PX * scale);
                let text = format!("{} px", active_thickness.round() as i32);
                let ext = cr.text_extents(&text)?;
                let tx = bx + (bw - ext.width()) / 2. - ext.x_bearing();
                let ty = by + (bh - ext.height()) / 2. - ext.y_bearing();
                cr.move_to(tx, ty);
                cr.set_source_rgb(0.95, 0.95, 0.95);
                cr.show_text(&text)?;
            }
        }
    }

    cr.restore()?;
    Ok(())
}

pub fn draw_color_wheel(cr: &Context, scale: f64, view: Rect) -> anyhow::Result<()> {
    let (bounds, _, _) = color_wheel_layout(view, scale);
    let radius = rnd(scale, WHEEL_RADIUS);
    let pad = rnd(scale, WHEEL_PAD);
    let top_gap = rnd(scale, PRESET_TOP_GAP);
    let (sw, gap) = preset_swatch_metrics(scale, radius);

    cr.save()?;
    cr.translate(bounds.x as f64, bounds.y as f64);

    rounded_rect(cr, 0., 0., bounds.w as f64, bounds.h as f64, pad as f64);
    cr.set_source_rgba(0.1, 0.1, 0.1, 0.92);
    cr.fill()?;

    let cx = (pad + radius) as f64;
    let cy = (pad + radius) as f64;
    let r = radius as f64;

    let steps = 360;
    for i in 0..steps {
        let a0 = i as f64 / steps as f64 * TAU;
        let a1 = (i + 1) as f64 / steps as f64 * TAU;
        let rgb = hsv_to_rgb(i as f64 / steps as f64, 1., 1.);
        cr.move_to(cx, cy);
        cr.arc(cx, cy, r, a0, a1 + 0.01);
        cr.close_path();
        cr.set_source_rgb(rgb[0], rgb[1], rgb[2]);
        cr.fill()?;
    }

    let grad = cairo::RadialGradient::new(cx, cy, 0., cx, cy, r);
    grad.add_color_stop_rgba(0., 1., 1., 1., 1.);
    grad.add_color_stop_rgba(1., 1., 1., 1., 0.);
    cr.arc(cx, cy, r, 0., TAU);
    cr.set_source(&grad)?;
    cr.fill()?;

    cr.arc(cx, cy, r, 0., TAU);
    cr.set_source_rgb(0.95, 0.95, 0.95);
    cr.set_line_width((2. * scale).max(1.5));
    cr.stroke()?;

    let n = PRESET_COLORS.len() as i32;
    let row_w = sw * n + gap * (n - 1);
    let sx0 = (bounds.w - row_w) / 2;
    let sy = pad + radius * 2 + top_gap;
    let sr = sw as f64 * 0.25;
    for (i, c) in PRESET_COLORS.iter().enumerate() {
        let x = (sx0 + i as i32 * (sw + gap)) as f64;
        rounded_rect(cr, x, sy as f64, sw as f64, sw as f64, sr);
        cr.set_source_rgba(c[0], c[1], c[2], 1.);
        cr.fill()?;
    }

    cr.restore()?;
    Ok(())
}

pub fn draw_thickness_slider(cr: &Context, scale: f64, view: Rect, value: f64) -> anyhow::Result<()> {
    let (bounds, _, _, _) = thickness_slider_layout(view, scale);
    let pad = rnd(scale, SLIDER_PAD);
    let handle_r = rnd(scale, SLIDER_HANDLE_R);
    let track_h = rnd(scale, SLIDER_TRACK_H);
    let text_h = rnd(scale, SLIDER_TEXT_H);

    cr.save()?;
    cr.translate(bounds.x as f64, bounds.y as f64);

    rounded_rect(cr, 0., 0., bounds.w as f64, bounds.h as f64, pad as f64);
    cr.set_source_rgba(0.1, 0.1, 0.1, 0.92);
    cr.fill()?;

    let x0 = (pad + handle_r) as f64;
    let x1 = (bounds.w - pad - handle_r) as f64;
    let cy = (pad + handle_r) as f64;
    let frac = ((value - MIN_THICKNESS) / (MAX_THICKNESS - MIN_THICKNESS)).clamp(0., 1.);
    let hx = x0 + frac * (x1 - x0);
    let th = track_h as f64;

    rounded_rect(cr, x0, cy - th / 2., x1 - x0, th, th / 2.);
    cr.set_source_rgba(0.4, 0.4, 0.4, 1.);
    cr.fill()?;

    if hx > x0 {
        rounded_rect(cr, x0, cy - th / 2., hx - x0, th, th / 2.);
        cr.set_source_rgba(0.25, 0.45, 0.85, 1.);
        cr.fill()?;
    }

    cr.arc(hx, cy, handle_r as f64, 0., TAU);
    cr.set_source_rgb(0.95, 0.95, 0.95);
    cr.fill()?;

    cr.select_font_face("sans-serif", FontSlant::Normal, FontWeight::Normal);
    cr.set_font_size(SLIDER_FONT_PX * scale);
    let text = format!("{} px", value.round() as i32);
    let ext = cr.text_extents(&text)?;
    let tx = (bounds.w as f64 - ext.width()) / 2. - ext.x_bearing();
    let ty = (pad + handle_r * 2) as f64 + text_h as f64 * 0.7;
    cr.move_to(tx, ty);
    cr.set_source_rgb(0.95, 0.95, 0.95);
    cr.show_text(&text)?;

    cr.restore()?;
    Ok(())
}
