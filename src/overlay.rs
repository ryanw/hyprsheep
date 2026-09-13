//! Fullscreen, click-through wlr-layer-shell overlay that the sheep is drawn on.

use std::time::Instant;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, FrameCallbackData, Region},
    delegate_registry,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
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
    protocol::{wl_output, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use crate::sprites::{Frame, Sheet, TILE};

/// The original runs its animations at roughly ten frames a second.
const FRAME_MS: u128 = 100;

pub struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    compositor: CompositorState,

    /// Logical size of the overlay, as configured by the compositor.
    width: u32,
    height: u32,
    /// Output scale factor; the shm buffer is this many times larger.
    scale: u32,
    configured: bool,
    pub exit: bool,

    sheet: Sheet,
    /// Placeholder walk cycle until the real animation set is wired up.
    cycle: Vec<Frame>,
    step: usize,
    last_step: Instant,
    /// Sheep position in logical pixels, top-left of the sprite.
    x: f32,
    y: f32,
}

pub fn run(sheet: Sheet) -> Result<(), String> {
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

    // An empty input region makes every click fall through to whatever is
    // underneath. Grabbing the sheep with the mouse comes later and will need
    // this region narrowed to the sprite instead of emptied.
    let empty = Region::new(&compositor).map_err(|e| format!("wl_region: {e}"))?;
    layer.wl_surface().set_input_region(Some(empty.wl_region()));

    layer.commit();

    let pool = SlotPool::new(256 * 256 * 4, &shm).map_err(|e| format!("shm pool: {e}"))?;

    // Placeholder: the first few tiles of the top row read as a walk cycle.
    // Phase 2 replaces this with the parsed animation set.
    let cycle = (0..4).map(|c| Frame::cell(c, 0)).collect();

    let mut overlay = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
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
        cycle,
        step: 0,
        last_step: Instant::now(),
        x: 40.0,
        y: 40.0,
    };

    loop {
        queue.blocking_dispatch(&mut overlay).map_err(|e| format!("dispatch: {e}"))?;
        if overlay.exit {
            return Ok(());
        }
    }
}

impl Overlay {
    /// Advance the placeholder walk. Real behaviour arrives with the state machine.
    fn tick(&mut self) {
        if self.last_step.elapsed().as_millis() < FRAME_MS {
            return;
        }
        self.last_step = Instant::now();
        self.step = (self.step + 1) % self.cycle.len();

        self.x += 4.0;
        if self.x > self.width as f32 {
            self.x = -(TILE as f32);
        }
        // Sit on the bottom edge so there is something obviously "ground"-like
        // to look at before real window collision exists.
        self.y = self.height.saturating_sub(TILE) as f32;
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        if self.width == 0 || self.height == 0 {
            return;
        }
        self.tick();

        let scale = self.scale;
        let bw = (self.width * scale) as i32;
        let bh = (self.height * scale) as i32;
        let stride = bw * 4;

        let (buffer, canvas) = match self.pool.create_buffer(bw, bh, stride, wl_shm::Format::Argb8888)
        {
            Ok(v) => v,
            Err(e) => {
                eprintln!("hyprsheep: buffer alloc failed: {e}");
                return;
            }
        };

        // Fully transparent everywhere except the sheep.
        canvas.fill(0);

        let frame = self.cycle[self.step];
        let ox = (self.x.round() as i32) * scale as i32;
        let oy = (self.y.round() as i32) * scale as i32;

        // Nearest-neighbour upscale by the output scale keeps the pixel art
        // crisp on HiDPI rather than letting the compositor blur it.
        for sy in 0..TILE {
            for sx in 0..TILE {
                let [r, g, b, a] = self.sheet.pixel(frame.x + sx, frame.y + sy);
                if a == 0 {
                    continue;
                }
                // wl_shm ARGB8888 expects premultiplied alpha.
                let pm = |c: u8| ((c as u32 * a as u32) / 255) as u8;
                let argb =
                    u32::from_le_bytes([pm(b), pm(g), pm(r), a]).to_le_bytes();

                for dy in 0..scale as i32 {
                    let py = oy + sy as i32 * scale as i32 + dy;
                    if py < 0 || py >= bh {
                        continue;
                    }
                    for dx in 0..scale as i32 {
                        let px = ox + sx as i32 * scale as i32 + dx;
                        if px < 0 || px >= bw {
                            continue;
                        }
                        let i = ((py * bw + px) * 4) as usize;
                        canvas[i..i + 4].copy_from_slice(&argb);
                    }
                }
            }
        }

        let surface = self.layer.wl_surface();
        surface.set_buffer_scale(scale as i32);
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
        // Re-assert the empty input region; a configure can reset it.
        if let Ok(empty) = Region::new(&self.compositor) {
            self.layer.wl_surface().set_input_region(Some(empty.wl_region()));
        }
        if !self.configured {
            self.configured = true;
            println!("hyprsheep: overlay {}x{} logical", self.width, self.height);
        }
        self.draw(qh);
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
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_dispatch2!(Overlay);
