mod editor;
mod render;
mod save;

use anyhow::Context as _;
use cairo::{Context, Format as CairoFormat, ImageSurface};
use editor::{Action, Editor};
use render::Rect;

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers};
use smithay_client_toolkit::seat::pointer::{
    CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::SlotPool;
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, registry_handlers,
};

use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard;
use smithay_client_toolkit::reexports::client::protocol::wl_output::WlOutput;
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::Format;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use smithay_client_toolkit::reexports::protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

const BTN_LEFT: u32 = 0x110;

/// In-progress screencopy of one output.
struct Capture {
    width: i32,
    height: i32,
    stride: i32,
    format: Format,
    data: Vec<u8>,
    done: bool,
    failed: bool,
}

impl Capture {
    fn reset() -> Self {
        Self {
            width: 0,
            height: 0,
            stride: 0,
            format: Format::Xrgb8888,
            data: Vec::new(),
            done: false,
            failed: false,
        }
    }
}

/// One per-output overlay: a viewport onto the shared global canvas.
struct Overlay {
    layer: LayerSurface,
    /// Top-left of this output within the global canvas, in device px.
    origin: (i32, i32),
    /// Output size in device px.
    size: (i32, i32),
    /// Output scale (logical pointer coords are multiplied by this).
    scale: i32,
    configured: bool,
    /// True after a commit that requested a frame callback, until the callback fires. While set,
    /// further redraws are coalesced into `needs_redraw` so we paint at most once per display
    /// frame instead of once per pointer event.
    frame_pending: bool,
    /// A redraw was requested while `frame_pending` was set; draw once the callback fires.
    needs_redraw: bool,
}

impl Overlay {
    fn view(&self) -> Rect {
        Rect {
            x: self.origin.0,
            y: self.origin.1,
            w: self.size.0,
            h: self.size.1,
        }
    }
}

struct State {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    screencopy: ZwlrScreencopyManagerV1,
    pool: SlotPool,
    qh: QueueHandle<State>,

    keyboard: Option<WlKeyboard>,
    themed_pointer: Option<ThemedPointer>,
    ctrl: bool,

    capture: Capture,
    // niri's wlr-screencopy requires the shm pool to be *exactly* the buffer size, so the capture
    // gets its own tightly-sized pool rather than the shared (over-allocated) `pool`.
    capture_pool: Option<SlotPool>,
    capture_buffer: Option<smithay_client_toolkit::shm::slot::Buffer>,

    // The single editor over the whole multi-monitor canvas.
    editor: Option<Editor>,
    overlays: Vec<Overlay>,
    // The output the pointer is currently on (the toolbar follows it).
    active: Option<usize>,
    down: bool,
    exit: bool,
}

fn main() -> anyhow::Result<()> {
    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let (globals, mut queue) = registry_queue_init(&conn).context("registry init")?;
    let qh = queue.handle();

    let registry_state = RegistryState::new(&globals);
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);
    let shm = Shm::bind(&globals, &qh).context("wl_shm not available")?;
    let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor not available")?;
    let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell not available")?;
    let screencopy: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .context("wlr-screencopy not available")?;
    let pool = SlotPool::new(1, &shm).context("creating shm pool")?;

    let mut state = State {
        registry_state,
        output_state,
        seat_state,
        shm,
        compositor,
        layer_shell,
        screencopy,
        pool,
        qh: qh.clone(),
        keyboard: None,
        themed_pointer: None,
        ctrl: false,
        capture: Capture::reset(),
        capture_pool: None,
        capture_buffer: None,
        editor: None,
        overlays: Vec::new(),
        active: None,
        down: false,
        exit: false,
    };

    // Learn about outputs.
    queue.roundtrip(&mut state)?;

    let outputs: Vec<WlOutput> = state.output_state.outputs().collect();
    if outputs.is_empty() {
        anyhow::bail!("no outputs found");
    }

    // Capture every output (before any overlay is mapped, so they aren't in the shots).
    struct Captured {
        output: WlOutput,
        image: ImageSurface,
        w: i32,
        h: i32,
        scale: i32,
        // Logical position in the global layout (compositor coords).
        logical: (i32, i32),
    }
    let mut captured = Vec::new();
    for output in &outputs {
        let (scale, logical) = state
            .output_state
            .info(output)
            .map(|i| (i.scale_factor.max(1), i.logical_position.unwrap_or((0, 0))))
            .unwrap_or((1, (0, 0)));

        state.capture = Capture::reset();
        state.capture_pool = None;
        state.capture_buffer = None;
        let _f: ZwlrScreencopyFrameV1 = state.screencopy.capture_output(0, output, &qh, ());
        while !state.capture.done && !state.capture.failed {
            queue.blocking_dispatch(&mut state)?;
        }
        if state.capture.failed {
            eprintln!("shareW: screencopy failed for an output; skipping it");
            continue;
        }
        let (w, h) = (state.capture.width, state.capture.height);
        let image = state.build_screenshot()?;
        captured.push(Captured {
            output: output.clone(),
            image,
            w,
            h,
            scale,
            logical,
        });
    }
    if captured.is_empty() {
        anyhow::bail!("failed to capture any output");
    }

    // Assemble the global canvas. Assume a uniform scale across outputs (the common case).
    let scale = captured[0].scale;
    let min_lx = captured.iter().map(|c| c.logical.0).min().unwrap();
    let min_ly = captured.iter().map(|c| c.logical.1).min().unwrap();
    let origin_of = |c: &Captured| ((c.logical.0 - min_lx) * scale, (c.logical.1 - min_ly) * scale);

    let canvas_w = captured
        .iter()
        .map(|c| origin_of(c).0 + c.w)
        .max()
        .unwrap();
    let canvas_h = captured
        .iter()
        .map(|c| origin_of(c).1 + c.h)
        .max()
        .unwrap();

    let global = ImageSurface::create(CairoFormat::Rgb24, canvas_w, canvas_h)?;
    {
        let cr = Context::new(&global)?;
        for c in &captured {
            let (ox, oy) = origin_of(c);
            cr.set_source_surface(&c.image, ox as f64, oy as f64)?;
            cr.rectangle(ox as f64, oy as f64, c.w as f64, c.h as f64);
            cr.fill()?;
        }
    }
    state.editor = Some(Editor::new(global, canvas_w, canvas_h, scale as f64));

    // Map one overlay per output, each a viewport onto the global canvas.
    for c in &captured {
        let (ox, oy) = origin_of(c);
        let surface = state.compositor.create_surface(&qh);
        let layer = state.layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("sharew"),
            Some(&c.output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_exclusive_zone(-1);
        layer.set_size(0, 0);
        layer.wl_surface().set_buffer_scale(c.scale);
        layer.commit();

        state.overlays.push(Overlay {
            layer,
            origin: (ox, oy),
            size: (c.w, c.h),
            scale: c.scale,
            configured: false,
            frame_pending: false,
            needs_redraw: false,
        });
    }

    while !state.exit {
        queue.blocking_dispatch(&mut state)?;
    }
    Ok(())
}

impl State {
    fn build_screenshot(&mut self) -> anyhow::Result<ImageSurface> {
        let (w, h, stride) = (self.capture.width, self.capture.height, self.capture.stride);
        let src = &self.capture.data;
        // Rgb24 is a 32-bit BGRX format, identical to the common Xrgb8888 shm layout, so the fast
        // path is a straight per-row memcpy. Only R/B-swapped formats need per-pixel work.
        let mut out = ImageSurface::create(CairoFormat::Rgb24, w, h)?;
        let dst_stride = out.stride() as usize;
        let src_stride = stride as usize;
        let row = w as usize * 4;
        {
            let mut data = out.data().map_err(|e| anyhow::anyhow!("{e}"))?;
            let swap_rb = matches!(self.capture.format, Format::Xbgr8888 | Format::Abgr8888);
            if swap_rb {
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let s = y * src_stride + x * 4;
                        let d = y * dst_stride + x * 4;
                        data[d] = src[s + 2];
                        data[d + 1] = src[s + 1];
                        data[d + 2] = src[s];
                    }
                }
            } else {
                for y in 0..h as usize {
                    data[y * dst_stride..y * dst_stride + row]
                        .copy_from_slice(&src[y * src_stride..y * src_stride + row]);
                }
            }
        }
        Ok(out)
    }

    fn overlay_index(&self, surface: &WlSurface) -> Option<usize> {
        self.overlays
            .iter()
            .position(|o| o.layer.wl_surface() == surface)
    }

    /// Coalesces redraws across all overlays into at most one paint per output per frame.
    fn request_redraw_all(&mut self) {
        for i in 0..self.overlays.len() {
            self.request_redraw(i);
        }
    }

    /// Either paints now (if no frame callback is in flight) or marks the overlay dirty so it
    /// will repaint when the callback fires. This stops a fast-moving pointer from queuing up
    /// commits faster than the compositor can display them — which was making the first stroke
    /// trail behind the cursor.
    fn request_redraw(&mut self, index: usize) {
        let Some(o) = self.overlays.get_mut(index) else {
            return;
        };
        if o.frame_pending {
            o.needs_redraw = true;
        } else {
            self.draw(index);
        }
    }

    fn draw(&mut self, index: usize) {
        let (origin, size, configured) = match self.overlays.get(index) {
            Some(o) => (o.origin, o.size, o.configured),
            None => return,
        };
        if !configured {
            return;
        }
        let active = self.active == Some(index);
        let (w, h) = size;
        let stride = w * 4;

        // Render this output's slice of the global canvas.
        let mut frame = match ImageSurface::create(CairoFormat::ARgb32, w, h) {
            Ok(s) => s,
            Err(err) => {
                eprintln!("shareW: surface alloc failed: {err}");
                return;
            }
        };
        {
            let Some(editor) = self.editor.as_mut() else {
                return;
            };
            let cr = match Context::new(&frame) {
                Ok(c) => c,
                Err(err) => {
                    eprintln!("shareW: cairo context: {err}");
                    return;
                }
            };
            cr.translate(-origin.0 as f64, -origin.1 as f64);
            if let Err(err) = editor.render_scene(&cr) {
                eprintln!("shareW: render: {err}");
            }
            if active {
                if let Err(err) = editor.render_chrome(&cr) {
                    eprintln!("shareW: chrome: {err}");
                }
            }
        }
        frame.flush();

        let (buffer, canvas) = match self.pool.create_buffer(w, h, stride, Format::Argb8888) {
            Ok(b) => b,
            Err(err) => {
                eprintln!("shareW: create_buffer: {err}");
                return;
            }
        };
        let cstride = frame.stride() as usize;
        let data = match frame.data() {
            Ok(d) => d,
            Err(err) => {
                eprintln!("shareW: surface data: {err}");
                return;
            }
        };
        let row = w as usize * 4;
        for y in 0..h as usize {
            canvas[y * stride as usize..y * stride as usize + row]
                .copy_from_slice(&data[y * cstride..y * cstride + row]);
        }
        drop(data);

        let surface = self.overlays[index].layer.wl_surface();
        if buffer.attach_to(surface).is_err() {
            return;
        }
        surface.damage_buffer(0, 0, w, h);
        // Throttle to the compositor's frame rate: the next paint waits until the compositor has
        // shown this one. Mark the overlay clean *before* requesting the callback so a redraw
        // request that arrives during commit still gets a follow-up paint.
        surface.frame(&self.qh, surface.clone());
        surface.commit();
        self.overlays[index].frame_pending = true;
        self.overlays[index].needs_redraw = false;
    }

    fn handle_action(&mut self, action: Action) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        match action {
            Action::None => {}
            Action::Cancel => self.exit = true,
            Action::SaveDisk => {
                match editor.render_save().and_then(|s| save::save_to_disk(&s)) {
                    Ok(path) => println!("{}", path.display()),
                    Err(err) => eprintln!("shareW: save failed: {err}"),
                }
                self.exit = true;
            }
            Action::SaveClipboard => {
                if let Err(err) = editor.render_save().and_then(|s| save::copy_to_clipboard(&s)) {
                    eprintln!("shareW: copy failed: {err}");
                }
                self.exit = true;
            }
        }
    }
}

// --- screencopy ------------------------------------------------------------

impl Dispatch<ZwlrScreencopyManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: <ZwlrScreencopyManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: <ZwlrScreencopyFrameV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if state.capture_buffer.is_some() {
                    return; // already chose a buffer
                }
                let format = match format {
                    WEnum::Value(f) => f,
                    WEnum::Unknown(_) => Format::Xrgb8888,
                };
                state.capture.width = width as i32;
                state.capture.height = height as i32;
                state.capture.stride = stride as i32;
                state.capture.format = format;

                // A pool sized exactly to this buffer (niri checks pool_size == stride*height).
                let len = stride as usize * height as usize;
                let mut pool = match SlotPool::new(len, &state.shm) {
                    Ok(p) => p,
                    Err(err) => {
                        eprintln!("shareW: capture pool alloc failed: {err}");
                        state.capture.failed = true;
                        return;
                    }
                };
                match pool.create_buffer(width as i32, height as i32, stride as i32, format) {
                    Ok((buffer, _)) => {
                        frame.copy(buffer.wl_buffer());
                        state.capture_buffer = Some(buffer);
                        state.capture_pool = Some(pool);
                    }
                    Err(err) => {
                        eprintln!("shareW: capture buffer alloc failed: {err}");
                        state.capture.failed = true;
                    }
                }
            }
            Event::Ready { .. } => {
                if let (Some(pool), Some(buffer)) =
                    (state.capture_pool.as_mut(), state.capture_buffer.as_ref())
                {
                    if let Some(canvas) = pool.canvas(buffer) {
                        state.capture.data = canvas.to_vec();
                    }
                }
                state.capture.done = true;
            }
            Event::Failed => state.capture.failed = true,
            _ => {}
        }
    }
}

// --- layer shell -----------------------------------------------------------

impl LayerShellHandler for State {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(index) = self
            .overlays
            .iter()
            .position(|o| &o.layer == layer)
        {
            self.overlays[index].configured = true;
            self.request_redraw(index);
        }
    }
}

// --- compositor ------------------------------------------------------------

impl CompositorHandler for State {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: smithay_client_toolkit::reexports::client::protocol::wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        let Some(index) = self.overlay_index(surface) else {
            return;
        };
        self.overlays[index].frame_pending = false;
        if self.overlays[index].needs_redraw {
            self.draw(index);
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: &WlOutput) {}
}

// --- output ----------------------------------------------------------------

impl OutputHandler for State {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {}
}

// --- shm -------------------------------------------------------------------

impl ShmHandler for State {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

// --- seat ------------------------------------------------------------------

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            if let Ok(kbd) = self.seat_state.get_keyboard(qh, &seat, None) {
                self.keyboard = Some(kbd);
            }
        }
        if capability == Capability::Pointer && self.themed_pointer.is_none() {
            let surface = self.compositor.create_surface(qh);
            if let Ok(themed) = self.seat_state.get_pointer_with_theme(
                qh,
                &seat,
                self.shm.wl_shm(),
                surface,
                ThemeSpec::default(),
            ) {
                self.themed_pointer = Some(themed);
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat, _: Capability) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

// --- keyboard --------------------------------------------------------------

impl KeyboardHandler for State {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: &WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: &WlSurface, _: u32) {}
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlKeyboard, _: u32, modifiers: Modifiers, _: u32) {
        self.ctrl = modifiers.ctrl;
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let k = event.keysym;
        let escape = k == Keysym::Escape;
        let enter_or_space = k == Keysym::Return || k == Keysym::KP_Enter || k == Keysym::space;
        let ch = keysym_to_ch(k);

        let action = match self.editor.as_mut() {
            Some(editor) => editor.key(ch, self.ctrl, escape, enter_or_space),
            None => Action::None,
        };
        self.handle_action(action);
        self.request_redraw_all();
    }
}

fn keysym_to_ch(k: Keysym) -> Option<&'static str> {
    let pairs: &[(&[Keysym], &str)] = &[
        (&[Keysym::c, Keysym::C], "c"),
        (&[Keysym::r, Keysym::R], "r"),
        (&[Keysym::l, Keysym::L], "l"),
        (&[Keysym::a, Keysym::A], "a"),
        (&[Keysym::f, Keysym::F], "f"),
        (&[Keysym::h, Keysym::H], "h"),
        (&[Keysym::b, Keysym::B], "b"),
        (&[Keysym::e, Keysym::E], "e"),
        (&[Keysym::z, Keysym::Z], "z"),
        (&[Keysym::minus, Keysym::KP_Subtract], "-"),
        (&[Keysym::equal], "="),
        (&[Keysym::plus, Keysym::KP_Add], "+"),
    ];
    for (keys, s) in pairs {
        if keys.contains(&k) {
            return Some(s);
        }
    }
    None
}

// --- pointer ---------------------------------------------------------------

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        _: &WlPointer,
        events: &[PointerEvent],
    ) {
        let mut redraw_all = false;
        let mut redraw_active = false;
        for e in events {
            let Some(index) = self.overlay_index(&e.surface) else {
                continue;
            };
            let origin = self.overlays[index].origin;
            let scale = self.overlays[index].scale as f64;
            // Translate the output-local logical position into global canvas (device) coords.
            let gx = origin.0 + (e.position.0 * scale) as i32;
            let gy = origin.1 + (e.position.1 * scale) as i32;
            let view = self.overlays[index].view();
            match e.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(themed) = &self.themed_pointer {
                        let _ = themed.set_cursor(conn, CursorIcon::Crosshair);
                    }
                    // The toolbar follows the pointer to this output.
                    self.active = Some(index);
                    if let Some(ed) = self.editor.as_mut() {
                        ed.set_view(view);
                    }
                    redraw_all = true;
                }
                PointerEventKind::Motion { .. } => {
                    if self.down {
                        if let Some(ed) = self.editor.as_mut() {
                            ed.pointer_motion(gx, gy);
                        }
                        redraw_active = true;
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.down = true;
                    self.active = Some(index);
                    if let Some(ed) = self.editor.as_mut() {
                        ed.set_view(view);
                        ed.pointer_down(gx, gy);
                    }
                    redraw_all = true;
                }
                PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                    self.down = false;
                    if let Some(ed) = self.editor.as_mut() {
                        ed.pointer_up();
                    }
                    redraw_all = true;
                }
                PointerEventKind::Axis { vertical, .. } => {
                    if vertical.absolute != 0. {
                        if let Some(ed) = self.editor.as_mut() {
                            ed.scroll(vertical.absolute < 0.);
                        }
                        redraw_all = true;
                    }
                }
                _ => {}
            }
        }
        if redraw_all {
            self.request_redraw_all();
        } else if redraw_active {
            if let Some(i) = self.active {
                self.request_redraw(i);
            }
        }
    }
}

// --- registry --------------------------------------------------------------

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(State);
delegate_output!(State);
delegate_shm!(State);
delegate_seat!(State);
delegate_keyboard!(State);
delegate_pointer!(State);
delegate_layer!(State);
delegate_registry!(State);
