//! Fullscreen, click-through wlr-layer-shell overlay that the sheep live on.

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
use crate::engine::{Event, Sheep, World};
use crate::hypr;
use crate::sprites::Sheet;

/// How often to re-read the window layout even without an event, so that
/// interactive drags and resizes are followed smoothly.
const REFRESH: Duration = Duration::from_millis(200);
/// Linux input code for the left mouse button.
const BTN_LEFT: u32 = 0x110;
/// Guard against a burst of catch-up steps after the compositor stalls us.
const MAX_STEPS_PER_FRAME: usize = 8;

struct Pen {
    sheep: Sheep,
    next_step: Instant,
    /// Last (animation, step) reported by the trace, so that re-entering the
    /// same animation still logs.
    traced: (u32, u32),
}

pub struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    seat_state: SeatState,
    pointer: Option<wl_pointer::WlPointer>,
    /// Index of the sheep the pointer is pressed on, if any.
    pressed: Option<usize>,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    compositor: CompositorState,

    /// Logical size of the overlay, as configured by the compositor.
    width: u32,
    height: u32,
    /// Output scale; the shm buffer is this many times larger.
    scale: u32,
    configured: bool,
    pub exit: bool,

    sheet: Sheet,
    pet: Pet,
    tile: f64,
    flock: Vec<Pen>,

    monitor: hypr::Monitor,
    world: World,
    dirty: Arc<AtomicBool>,
    last_refresh: Instant,
    /// Log every animation change; set HYPRSHEEP_TRACE=1.
    trace: bool,
}

pub fn run(sheet: Sheet, pet: Pet) -> Result<(), String> {
    let (monitor, world) = hypr::current()?;
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

    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("hyprsheep"), None);

    // Anchoring to all four edges with a zero size asks the compositor for the
    // full output. A negative exclusive zone means bars don't push us around,
    // so the sheep can reach every pixel of the screen.
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, 0);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);

    // Start fully click-through; once there are sheep on screen the region is
    // narrowed to just their sprites so everything else still falls through.
    let empty = Region::new(&compositor).map_err(|e| format!("wl_region: {e}"))?;
    layer.wl_surface().set_input_region(Some(empty.wl_region()));

    layer.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).map_err(|e| format!("shm pool: {e}"))?;
    let tile = (sheet.width / pet.tiles_x.max(1)) as f64;

    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        pointer: None,
        pressed: None,
        shm,
        pool,
        layer,
        compositor,
        width: 0,
        height: 0,
        scale: 1,
        configured: false,
        exit: false,
        sheet,
        pet,
        tile,
        flock: Vec::new(),
        monitor,
        world,
        dirty,
        last_refresh: Instant::now(),
        trace: std::env::var_os("HYPRSHEEP_TRACE").is_some(),
    };

    overlay.add_sheep(false, None);

    loop {
        queue.blocking_dispatch(&mut overlay).map_err(|e| format!("dispatch: {e}"))?;
        if overlay.exit {
            return Ok(());
        }
    }
}

impl Overlay {
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

    /// Re-read the window layout when an event says it may have changed, or
    /// periodically to follow an in-progress drag.
    fn refresh_world(&mut self) {
        let due = self.dirty.swap(false, Ordering::Relaxed)
            || self.last_refresh.elapsed() >= REFRESH;
        if !due {
            return;
        }
        self.last_refresh = Instant::now();

        if let Ok(m) = hypr::refresh_monitor(self.monitor.id) {
            self.monitor = m;
        }
        match hypr::snapshot(&self.monitor) {
            Ok(w) => self.world = w,
            Err(e) => eprintln!("hyprsheep: window query failed: {e}"),
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
                let name = self
                    .pet
                    .get(pen.sheep.animation)
                    .map(|a| a.name.as_str())
                    .unwrap_or("?");
                println!(
                    "{:>3} {:<18} at ({:>5.0},{:>5.0}) {}{}",
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

        // The flock should never empty out; a lone sheep respawns itself.
        if self.flock.is_empty() {
            self.add_sheep(false, None);
        }
    }

    /// Restrict input to the sheep themselves: clicks anywhere else fall
    /// through to the window underneath, but the sheep can be picked up.
    fn update_input_region(&mut self) {
        let Ok(region) = Region::new(&self.compositor) else { return };
        let tile = self.tile as i32;
        for pen in &self.flock {
            region.add(
                pen.sheep.x.round() as i32,
                (pen.sheep.y + pen.sheep.offset_y).round() as i32,
                tile,
                tile,
            );
        }
        self.layer.wl_surface().set_input_region(Some(region.wl_region()));
    }

    /// The topmost sheep whose sprite covers a surface-local point.
    fn sheep_at(&self, x: f64, y: f64) -> Option<usize> {
        self.flock.iter().rposition(|pen| {
            let top = pen.sheep.y + pen.sheep.offset_y;
            x >= pen.sheep.x
                && x < pen.sheep.x + self.tile
                && y >= top
                && y < top + self.tile
        })
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        self.tick();

        let scale = self.scale as i32;
        let bw = self.width as i32 * scale;
        let bh = self.height as i32 * scale;
        let stride = bw * 4;

        let (buffer, canvas) =
            match self.pool.create_buffer(bw, bh, stride, wl_shm::Format::Argb8888) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("hyprsheep: buffer alloc failed: {e}");
                    return;
                }
            };

        // Fully transparent everywhere except the sheep.
        canvas.fill(0);

        let tile = self.tile as u32;
        for pen in &self.flock {
            let s = &pen.sheep;
            let frame = s.frame;
            let (col, row) = (frame % self.pet.tiles_x, frame / self.pet.tiles_x);
            let (sx0, sy0) = (col * tile, row * tile);
            let ox = s.x.round() as i32 * scale;
            let oy = (s.y + s.offset_y).round() as i32 * scale;
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

        self.update_input_region();

        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(scale);
        surface.damage_buffer(0, 0, bw, bh);
        surface.frame(qh, FrameCallbackData(surface.clone()));
        if let Err(e) = buffer.attach_to(surface) {
            eprintln!("hyprsheep: buffer attach failed: {e}");
            return;
        }
        self.layer.commit();
    }
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        new_factor: i32,
    ) {
        self.scale = new_factor.max(1) as u32;
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
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(qh);
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
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        if w != 0 && h != 0 {
            self.width = w;
            self.height = h;
        }
        // A configure can reset the input region, so re-assert it.
        self.update_input_region();
        if !self.configured {
            self.configured = true;
            println!(
                "hyprsheep: overlay {}x{} logical, floor {}, {} windows",
                self.width,
                self.height,
                self.world.area_h,
                self.world.windows.len()
            );
        }
        self.draw(qh);
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
            let (x, y) = event.position;
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
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
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
