//! Click-through wlr-layer-shell overlays, one per output, that the sheep
//! live on.
//!
//! The sheep exist in a single global coordinate space spanning every monitor;
//! each panel draws whichever of them overlap its own output, so a sheep
//! crossing the seam is simply drawn on both.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::anim::Pet;
use crate::config::Config;
use crate::engine::{Event, Sheep, World};
use crate::hypr;
use crate::sprites::Sheet;

/// Linux input code for the left mouse button.
const BTN_LEFT: u32 = 0x110;
/// How often to re-read the window layout even without an event, so that
/// interactive drags and resizes are followed smoothly.
const REFRESH: Duration = Duration::from_millis(200);
/// Guard against a burst of catch-up steps after the compositor stalls us.
const MAX_STEPS_PER_FRAME: usize = 8;

struct Pen {
    sheep: Sheep,
    next_step: Instant,
    /// Last (animation, step) reported by the trace, so that re-entering the
    /// same animation still logs.
    traced: (u32, u32),
}

/// One output's overlay surface.
struct Panel {
    output: wl_output::WlOutput,
    /// Connector name, used to match this output to a Hyprland monitor.
    name: String,
    layer: LayerSurface,
    pool: SlotPool,
    /// Logical size, as configured by the compositor.
    width: u32,
    height: u32,
    /// Output scale; the shm buffer is this many times larger.
    scale: u32,
    /// Global logical position of this output's top-left corner.
    origin: (f64, f64),
}

pub struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    compositor: CompositorState,
    layer_shell: LayerShell,
    pointer: Option<wl_pointer::WlPointer>,
    /// Index of the sheep the pointer is pressed on, if any.
    pressed: Option<usize>,
    pub exit: bool,

    panels: Vec<Panel>,
    sheet: Sheet,
    pet: Pet,
    /// Sprite size. Tiles are not always a whole number of pixels, so the
    /// sheet is sampled at the exact fraction and the sprite drawn rounded.
    tile: f64,
    tile_h: f64,
    flock: Vec<Pen>,

    monitors: Vec<hypr::Monitor>,
    world: World,
    cfg: Config,
    dirty: Arc<AtomicBool>,
    last_refresh: Instant,
    /// Log every animation change; set HYPRSHEEP_TRACE=1.
    trace: bool,
}

pub fn run(sheet: Sheet, pet: Pet, cfg: Config) -> Result<(), String> {
    let (monitors, world) = hypr::world()?;
    let dirty = Arc::new(AtomicBool::new(false));
    hypr::watch(dirty.clone());

    let conn = Connection::connect_to_env().map_err(|e| format!("wayland connect: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init(&conn).map_err(|e| format!("registry init: {e}"))?;
    let qh = queue.handle();

    let compositor =
        CompositorState::bind(&globals, &qh).map_err(|e| format!("wl_compositor: {e}"))?;
    let layer_shell =
        LayerShell::bind(&globals, &qh).map_err(|e| format!("wlr-layer-shell: {e}"))?;
    let shm = Shm::bind(&globals, &qh).map_err(|e| format!("wl_shm: {e}"))?;
    let tile = sheet.width as f64 / pet.tiles_x.max(1) as f64;
    let tile_h = sheet.height as f64 / pet.tiles_y.max(1) as f64;

    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm,
        compositor,
        layer_shell,
        pointer: None,
        pressed: None,
        exit: false,
        panels: Vec::new(),
        sheet,
        pet,
        tile,
        tile_h,
        flock: Vec::new(),
        monitors,
        world,
        cfg,
        dirty,
        last_refresh: Instant::now(),
        trace: std::env::var_os("HYPRSHEEP_TRACE").is_some(),
    };

    // Outputs already present are announced during the first roundtrip, which
    // calls new_output for each and gives it a panel.
    queue.roundtrip(&mut overlay).map_err(|e| format!("roundtrip: {e}"))?;
    if overlay.panels.is_empty() {
        let available: Vec<&str> =
            overlay.monitors.iter().map(|m| m.name.as_str()).collect();
        return Err(format!(
            "no usable outputs: the `monitors` setting matches none of {}",
            if available.is_empty() { "any attached monitor".to_string() } else { available.join(", ") }
        ));
    }
    overlay.top_up_flock();

    loop {
        queue.blocking_dispatch(&mut overlay).map_err(|e| format!("dispatch: {e}"))?;
        if overlay.exit {
            return Ok(());
        }
    }
}

impl Overlay {
    /// Give an output its own overlay surface.
    fn add_panel(&mut self, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if self.panels.iter().any(|p| p.output == output) {
            return;
        }
        let info = self.output_state.info(&output);
        let name = info.as_ref().and_then(|i| i.name.clone()).unwrap_or_default();
        if !self.cfg.monitors.allows(&name) {
            println!("hyprsheep: skipping {name}, not in the configured monitors");
            return;
        }

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("hyprsheep"),
            Some(&output),
        );

        // Anchoring to all four edges with a zero size asks the compositor for
        // the full output. A negative exclusive zone means bars don't push us
        // around, so the sheep can reach every pixel of the screen.
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        // Start fully click-through; the region is narrowed to the sheep once
        // there are any on this output.
        if let Ok(empty) = Region::new(&self.compositor) {
            layer.wl_surface().set_input_region(Some(empty.wl_region()));
        }
        layer.commit();

        let pool = match SlotPool::new(256 * 256 * 4, &self.shm) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("hyprsheep: shm pool for {name}: {e}");
                return;
            }
        };

        let origin = self
            .monitors
            .iter()
            .find(|m| m.name == name)
            .map(|m| (m.x, m.y))
            .unwrap_or((0.0, 0.0));

        println!("hyprsheep: overlay on {name} at ({}, {})", origin.0, origin.1);
        self.panels.push(Panel {
            output,
            name,
            layer,
            pool,
            width: 0,
            height: 0,
            scale: 1,
            origin,
        });
    }

    fn add_sheep(&mut self, is_child: bool, start: Option<(u32, f64, f64)>) {
        let mut sheep = Sheep::new(is_child);
        let mut events = Vec::new();
        match start {
            Some((animation, x, y)) => {
                sheep.x = x;
                sheep.y = y;
                sheep.begin(&self.pet, &self.world, self.tile, animation, &mut events);
            }
            None => sheep.spawn(&self.pet, &self.world, self.tile, &mut events),
        }
        self.flock.push(Pen { sheep, next_step: Instant::now(), traced: (u32::MAX, 0) });
        self.handle(events);
    }

    fn handle(&mut self, events: Vec<Event>) {
        for e in events {
            if let Event::SpawnChild { animation, x, y } = e {
                // A companion is a fully independent sheep that dies rather
                // than respawning when its chain ends.
                self.add_sheep(true, Some((animation, x, y)));
            }
        }
    }

    /// Re-read the monitor layout and windows when an event says they may have
    /// changed, or periodically to follow an in-progress drag.
    fn refresh_world(&mut self) {
        let due =
            self.dirty.swap(false, Ordering::Relaxed) || self.last_refresh.elapsed() >= REFRESH;
        if !due {
            return;
        }
        self.last_refresh = Instant::now();

        match hypr::world() {
            Ok((monitors, world)) => {
                self.monitors = monitors;
                self.world = world;
                // A monitor may have been moved or rescaled underneath us.
                for panel in &mut self.panels {
                    if let Some(m) = self.monitors.iter().find(|m| m.name == panel.name) {
                        panel.origin = (m.x, m.y);
                    }
                }
            }
            Err(e) => eprintln!("hyprsheep: layout query failed: {e}"),
        }
    }

    fn tick(&mut self) {
        self.refresh_world();

        let now = Instant::now();
        let mut events = Vec::new();
        let mut dead = Vec::new();

        for (i, pen) in self.flock.iter_mut().enumerate() {
            let mut steps = 0;
            while now >= pen.next_step && steps < MAX_STEPS_PER_FRAME {
                let delay = pen.sheep.step(&self.pet, &self.world, self.tile, &mut events);
                pen.next_step += delay;
                steps += 1;
            }
            // If we fell far behind, resynchronise rather than sprinting.
            if steps == MAX_STEPS_PER_FRAME && now > pen.next_step {
                pen.next_step = now;
            }

            let restarted = pen.sheep.animation != pen.traced.0 || pen.sheep.step < pen.traced.1;
            if self.trace && restarted {
                pen.traced = (pen.sheep.animation, pen.sheep.step);
                let name =
                    self.pet.get(pen.sheep.animation).map(|a| a.name.as_str()).unwrap_or("?");
                println!(
                    "{:>3} {:<18} at ({:>6.0},{:>5.0}) {}{}",
                    pen.sheep.animation,
                    name,
                    pen.sheep.x,
                    pen.sheep.y,
                    if pen.sheep.flipped { "flipped " } else { "" },
                    match pen.sheep.resting_on() {
                        Some(id) => format!("on window {id:#x}"),
                        None => "airborne".to_string(),
                    }
                );
            } else if self.trace {
                pen.traced.1 = pen.sheep.step;
            }

            if events.contains(&Event::Died) {
                dead.push(i);
            }
            events.retain(|e| *e != Event::Died);
        }

        for i in dead.into_iter().rev() {
            self.flock.remove(i);
        }
        self.handle(events);

        self.top_up_flock();
    }

    /// Keep the configured number of ordinary sheep on screen. Companions are
    /// not counted: they come and go on their own.
    fn top_up_flock(&mut self) {
        let ordinary = self.flock.iter().filter(|p| !p.sheep.is_child).count();
        for _ in ordinary..self.cfg.sheep {
            self.add_sheep(false, None);
        }
    }

    /// The topmost sheep whose sprite covers a global point.
    fn sheep_at(&self, x: f64, y: f64) -> Option<usize> {
        self.flock.iter().rposition(|pen| {
            let top = pen.sheep.y + pen.sheep.offset_y;
            x >= pen.sheep.x && x < pen.sheep.x + self.tile && y >= top && y < top + self.tile
        })
    }

    fn draw(&mut self, qh: &QueueHandle<Self>, index: usize) {
        self.tick();

        let Some(panel) = self.panels.get_mut(index) else { return };
        if panel.width == 0 || panel.height == 0 {
            return;
        }

        let scale = panel.scale as i32;
        let bw = panel.width as i32 * scale;
        let bh = panel.height as i32 * scale;
        let stride = bw * 4;

        let (buffer, canvas) =
            match panel.pool.create_buffer(bw, bh, stride, wl_shm::Format::Argb8888) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("hyprsheep: buffer alloc failed: {e}");
                    return;
                }
            };

        // Fully transparent everywhere except the sheep.
        canvas.fill(0);

        let tile = self.tile.round() as u32;
        let region = Region::new(&self.compositor).ok();

        for pen in &self.flock {
            let s = &pen.sheep;
            // Global position translated into this output's own space; a sheep
            // straddling two outputs is drawn on both and clipped by each.
            let lx = s.x - panel.origin.0;
            let ly = s.y + s.offset_y - panel.origin.1;
            if lx + self.tile <= 0.0
                || ly + self.tile <= 0.0
                || lx >= panel.width as f64
                || ly >= panel.height as f64
            {
                continue;
            }

            if self.cfg.draggable {
                if let Some(r) = &region {
                    r.add(lx.round() as i32, ly.round() as i32, tile as i32, tile as i32);
                }
            }

            let (col, row) = (s.frame % self.pet.tiles_x, s.frame / self.pet.tiles_x);
            let sx0 = (col as f64 * self.tile).round() as u32;
            let sy0 = (row as f64 * self.tile_h).round() as u32;
            let ox = lx.round() as i32 * scale;
            let oy = ly.round() as i32 * scale;
            let alpha = s.opacity.clamp(0.0, 1.0);

            // Nearest-neighbour upscale by the output scale keeps the pixel art
            // crisp on HiDPI rather than letting the compositor blur it.
            for sy in 0..tile {
                for sx in 0..tile {
                    // Flipping mirrors the sprite horizontally; the engine
                    // mirrors its velocity to match.
                    let src_x = if s.flipped { tile - 1 - sx } else { sx };
                    let [r, g, b, a] = self.sheet.pixel(sx0 + src_x, sy0 + sy);
                    if a == 0 {
                        continue;
                    }
                    let a = (a as f64 * alpha) as u32;
                    if a == 0 {
                        continue;
                    }
                    // wl_shm ARGB8888 expects premultiplied alpha.
                    let pm = |c: u8| ((c as u32 * a) / 255) as u8;
                    let argb = u32::from_le_bytes([pm(b), pm(g), pm(r), a as u8]).to_le_bytes();

                    for dy in 0..scale {
                        let py = oy + sy as i32 * scale + dy;
                        if py < 0 || py >= bh {
                            continue;
                        }
                        for dx in 0..scale {
                            let px = ox + sx as i32 * scale + dx;
                            if px < 0 || px >= bw {
                                continue;
                            }
                            let i = ((py * bw + px) * 4) as usize;
                            canvas[i..i + 4].copy_from_slice(&argb);
                        }
                    }
                }
            }
        }

        let surface = panel.layer.wl_surface();
        // Restrict input to the sheep themselves: clicks anywhere else fall
        // through to the window underneath. With dragging off nothing is added
        // to the region, so the overlay stays entirely click-through.
        if let Some(r) = &region {
            surface.set_input_region(Some(r.wl_region()));
        }
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, bw, bh);
        surface.frame(qh, FrameCallbackData(surface.clone()));
        if let Err(e) = buffer.attach_to(surface) {
            eprintln!("hyprsheep: buffer attach failed: {e}");
            return;
        }
        panel.layer.commit();
    }

    fn panel_of(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.panels.iter().position(|p| p.layer.wl_surface() == surface)
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        if let Some(i) = self.panel_of(surface) {
            self.panels[i].scale = new_factor.max(1) as u32;
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        if let Some(i) = self.panel_of(surface) {
            self.draw(qh, i);
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        self.panels.retain(|p| &p.layer != layer);
        // Losing every output means there is nowhere left to draw.
        if self.panels.is_empty() {
            self.exit = true;
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(i) = self.panels.iter().position(|p| &p.layer == layer) else { return };
        let (w, h) = configure.new_size;
        if w != 0 && h != 0 {
            self.panels[i].width = w;
            self.panels[i].height = h;
        }
        self.draw(qh, i);
    }
}

impl SeatHandler for Overlay {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            match self.seat_state.get_pointer(qh, &seat) {
                Ok(p) => self.pointer = Some(p),
                Err(e) => eprintln!("hyprsheep: no pointer: {e}"),
            }
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Overlay {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            // Pointer positions are surface-local; lift them into the global
            // space the sheep live in.
            let Some(i) = self.panel_of(&event.surface) else { continue };
            let origin = self.panels[i].origin;
            let (x, y) = (event.position.0 + origin.0, event.position.1 + origin.1);

            match event.kind {
                PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                    self.pressed = self.sheep_at(x, y);
                }
                PointerEventKind::Motion { .. } => {
                    // As in the original, a click alone does not pick the sheep
                    // up; it takes a press followed by movement.
                    if let Some(i) = self.pressed {
                        if let Some(pen) = self.flock.get_mut(i) {
                            if !pen.sheep.dragging {
                                pen.sheep.grab(&self.pet, &self.world, self.tile);
                                pen.next_step = Instant::now();
                            }
                            pen.sheep.drag_to(x, y, self.tile);
                        }
                    }
                }
                PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                    if let Some(pen) = self.pressed.take().and_then(|i| self.flock.get_mut(i)) {
                        if pen.sheep.dragging {
                            pen.sheep.release();
                        }
                    }
                }
                PointerEventKind::Leave { .. } => self.pressed = None,
                _ => {}
            }
        }
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.refresh_world();
        self.add_panel(qh, output);
    }

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.dirty.store(true, Ordering::Relaxed);
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.panels.retain(|p| p.output != output);
        self.dirty.store(true, Ordering::Relaxed);
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Overlay);

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_dispatch2!(Overlay);
