//! Annotation editor: state, interaction, and the per-frame render.

use cairo::{Context, ImageSurface};

use crate::render::{self, Annotation, Rect, Tool, ToolbarItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    SaveDisk,
    SaveClipboard,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drag {
    None,
    NewSelection,
    Draw,
    Slider,
    ColorWheel,
}

pub struct Editor {
    // The whole multi-monitor canvas, composited into one surface; all coordinates are in this
    // global canvas space.
    screenshot: ImageSurface,
    width: i32,
    height: i32,
    scale: f64,
    // The output the pointer is currently on (canvas coords); the toolbar/popups live here.
    view: Rect,
    // The output a drag is confined to, locked when the press starts so a tool can't overflow onto
    // an adjacent monitor if the pointer wanders there mid-drag.
    draw_bounds: Rect,

    tool: Tool,
    draw_color: [f64; 4],
    highlight_color: [f64; 4],
    thickness: f64,
    highlight_thickness: f64,
    blur_thickness: f64,

    annotations: Vec<Annotation>,
    draw: Option<Annotation>,

    selection: ((i32, i32), (i32, i32)),
    has_selection: bool,
    selection_backup: ((i32, i32), (i32, i32)),
    had_selection_backup: bool,

    show_color_wheel: bool,
    show_thickness_slider: bool,
    drag: Drag,

    blur_mosaic: Option<ImageSurface>,
    blur_block: i32,
}

impl Editor {
    pub fn new(screenshot: ImageSurface, width: i32, height: i32, scale: f64) -> Self {
        let selection = ((0, 0), (width - 1, height - 1));
        Self {
            screenshot,
            width,
            height,
            scale,
            view: Rect { x: 0, y: 0, w: width, h: height },
            draw_bounds: Rect { x: 0, y: 0, w: width, h: height },
            tool: Tool::Crop,
            draw_color: render::DEFAULT_COLOR,
            highlight_color: render::HIGHLIGHT_COLOR,
            thickness: render::DEFAULT_THICKNESS,
            highlight_thickness: render::HIGHLIGHT_THICKNESS,
            blur_thickness: render::BLUR_THICKNESS,
            annotations: Vec::new(),
            draw: None,
            selection,
            has_selection: false,
            selection_backup: selection,
            had_selection_backup: false,
            show_color_wheel: false,
            show_thickness_slider: false,
            drag: Drag::None,
            blur_mosaic: None,
            blur_block: render::BLUR_DOWNSAMPLE,
        }
    }

    fn clamp(&self, x: i32, y: i32) -> (i32, i32) {
        let b = self.draw_bounds;
        (x.clamp(b.x, b.x + b.w - 1), y.clamp(b.y, b.y + b.h - 1))
    }

    fn active_color_mut(&mut self) -> &mut [f64; 4] {
        if self.tool.is_highlight() {
            &mut self.highlight_color
        } else {
            &mut self.draw_color
        }
    }

    fn active_color(&self) -> [f64; 4] {
        if self.tool.is_highlight() {
            self.highlight_color
        } else {
            self.draw_color
        }
    }

    fn active_thickness_mut(&mut self) -> &mut f64 {
        match self.tool {
            Tool::Highlight => &mut self.highlight_thickness,
            Tool::Blur => &mut self.blur_thickness,
            _ => &mut self.thickness,
        }
    }

    fn active_thickness(&self) -> f64 {
        match self.tool {
            Tool::Highlight => self.highlight_thickness,
            Tool::Blur => self.blur_thickness,
            _ => self.thickness,
        }
    }

    fn adjust_thickness(&mut self, delta: f64) {
        let t = self.active_thickness_mut();
        *t = (*t + delta).clamp(render::MIN_THICKNESS, render::MAX_THICKNESS);
    }

    fn needs_blur(&self) -> bool {
        self.annotations.iter().any(|a| a.tool == Tool::Blur)
            || self.draw.as_ref().is_some_and(|d| d.tool == Tool::Blur)
    }

    fn ensure_mosaic(&mut self) {
        if !self.needs_blur() || self.blur_mosaic.is_some() {
            return;
        }
        match render::build_blur_surface(&self.screenshot, self.width, self.height, self.scale) {
            Ok((m, ds)) => {
                self.blur_mosaic = Some(m);
                self.blur_block = ds;
            }
            Err(err) => eprintln!("shareW: failed to build blur surface: {err}"),
        }
    }

    // -- Rendering -----------------------------------------------------------

    /// Sets the active output (the one the pointer is on), where the toolbar is drawn.
    pub fn set_view(&mut self, view: Rect) {
        self.view = view;
    }

    /// Draws the global canvas (screenshot, dim, annotations, selection border). The caller
    /// translates `cr` by the negative of the output's canvas origin so each overlay shows its
    /// slice of the canvas.
    pub fn render_scene(&mut self, cr: &Context) -> anyhow::Result<()> {
        self.ensure_mosaic();
        let blur = self.blur_mosaic.as_ref().map(|m| (m, self.blur_block));

        cr.set_source_surface(&self.screenshot, 0., 0.)?;
        cr.paint()?;

        let sel = render::rect_from_corner_points(self.selection.0, self.selection.1);
        if self.has_selection {
            render::draw_dim_outside(cr, sel, self.width, self.height)?;
        } else {
            render::draw_dim_full(cr, self.width, self.height)?;
        }

        render::draw_annotations(cr, self.scale, &self.annotations, self.draw.as_ref(), blur)?;

        if self.has_selection {
            render::draw_selection_border(cr, sel, self.scale)?;
        }
        Ok(())
    }

    /// Draws the toolbar and any open popup, anchored to the active output (`self.view`). Drawn
    /// only on the overlay for the active output (in the same canvas-coordinate, `cr`-translated
    /// space as `render_scene`).
    pub fn render_chrome(&self, cr: &Context) -> anyhow::Result<()> {
        render::draw_toolbar(
            cr,
            self.scale,
            self.view,
            self.tool,
            self.active_color(),
            self.active_thickness(),
        )?;
        if self.show_color_wheel {
            render::draw_color_wheel(cr, self.scale, self.view)?;
        }
        if self.show_thickness_slider {
            render::draw_thickness_slider(cr, self.scale, self.view, self.active_thickness())?;
        }
        Ok(())
    }

    /// Renders the final cropped, annotated image for saving (no UI chrome, no darkening).
    pub fn render_save(&mut self) -> anyhow::Result<ImageSurface> {
        self.ensure_mosaic();
        let blur = self.blur_mosaic.as_ref().map(|m| (m, self.blur_block));

        let sel = render::rect_from_corner_points(self.selection.0, self.selection.1);
        let out = ImageSurface::create(cairo::Format::ARgb32, sel.w, sel.h)?;
        let cr = Context::new(&out)?;
        cr.translate(-sel.x as f64, -sel.y as f64);
        cr.set_source_surface(&self.screenshot, 0., 0.)?;
        cr.paint()?;
        render::draw_annotations(&cr, self.scale, &self.annotations, None, blur)?;
        drop(cr);
        Ok(out)
    }

    // -- Input ---------------------------------------------------------------

    pub fn pointer_down(&mut self, x: i32, y: i32) {
        // Confine this drag to the monitor it began on.
        self.draw_bounds = self.view;
        let (x, y) = self.clamp(x, y);
        let point = (x, y);

        // Color wheel popup.
        if self.show_color_wheel {
            let (bounds, center, radius) = render::color_wheel_layout(self.view, self.scale);
            if bounds.contains(x, y) {
                // Swatches: select immediately and close the wheel.
                for (i, r) in render::preset_swatch_rects(bounds, self.scale).iter().enumerate() {
                    if r.contains(x, y) {
                        *self.active_color_mut() = render::PRESET_COLORS[i];
                        self.show_color_wheel = false;
                        return;
                    }
                }
                // Inside the wheel: preview while dragging, commit on pointer up.
                if let Some(rgb) = render::wheel_color_at(point, center, radius) {
                    *self.active_color_mut() = [rgb[0], rgb[1], rgb[2], 1.];
                    self.drag = Drag::ColorWheel;
                }
                return;
            }
        }

        // Thickness slider popup.
        if self.show_thickness_slider {
            let (bounds, x0, x1, _) = render::thickness_slider_layout(self.view, self.scale);
            if bounds.contains(x, y) {
                *self.active_thickness_mut() = render::thickness_from_slider_x(x, x0, x1);
                self.drag = Drag::Slider;
                return;
            }
        }

        // Toolbar.
        let (tb_bounds, buttons) = render::toolbar_layout(self.view, self.scale);
        if tb_bounds.contains(x, y) {
            for (item, r) in &buttons {
                if r.contains(x, y) {
                    match item {
                        ToolbarItem::Tool(t) => {
                            self.tool = *t;
                            self.show_color_wheel = false;
                            self.show_thickness_slider = false;
                        }
                        ToolbarItem::Clear => {
                            self.annotations.clear();
                            self.show_color_wheel = false;
                            self.show_thickness_slider = false;
                        }
                        ToolbarItem::Color => {
                            self.show_color_wheel = !self.show_color_wheel;
                            self.show_thickness_slider = false;
                        }
                        ToolbarItem::Thickness => {
                            self.show_thickness_slider = !self.show_thickness_slider;
                            self.show_color_wheel = false;
                        }
                    }
                    return;
                }
            }
            return;
        }

        // Clicked elsewhere while a popup was open: close it.
        if self.show_color_wheel || self.show_thickness_slider {
            self.show_color_wheel = false;
            self.show_thickness_slider = false;
            return;
        }

        // Drawing tools.
        if self.tool.is_drawing() {
            let (th, color) = match self.tool {
                Tool::Highlight => (self.highlight_thickness, self.highlight_color),
                Tool::Blur => (self.blur_thickness, self.draw_color),
                _ => (self.thickness, self.draw_color),
            };
            let points = if self.tool.is_freehand() {
                vec![point]
            } else {
                Vec::new()
            };
            self.draw = Some(Annotation {
                tool: self.tool,
                start: point,
                end: point,
                points,
                color,
                thickness: th,
            });
            self.drag = Drag::Draw;
            return;
        }

        // Crop: start a new selection (shown bright immediately as a live preview).
        self.selection_backup = self.selection;
        self.had_selection_backup = self.has_selection;
        self.selection = (point, point);
        self.has_selection = true;
        self.drag = Drag::NewSelection;
    }

    pub fn pointer_motion(&mut self, x: i32, y: i32) {
        let (x, y) = self.clamp(x, y);
        match self.drag {
            Drag::Slider => {
                let (_, x0, x1, _) = render::thickness_slider_layout(self.view, self.scale);
                *self.active_thickness_mut() = render::thickness_from_slider_x(x, x0, x1);
            }
            Drag::Draw => {
                if let Some(d) = &mut self.draw {
                    d.end = (x, y);
                    if d.tool.is_freehand() {
                        d.points.push((x, y));
                    }
                }
            }
            Drag::NewSelection => {
                self.selection.1 = (x, y);
            }
            Drag::ColorWheel => {
                let (_, center, radius) = render::color_wheel_layout(self.view, self.scale);
                if let Some(rgb) = render::wheel_color_at((x, y), center, radius) {
                    *self.active_color_mut() = [rgb[0], rgb[1], rgb[2], 1.];
                }
            }
            Drag::None => {}
        }
    }

    pub fn pointer_up(&mut self) {
        match self.drag {
            Drag::Draw => {
                if let Some(d) = self.draw.take() {
                    let keep = if d.tool.is_freehand() {
                        !d.points.is_empty()
                    } else {
                        d.start != d.end
                    };
                    if keep {
                        self.annotations.push(d);
                    }
                }
            }
            Drag::NewSelection => {
                let sel = render::rect_from_corner_points(self.selection.0, self.selection.1);
                // Treat a tiny drag as an accidental click and restore the previous state.
                if sel.w < 8 || sel.h < 8 {
                    self.selection = self.selection_backup;
                    self.has_selection = self.had_selection_backup;
                }
            }
            Drag::ColorWheel => {
                self.show_color_wheel = false;
            }
            _ => {}
        }
        self.drag = Drag::None;
    }

    pub fn scroll(&mut self, up: bool) {
        self.adjust_thickness(if up { 2. } else { -2. });
    }

    /// Handles a key press. `ch` is the logical character (if any); `named_*` flags cover the
    /// special keys we care about.
    pub fn key(
        &mut self,
        ch: Option<&str>,
        ctrl: bool,
        escape: bool,
        enter_or_space: bool,
    ) -> Action {
        if escape {
            return Action::Cancel;
        }
        if ctrl {
            match ch {
                Some("z") => self.annotations.pop().map(|_| ()).unwrap_or(()),
                Some("c") => return Action::SaveClipboard,
                _ => {}
            }
            return Action::None;
        }
        if enter_or_space {
            return Action::SaveDisk;
        }
        match ch {
            Some("c") => self.select_tool(Tool::Crop),
            Some("r") => self.select_tool(Tool::Rectangle),
            Some("l") => self.select_tool(Tool::Line),
            Some("a") => self.select_tool(Tool::Arrow),
            Some("f") => self.select_tool(Tool::Freehand),
            Some("h") => self.select_tool(Tool::Highlight),
            Some("b") => self.select_tool(Tool::Blur),
            Some("e") => self.annotations.clear(),
            Some("-") | Some("_") => self.adjust_thickness(-2.),
            Some("=") | Some("+") => self.adjust_thickness(2.),
            _ => {}
        }
        Action::None
    }

    fn select_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.show_color_wheel = false;
        self.show_thickness_slider = false;
    }
}
