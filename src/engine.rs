//! The sheep state machine: one `step()` per animation frame.
//!
//! Movement, gravity and collision all come out of the pet data rather than a
//! physics simulation - `<start>`/`<end>` carry per-step velocities that ramp
//! across the sequence, so e.g. falling accelerates because the XML says so.

use std::time::Duration;

use crate::anim::{Action, Animation, Pet, Situation};
use crate::expr::{eval_or, Ctx};

/// A rectangle the sheep can stand on the top edge of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    /// Stable identity, so we can tell "the window moved" from "a different window".
    pub id: u64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn top(&self) -> f64 {
        self.y
    }
    pub fn left(&self) -> f64 {
        self.x
    }
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
}

/// One monitor, placed in the global coordinate space.
#[derive(Clone, Copy, Debug)]
pub struct Screen {
    pub id: i64,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Space claimed by bars: left, top, right, bottom.
    pub reserved: (f64, f64, f64, f64),
}

impl Screen {
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
    /// Global y of the walkable floor: the bottom, minus any bottom bar.
    pub fn floor(&self) -> f64 {
        self.bottom() - self.reserved.3
    }
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
}

/// Everything outside the sheep that it can collide with.
///
/// All coordinates are global: monitors are rectangles laid out in one space,
/// which is what lets a sheep walk off one screen and onto the next.
#[derive(Clone, Debug, Default)]
pub struct World {
    pub screens: Vec<Screen>,
    /// Walkable window edges, highest first.
    pub windows: Vec<Rect>,
}

impl World {
    fn find(&self, id: u64) -> Option<&Rect> {
        self.windows.iter().find(|r| r.id == id)
    }

    /// The screen containing a point, if any.
    pub fn screen_at(&self, x: f64, y: f64) -> Option<&Screen> {
        self.screens.iter().find(|s| s.contains(x, y))
    }

    /// The screen a point belongs to, falling back to the nearest one so the
    /// sheep always has somewhere to be even in a gap between monitors.
    pub fn screen_for(&self, x: f64, y: f64) -> Option<&Screen> {
        self.screen_at(x, y).or_else(|| {
            self.screens.iter().min_by(|a, b| {
                let d = |s: &Screen| {
                    let cx = x.clamp(s.x, s.right());
                    let cy = y.clamp(s.y, s.bottom());
                    (x - cx).powi(2) + (y - cy).powi(2)
                };
                d(a).total_cmp(&d(b))
            })
        })
    }
}

/// Things the host must act on after a step.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// Create an independent companion sheep.
    SpawnChild { animation: u32, x: f64, y: f64 },
    /// This sheep reached a terminal animation and should be removed.
    Died,
}

pub struct Sheep {
    /// Position of the sprite's top-left corner, in logical pixels.
    pub x: f64,
    pub y: f64,
    pub animation: u32,
    pub step: u32,
    pub flipped: bool,
    /// Current tile index into the sheet.
    pub frame: u32,
    /// Vertical draw offset for the current step.
    pub offset_y: f64,
    pub opacity: f64,
    pub dragging: bool,
    /// True for companion sheep, which die instead of respawning.
    pub is_child: bool,

    /// Stable per-sheep personality value in 0..100.
    rand_s: f64,
    /// The window we are standing on, if any.
    resting_on: Option<u64>,
    situation: Situation,
    /// Cached step count for the current animation.
    steps: u32,
}

/// How far past a window's edge the sheep will still step back on. Beyond
/// this it is not a ledge any more: the window moved out from under it.
const NUDGE_REACH: f64 = 40.0;
/// While being dragged the original ignores physics and ticks at a fixed rate.
const DRAG_INTERVAL: Duration = Duration::from_millis(50);

impl Sheep {
    pub fn new(is_child: bool) -> Self {
        Sheep {
            x: 0.0,
            y: 0.0,
            animation: 1,
            step: 0,
            flipped: false,
            frame: 0,
            offset_y: 0.0,
            opacity: 1.0,
            dragging: false,
            is_child,
            rand_s: fastrand::f64() * 100.0,
            resting_on: None,
            situation: Situation::default(),
            steps: 1,
        }
    }

    /// The screen the sheep is currently on.
    pub fn screen(&self, world: &World, tile: f64) -> Screen {
        world
            .screen_for(self.x + tile / 2.0, self.y + tile / 2.0)
            .copied()
            .unwrap_or(Screen {
                id: -1,
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0,
                reserved: (0.0, 0.0, 0.0, 0.0),
            })
    }

    /// Expressions are written for a single screen with its origin at the top
    /// left, so they are evaluated in screen-local coordinates and the results
    /// translated back into the global space.
    fn ctx_on(&self, screen: &Screen, tile: f64) -> Ctx {
        Ctx {
            screen_w: screen.w,
            screen_h: screen.h,
            area_w: screen.w - screen.reserved.0 - screen.reserved.2,
            area_h: screen.h - screen.reserved.3,
            image_w: tile,
            image_h: tile,
            image_x: self.x - screen.x,
            image_y: self.y - screen.y,
            rand_s: self.rand_s,
        }
    }

    fn ctx(&self, world: &World, pet: &Pet, tile: f64) -> Ctx {
        let _ = pet;
        self.ctx_on(&self.screen(world, tile), tile)
    }

    /// Enter `id`, resetting the sequence and spawning any companion it declares.
    fn enter(&mut self, pet: &Pet, world: &World, tile: f64, id: u32, events: &mut Vec<Event>) {
        self.animation = id;
        self.step = 0;
        let ctx = self.ctx(world, pet, tile);
        self.steps = pet.get(id).map(|a| a.steps(&ctx)).unwrap_or(1);

        let screen = self.screen(world, tile);
        for c in pet.children.iter().filter(|c| c.animation_id == id) {
            events.push(Event::SpawnChild {
                animation: c.next,
                // Child coordinates are evaluated against the *parent's* state,
                // in the parent's screen-local space.
                x: screen.x + eval_or(&c.x, &ctx, 0.0),
                y: screen.y + eval_or(&c.y, &ctx, 0.0),
            });
        }
    }

    /// The window the sheep is standing on, if any.
    pub fn resting_on(&self) -> Option<u64> {
        self.resting_on
    }

    /// Start `id` directly, rather than via the spawn table. Used for
    /// companion sheep, whose animation and position the parent dictates.
    pub fn begin(&mut self, pet: &Pet, world: &World, tile: f64, id: u32, events: &mut Vec<Event>) {
        self.enter(pet, world, tile, id, events);
    }

    /// Place the sheep at a weighted-random spawn point and start its animation.
    pub fn spawn(&mut self, pet: &Pet, world: &World, tile: f64, events: &mut Vec<Event>) {
        let Some(spawn) = pet.choose_spawn() else { return };
        let (x, y, next) = (spawn.x.clone(), spawn.y.clone(), spawn.next);
        // Pick a monitor to arrive on, so the sheep turns up on either screen.
        let screen = match world.screens.len() {
            0 => self.screen(world, tile),
            n => world.screens[fastrand::usize(..n)],
        };
        let ctx = self.ctx_on(&screen, tile);
        self.x = screen.x + eval_or(&x, &ctx, 0.0);
        self.y = screen.y + eval_or(&y, &ctx, 0.0);
        self.flipped = false;
        self.resting_on = None;
        self.enter(pet, world, tile, next, events);
    }

    /// Begin a drag: the mouse takes over positioning until released.
    pub fn grab(&mut self, pet: &Pet, world: &World, tile: f64) {
        self.dragging = true;
        self.resting_on = None;
        if let Some(drag) = pet.by_name("drag") {
            let id = drag.id;
            let mut ignored = Vec::new();
            self.enter(pet, world, tile, id, &mut ignored);
        }
    }

    pub fn release(&mut self) {
        self.dragging = false;
        // The drag sequence is already past its end, so the next step resolves
        // its transition immediately and gravity takes over from there.
        self.step = self.steps;
    }

    pub fn drag_to(&mut self, x: f64, y: f64, tile: f64) {
        self.x = x - tile / 2.0;
        self.y = y - tile / 2.0;
    }

    /// Is the sheep's foot line resting on this rectangle's top edge?
    fn stands_on(&self, r: &Rect, tile: f64, sticky: bool) -> bool {
        let feet = self.y + tile;
        let margin = if sticky { 5.0 } else { 20.0 };
        feet > r.top() - 2.0
            && feet < r.top() + margin
            && self.x > r.left()
            && self.x < r.right() - tile
    }

    fn surface_under(&self, world: &World, tile: f64, sticky: bool) -> Option<Rect> {
        world.windows.iter().find(|r| self.stands_on(r, tile, sticky)).copied()
    }

    /// Advance one animation step. Returns how long to wait before the next.
    pub fn step(
        &mut self,
        pet: &Pet,
        world: &World,
        tile: f64,
        events: &mut Vec<Event>,
    ) -> Duration {
        let Some(anim) = pet.get(self.animation).cloned() else {
            events.push(Event::Died);
            return DRAG_INTERVAL;
        };

        self.frame = anim.frame_at(self.step);

        // Dragging freezes physics but keeps the sprite animating.
        if self.dragging {
            self.step += 1;
            return DRAG_INTERVAL;
        }

        let ctx = self.ctx(world, pet, tile);
        let steps = self.steps.max(1);
        // Values ramp linearly from the start endpoint to the end endpoint
        // across the sequence.
        let ramp = |a: &str, b: &str, at: u32| -> f64 {
            let (v1, v2) = (eval_or(a, &ctx, 0.0), eval_or(b, &ctx, 0.0));
            v1 + (v2 - v1) * at as f64 / steps as f64
        };
        let at = self.step;

        let x2 = eval_or(&anim.end.x, &ctx, 0.0);
        let y2 = eval_or(&anim.end.y, &ctx, 0.0);
        let (dx, dy) =
            (ramp(&anim.start.x, &anim.end.x, at), ramp(&anim.start.y, &anim.end.y, at));
        // Flipping mirrors horizontal motion only.
        let sign = if self.flipped { -1.0 } else { 1.0 };
        self.x += (dx * sign).trunc();
        self.y += dy.trunc();

        // The reference parses these and then never applies them; we do.
        self.offset_y = ramp(&anim.start.offset_y, &anim.end.offset_y, at);
        self.opacity = (anim.start.opacity
            + (anim.end.opacity - anim.start.opacity) * at as f64 / steps as f64)
            .clamp(0.0, 1.0);

        self.step += 1;

        let flipped_x2 = x2 * sign;
        let transition = self.resolve(pet, world, tile, &anim, flipped_x2, y2, events);
        if let Some(id) = transition {
            self.enter(pet, world, tile, id, events);
        }

        // The delay uses the post-increment step, as the original does.
        let ms = ramp(&anim.start.interval, &anim.end.interval, self.step);
        Duration::from_millis(ms.max(10.0) as u64)
    }

    /// Decide which animation to move to, if any, after this step's movement.
    fn resolve(
        &mut self,
        pet: &Pet,
        world: &World,
        tile: f64,
        anim: &Animation,
        x2: f64,
        y2: f64,
        events: &mut Vec<Event>,
    ) -> Option<u32> {
        // Revalidate support up front so that every decision below - the
        // `only` filters included - sees whether the sheep is really still on
        // a window, rather than a claim left over from an earlier step.
        let had_support = self.resting_on.is_some();
        let supported_now = self
            .resting_on
            .and_then(|id| world.find(id).copied())
            .is_some_and(|r| self.stands_on(&r, tile, true));

        // 1. Sequence complete.
        if self.step >= self.steps {
            if anim.action == Action::Flip {
                self.flipped = !self.flipped;
            }
            // Let go of a window we are no longer on, so that what comes next
            // is chosen against the truth rather than a stale claim.
            if !supported_now {
                self.resting_on = None;
            }
            self.update_situation(world, tile, supported_now);
            return match pet.choose(&anim.next, self.situation) {
                Some(n) => Some(n.target),
                None => {
                    // Terminal: children die, ordinary sheep start over.
                    if self.is_child {
                        events.push(Event::Died);
                    } else {
                        self.spawn(pet, world, tile, events);
                    }
                    None
                }
            };
        }

        // 2. Borders: screen edges and window tops. Which edge matters depends
        //    on the direction of travel, exactly as the original does it. An
        //    edge with another monitor behind it is not an edge at all - the
        //    sheep walks straight across the seam.
        let mut hit_border = false;
        // Set when the sheep is being stepped back onto a ledge it has just
        // walked off, which counts as still having support this step.
        let mut nudged = false;
        let screen = self.screen(world, tile);
        let floor = screen.floor() - tile;
        let mid_y = self.y + tile / 2.0;
        let mid_x = self.x + tile / 2.0;
        let continues = |x: f64, y: f64| world.screen_at(x, y).is_some();

        if x2 < 0.0 && self.x < screen.x && !continues(self.x - 1.0, mid_y) {
            self.x = screen.x;
            hit_border = true;
            self.situation.on_vertical = true;
        } else if x2 > 0.0
            && self.x > screen.right() - tile
            && !continues(self.x + tile + 1.0, mid_y)
        {
            self.x = screen.right() - tile;
            hit_border = true;
            self.situation.on_vertical = true;
        } else if y2 < 0.0 && self.y < screen.y && !continues(mid_x, self.y - 1.0) {
            self.y = screen.y;
            hit_border = true;
            self.situation.on_horizontal = true;
        } else if y2 > 0.0 && self.y > floor && !continues(mid_x, self.y + tile + 1.0) {
            self.y = floor;
            self.resting_on = None;
            hit_border = true;
        } else if y2 > 0.0 {
            // Descending: look for a window top to land on.
            if let Some(r) = self.surface_under(world, tile, false) {
                // The reference refuses to land while the sheep is within one
                // tile of the top of the screen, to avoid latching onto page
                // elements at the very top. Under a tiling compositor every
                // top-row window sits just below the bar, so that test would
                // make all of them unlandable; require instead that the sheep
                // ends up on screen.
                let landing = r.top().ceil() - tile;
                if landing >= 0.0 {
                    self.y = landing;
                    self.resting_on = Some(r.id);
                    hit_border = true;
                }
            }
        } else if let Some(id) = self.resting_on {
            // Standing on something: has it moved, closed, or have we walked off?
            match world.find(id).copied() {
                None => self.resting_on = None,
                Some(r) => {
                    if !self.stands_on(&r, tile, true) {
                        let feet = self.y + tile;
                        // Which edge are we off, and by how far?
                        let (overrun, step_back) = if self.x <= r.left() {
                            (r.left() - self.x, 3.0)
                        } else {
                            (self.x - (r.right() - tile), -3.0)
                        };
                        if (feet - r.top()).abs() > 3.0 {
                            // The window moved vertically out from under us.
                            self.resting_on = None;
                        } else if overrun > NUDGE_REACH {
                            // Too far past the edge to be a step off it: the
                            // window was dragged out from under us.
                            self.resting_on = None;
                        } else {
                            self.x += step_back;
                            hit_border = true;
                            nudged = true;
                        }
                    }
                }
            }
        }

        if hit_border {
            self.update_situation(world, tile, supported_now || nudged);
            if let Some(n) = pet.choose(&anim.border, self.situation) {
                return Some(n.target);
            }
        }

        // 3. Gravity: airborne with nothing underfoot.
        let supported = nudged || supported_now;
        if !supported {
            self.resting_on = None;
        }

        if !supported && self.y < floor - 2.0 {
            self.update_situation(world, tile, false);
            if let Some(n) = pet.choose(&anim.gravity, self.situation) {
                return Some(n.target);
            }
            // Only four of the original's animations declare any <gravity>, so
            // a sleeping or idling sheep never checks the ground. That is fine
            // in a browser, where nothing moves underneath it; under a tiling
            // compositor windows move constantly, and the sheep would hang in
            // mid-air until its animation happened to end.
            //
            // This applies only to a sheep that just lost real support while
            // in an animation that is not already taking it downwards. An
            // animation that descends under its own steam - falling, dropping
            // off a ledge, climbing down a wall - is already dealing with
            // gravity, and interrupting it would bounce the sheep between
            // falling and landing forever.
            if had_support && y2 <= 0.0 {
                return pet.fall_animation();
            }
        }

        None
    }

    fn update_situation(&mut self, world: &World, tile: f64, on_window: bool) {
        let screen = self.screen(world, tile);
        let floor = screen.floor() - tile;
        self.situation = Situation {
            on_window,
            on_taskbar: self.y >= floor - 2.0,
            on_vertical: self.x <= screen.x || self.x >= screen.right() - tile,
            on_horizontal: self.y <= screen.y || self.y >= floor - 2.0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = include_str!("../assets/animations.xml");
    const TILE: f64 = 40.0;

    fn pet() -> Pet {
        Pet::parse(XML).unwrap()
    }

    fn screen(id: i64, x: f64, w: f64, h: f64) -> Screen {
        Screen { id, x, y: 0.0, w, h, reserved: (0.0, 0.0, 0.0, 0.0) }
    }

    fn world() -> World {
        World { screens: vec![screen(0, 0.0, 1920.0, 1080.0)], windows: vec![] }
    }

    /// Two monitors side by side, the second narrower.
    fn two_screens() -> World {
        World {
            screens: vec![screen(0, 0.0, 1920.0, 1080.0), screen(1, 1920.0, 960.0, 1080.0)],
            windows: vec![],
        }
    }

    /// Run the machine for many steps and make sure it never panics, never
    /// leaves the screen for good, and never gets stuck.
    #[test]
    fn long_run_stays_sane() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);

        let mut visited = std::collections::HashSet::new();
        for _ in 0..200_000 {
            ev.clear();
            let d = s.step(&p, &w, TILE, &mut ev);
            visited.insert(s.animation);
            assert!(d.as_millis() >= 10, "implausible delay {d:?}");
            assert!(s.x.is_finite() && s.y.is_finite(), "position went non-finite");
            assert!(p.get(s.animation).is_some(), "entered unknown animation");
        }
        // A long run should exercise a decent slice of the behaviour graph.
        assert!(visited.len() > 10, "only reached {} animations", visited.len());
    }

    #[test]
    fn falls_when_unsupported_and_lands_on_a_window() {
        let p = pet();
        let mut w = world();
        w.windows.push(Rect { id: 7, x: 200.0, y: 600.0, w: 800.0, h: 400.0 });

        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 400.0;
        s.y = 100.0;
        // Start walking in mid-air; gravity must take over.
        s.begin(&p, &w, TILE, 1, &mut ev);
        let mut landed = false;
        for _ in 0..400 {
            s.step(&p, &w, TILE, &mut ev);
            if s.resting_on == Some(7) {
                landed = true;
                break;
            }
        }
        assert!(landed, "sheep never landed on the window (ended at y={})", s.y);
        // Feet should sit exactly on the window's top edge.
        assert!((s.y + TILE - 600.0).abs() < 1.0, "feet at {}", s.y + TILE);
    }

    /// Under a bar, tiled windows sit only a few pixels down. The sheep must
    /// still be able to land on them.
    #[test]
    fn lands_on_a_window_just_below_a_bar() {
        let p = pet();
        let mut w = world();
        w.windows.push(Rect { id: 3, x: 12.0, y: 42.0, w: 941.0, h: 1026.0 });

        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 500.0;
        s.y = -60.0;
        s.begin(&p, &w, TILE, 5, &mut ev);
        for _ in 0..400 {
            s.step(&p, &w, TILE, &mut ev);
            if s.resting_on == Some(3) {
                assert!((s.y + TILE - 42.0).abs() < 1.0, "feet at {}", s.y + TILE);
                assert!(s.y >= 0.0, "sheep landed off the top of the screen");
                return;
            }
        }
        panic!("sheep fell past a window at y=42 (ended at y={})", s.y);
    }

    /// Only four of the pet's animations declare any `<gravity>`, so a
    /// sleeping or idling sheep must still notice when its window moves away
    /// rather than hanging in mid-air until the animation happens to end.
    #[test]
    fn a_sheep_that_is_not_walking_still_falls() {
        let p = pet();
        for (id, name) in [(15u32, "sleep1a"), (19, "sleep3a"), (39, "top_walk2"), (16, "sleep1b")]
        {
            assert!(p.get(id).unwrap().gravity.is_empty(), "{name} unexpectedly declares gravity");

            let mut w = world();
            w.windows.push(Rect { id: 7, x: 200.0, y: 600.0, w: 800.0, h: 400.0 });
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.x = 400.0;
            s.y = 560.0;
            s.resting_on = Some(7);
            s.begin(&p, &w, TILE, id, &mut ev);

            // The window is dragged off to the right, out from under the sheep.
            w.windows[0].x = 1400.0;

            let mut fell = false;
            for _ in 0..40 {
                s.step(&p, &w, TILE, &mut ev);
                if s.y > 600.0 {
                    fell = true;
                    break;
                }
            }
            assert!(fell, "{name} hung in mid-air at y={} after its window moved", s.y);
            assert_eq!(s.resting_on, None, "{name} still claims to be on the window");
        }
    }

    /// An animation that takes the sheep downwards under its own steam is
    /// already dealing with gravity, so losing support must not yank it into a
    /// fall midway through.
    #[test]
    fn a_descending_animation_is_not_interrupted() {
        let p = pet();
        let fall = p.fall_animation().unwrap();
        // vertical_walk_down climbs down a wall: it descends, declares no
        // <gravity> of its own, and runs for a long time.
        let climbing = 41;
        let a = p.get(climbing).unwrap();
        assert!(a.gravity.is_empty() && a.end.y == "2", "vertical_walk_down changed shape");

        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 400.0;
        s.y = 300.0;
        // It believes it is on a window that is no longer there.
        s.resting_on = Some(99);
        s.begin(&p, &w, TILE, climbing, &mut ev);

        for _ in 0..20 {
            s.step(&p, &w, TILE, &mut ev);
            assert_ne!(s.animation, fall, "climbing down was interrupted by a fall");
        }
        assert_eq!(s.animation, climbing);
    }

    /// The sheep must never carry on walking in mid-air. Whenever it is
    /// unsupported and off the floor in an animation that declares gravity,
    /// it should fall essentially at once.
    #[test]
    fn it_never_walks_in_mid_air() {
        let p = pet();
        let mut w = two_screens();
        w.windows.push(Rect { id: 1, x: 12.0, y: 562.0, w: 851.0, h: 506.0 });
        w.windows.push(Rect { id: 2, x: 877.0, y: 42.0, w: 506.0, h: 1026.0 });

        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);

        let mut airborne_run = 0;
        let mut worst = 0;
        for _ in 0..200_000 {
            s.step(&p, &w, TILE, &mut ev);
            let floor = s.screen(&w, TILE).floor() - TILE;
            let grounded = s.resting_on.is_some() || s.y >= floor - 2.0;
            // Animations that declare gravity are the walking-about ones; the
            // rest are deliberately airborne (falling, jumping, climbing).
            let should_fall = !grounded && !p.get(s.animation).unwrap().gravity.is_empty();
            airborne_run = if should_fall { airborne_run + 1 } else { 0 };
            worst = worst.max(airborne_run);
            assert!(
                airborne_run < 3,
                "walked {airborne_run} steps in mid-air: anim {} ({}) at ({:.0},{:.0}), resting {:?}",
                s.animation,
                p.get(s.animation).unwrap().name,
                s.x,
                s.y,
                s.resting_on
            );
        }
        assert!(worst <= 2, "worst airborne run was {worst}");
    }

    /// Stepping just off a ledge nudges the sheep back on, but a window that
    /// has moved far away is not a ledge: chasing it would drag the sheep
    /// across the screen instead of letting it fall.
    #[test]
    fn it_steps_back_onto_a_ledge_but_does_not_chase_a_moved_window() {
        let p = pet();

        // Barely off the left edge: step back on, stay put.
        let mut w = world();
        w.windows.push(Rect { id: 7, x: 400.0, y: 600.0, w: 800.0, h: 400.0 });
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 390.0;
        s.y = 560.0;
        s.resting_on = Some(7);
        s.begin(&p, &w, TILE, 1, &mut ev);
        for _ in 0..10 {
            s.step(&p, &w, TILE, &mut ev);
        }
        assert_eq!(s.resting_on, Some(7), "nudged off a ledge it had only just left");

        // Far past the edge: the window moved, so let go and fall.
        let mut w = world();
        w.windows.push(Rect { id: 7, x: 400.0, y: 600.0, w: 800.0, h: 400.0 });
        let mut s = Sheep::new(false);
        s.x = 100.0;
        s.y = 560.0;
        s.resting_on = Some(7);
        s.begin(&p, &w, TILE, 1, &mut ev);
        let start_x = s.x;
        for _ in 0..40 {
            s.step(&p, &w, TILE, &mut ev);
        }
        assert_eq!(s.resting_on, None, "still hanging onto a window 300px away");
        assert!(s.y > 560.0, "never started falling (y={})", s.y);
        assert!(s.x < start_x + 40.0, "crept toward the distant window (x={})", s.x);
    }


    #[test]
    fn a_closed_window_drops_the_sheep() {
        let p = pet();
        let mut w = world();
        w.windows.push(Rect { id: 7, x: 200.0, y: 600.0, w: 800.0, h: 400.0 });
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 400.0;
        s.y = 560.0;
        s.resting_on = Some(7);
        s.begin(&p, &w, TILE, 1, &mut ev);

        w.windows.clear();
        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
            if s.resting_on.is_none() && s.y > 600.0 {
                return;
            }
        }
        panic!("sheep kept standing on a window that closed (y={})", s.y);
    }

    #[test]
    fn walks_across_the_seam_onto_the_next_monitor() {
        let p = pet();
        let w = two_screens();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        // On the floor near the right edge of the first monitor, facing right.
        s.x = 1900.0;
        s.y = 1040.0;
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);

        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
            if s.x > 1960.0 {
                assert_eq!(s.screen(&w, TILE).id, 1, "should now be on the second monitor");
                return;
            }
            // It must never be clamped to the inner edge: that edge is not a wall.
            assert!(s.x >= 1880.0 || s.animation != 1, "clamped at the seam (x={})", s.x);
        }
        panic!("never crossed the seam (ended at x={}, anim {})", s.x, s.animation);
    }

    #[test]
    fn the_outer_edge_is_still_a_wall() {
        let p = pet();
        let w = two_screens();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        // Far right of the second monitor, walking further right.
        s.x = 2820.0;
        s.y = 1040.0;
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);

        for _ in 0..400 {
            s.step(&p, &w, TILE, &mut ev);
            assert!(s.x <= 2840.0, "sheep walked off the right of the desktop (x={})", s.x);
        }
    }

    #[test]
    fn a_gap_below_an_adjacent_monitor_is_a_wall() {
        let p = pet();
        // The second monitor is short, so the floor of the first has nothing
        // beside it; the sheep must not walk into the void.
        let w = World {
            screens: vec![screen(0, 0.0, 1920.0, 1080.0), screen(1, 1920.0, 960.0, 540.0)],
            windows: vec![],
        };
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 1900.0;
        s.y = 1040.0;
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);

        for _ in 0..300 {
            s.step(&p, &w, TILE, &mut ev);
            assert!(s.x <= 1880.0, "sheep walked off into the gap (x={})", s.x);
        }
    }

    #[test]
    fn expressions_are_evaluated_per_screen() {
        let p = pet();
        let w = two_screens();
        // Spawning many times must always place the sheep near some monitor,
        // never at a coordinate derived from the wrong one.
        let mut on_second = 0;
        for _ in 0..400 {
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.spawn(&p, &w, TILE, &mut ev);
            let sc = s.screen(&w, TILE);
            // Spawns sit at or just outside an edge of their own monitor.
            assert!(
                s.x >= sc.x - TILE * 2.0 && s.x <= sc.right() + TILE * 2.0,
                "spawned at x={} for monitor at {}..{}",
                s.x,
                sc.x,
                sc.right()
            );
            assert!(s.y <= sc.bottom(), "spawned below monitor bottom: y={}", s.y);
            if sc.id == 1 {
                on_second += 1;
            }
        }
        assert!(on_second > 50, "only {on_second}/400 spawns on the second monitor");
    }

    #[test]
    fn each_screen_has_its_own_floor() {
        let w = World {
            screens: vec![screen(0, 0.0, 1920.0, 1080.0), screen(1, 1920.0, 960.0, 540.0)],
            windows: vec![],
        };
        let mut tall = Sheep::new(false);
        tall.x = 100.0;
        tall.y = 100.0;
        assert_eq!(tall.screen(&w, TILE).floor(), 1080.0);

        let mut short = Sheep::new(false);
        short.x = 2000.0;
        short.y = 100.0;
        assert_eq!(short.screen(&w, TILE).floor(), 540.0);
    }

    /// Starting in any animation must run without panicking or wedging.
    #[test]
    fn every_animation_can_be_entered_and_run() {
        let p = pet();
        let mut w = two_screens();
        w.windows.push(Rect { id: 1, x: 200.0, y: 500.0, w: 700.0, h: 500.0 });
        w.windows.push(Rect { id: 2, x: 1000.0, y: 42.0, w: 600.0, h: 900.0 });

        for id in 1..=54u32 {
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.x = 600.0;
            s.y = 300.0;
            s.begin(&p, &w, TILE, id, &mut ev);
            for _ in 0..3000 {
                ev.clear();
                let d = s.step(&p, &w, TILE, &mut ev);
                assert!(d.as_millis() >= 10, "anim {id}: implausible delay {d:?}");
                assert!(s.x.is_finite() && s.y.is_finite(), "anim {id}: position diverged");
                assert!(p.get(s.animation).is_some(), "anim {id}: entered unknown animation");
                assert!((0.0..=1.0).contains(&s.opacity), "anim {id}: opacity {}", s.opacity);
            }
        }
    }

    /// `drag` is entered only by the mouse, and `kill`/`sync` were driven by
    /// Windows events the original had and we do not. Everything else must be
    /// reachable by simply letting the sheep run.
    #[test]
    fn every_other_animation_is_reachable_from_a_spawn() {
        let p = pet();
        let mut seen = std::collections::HashSet::new();
        let mut stack: Vec<u32> = p.spawns.iter().map(|s| s.next).collect();
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            let a = p.get(id).unwrap();
            stack.extend(a.next.iter().chain(&a.border).chain(&a.gravity).map(|n| n.target));
            stack.extend(p.children.iter().filter(|c| c.animation_id == id).map(|c| c.next));
        }

        let orphans: Vec<&str> = (1..=54u32)
            .filter(|i| !seen.contains(i))
            .map(|i| p.get(i).unwrap().name.as_str())
            .collect();
        assert_eq!(orphans, vec!["drag", "kill", "sync"], "reachability changed");
        assert!(p.by_name("drag").is_some(), "drag must still be reachable by name");
    }

    /// The two rare spawn points start long chains, each of which brings on a
    /// companion. They sit behind weights of 3 in 106, so ordinary play rarely
    /// reaches them.
    #[test]
    fn the_rare_chains_run_their_course() {
        let p = pet();
        let w = world();
        // batha brings on the bathtub and ends by rejoining ordinary life;
        // blacksheepa brings on the second sheep.
        for (start, companion, expected) in
            [(21u32, 23u32, vec![22u32, 47, 48]), (28, 31, vec![29, 30])]
        {
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.x = 800.0;
            s.y = 1040.0;
            s.begin(&p, &w, TILE, start, &mut ev);
            assert!(
                ev.iter().any(|e| matches!(e, Event::SpawnChild { animation, .. }
                    if *animation == companion)),
                "anim {start} should bring on companion {companion}, got {ev:?}"
            );

            let mut visited = std::collections::HashSet::new();
            for _ in 0..40_000 {
                s.step(&p, &w, TILE, &mut ev);
                visited.insert(s.animation);
            }
            for id in expected {
                assert!(visited.contains(&id), "chain from {start} never reached {id}");
            }
        }
    }

    /// Each companion runs its own short chain and then dies. The bathtub is
    /// the clearest case: it fills, then stops for good.
    #[test]
    fn the_bathtub_fills_and_then_finishes() {
        let p = pet();
        let w = world();
        let mut tub = Sheep::new(true);
        let mut ev = Vec::new();
        tub.x = 800.0;
        tub.y = 1040.0;
        tub.begin(&p, &w, TILE, 23, &mut ev);

        let mut visited = std::collections::HashSet::new();
        for _ in 0..40_000 {
            ev.clear();
            tub.step(&p, &w, TILE, &mut ev);
            visited.insert(tub.animation);
            if ev.contains(&Event::Died) {
                assert!(visited.contains(&24), "tub died without reaching bathz");
                return;
            }
        }
        panic!("bathtub never finished (reached {visited:?})");
    }

    /// The companion sheep of the black-sheep encounter must run its own
    /// chain and then die, rather than respawning like an ordinary sheep.
    #[test]
    fn the_companion_sheep_runs_its_chain_and_dies() {
        let p = pet();
        let w = world();
        let mut child = Sheep::new(true);
        let mut ev = Vec::new();
        child.x = 700.0;
        child.y = 1040.0;
        child.begin(&p, &w, TILE, 31, &mut ev);

        let mut visited = std::collections::HashSet::new();
        for _ in 0..20_000 {
            ev.clear();
            child.step(&p, &w, TILE, &mut ev);
            visited.insert(child.animation);
            if ev.contains(&Event::Died) {
                assert!(visited.len() > 1, "died without running its chain");
                return;
            }
        }
        panic!("companion never finished (reached {visited:?})");
    }

    #[test]
    fn dragging_freezes_physics() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);
        s.grab(&p, &w, TILE);
        assert_eq!(p.get(s.animation).unwrap().name, "drag");

        s.drag_to(500.0, 500.0, TILE);
        let (x, y) = (s.x, s.y);
        for _ in 0..50 {
            assert_eq!(s.step(&p, &w, TILE, &mut ev), DRAG_INTERVAL);
        }
        assert_eq!((s.x, s.y), (x, y), "position moved while dragged");

        s.release();
        s.step(&p, &w, TILE, &mut ev);
        assert_ne!(p.get(s.animation).unwrap().name, "drag", "drag never ended");
    }

    #[test]
    fn children_are_spawned_and_die() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        // Animation 26 (eat) declares a companion flower.
        s.begin(&p, &w, TILE, 26, &mut ev);
        assert!(
            matches!(ev.first(), Some(Event::SpawnChild { animation: 27, .. })),
            "eat should spawn the flower, got {ev:?}"
        );

        // The flower is terminal, so a child running it must report Died.
        let mut child = Sheep::new(true);
        ev.clear();
        child.begin(&p, &w, TILE, 27, &mut ev);
        for _ in 0..5000 {
            child.step(&p, &w, TILE, &mut ev);
            if ev.contains(&Event::Died) {
                return;
            }
        }
        panic!("terminal child never died");
    }
}
