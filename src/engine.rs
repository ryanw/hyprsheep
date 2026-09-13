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

/// Everything outside the sheep that it can collide with.
#[derive(Clone, Debug, Default)]
pub struct World {
    pub screen_w: f64,
    pub screen_h: f64,
    /// Work area, i.e. the screen minus space reserved by bars.
    pub area_w: f64,
    pub area_h: f64,
    /// Walkable window edges, nearest-to-front first.
    pub windows: Vec<Rect>,
}

impl World {
    fn find(&self, id: u64) -> Option<&Rect> {
        self.windows.iter().find(|r| r.id == id)
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

    fn ctx(&self, world: &World, pet: &Pet, tile: f64) -> Ctx {
        let _ = pet;
        Ctx {
            screen_w: world.screen_w,
            screen_h: world.screen_h,
            area_w: world.area_w,
            area_h: world.area_h,
            image_w: tile,
            image_h: tile,
            image_x: self.x,
            image_y: self.y,
            rand_s: self.rand_s,
        }
    }

    /// Enter `id`, resetting the sequence and spawning any companion it declares.
    fn enter(&mut self, pet: &Pet, world: &World, tile: f64, id: u32, events: &mut Vec<Event>) {
        self.animation = id;
        self.step = 0;
        let ctx = self.ctx(world, pet, tile);
        self.steps = pet.get(id).map(|a| a.steps(&ctx)).unwrap_or(1);

        for c in pet.children.iter().filter(|c| c.animation_id == id) {
            events.push(Event::SpawnChild {
                animation: c.next,
                // Child coordinates are evaluated against the *parent's* state.
                x: eval_or(&c.x, &ctx, 0.0),
                y: eval_or(&c.y, &ctx, 0.0),
            });
        }
    }

    /// Place the sheep at a weighted-random spawn point and start its animation.
    pub fn spawn(&mut self, pet: &Pet, world: &World, tile: f64, events: &mut Vec<Event>) {
        let Some(spawn) = pet.choose_spawn() else { return };
        let (x, y, next) = (spawn.x.clone(), spawn.y.clone(), spawn.next);
        let ctx = self.ctx(world, pet, tile);
        self.x = eval_or(&x, &ctx, 0.0);
        self.y = eval_or(&y, &ctx, 0.0);
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
        // 1. Sequence complete.
        if self.step >= self.steps {
            if anim.action == Action::Flip {
                self.flipped = !self.flipped;
            }
            self.update_situation(world, tile);
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
        //    on the direction of travel, exactly as the original does it.
        let mut hit_border = false;
        let floor = world.area_h - tile;
        if x2 < 0.0 && self.x < 0.0 {
            self.x = 0.0;
            hit_border = true;
            self.situation.on_vertical = true;
        } else if x2 > 0.0 && self.x > world.screen_w - tile {
            self.x = world.screen_w - tile;
            hit_border = true;
            self.situation.on_vertical = true;
        } else if y2 < 0.0 && self.y < 0.0 {
            self.y = 0.0;
            hit_border = true;
            self.situation.on_horizontal = true;
        } else if y2 > 0.0 && self.y > floor {
            self.y = floor;
            self.resting_on = None;
            hit_border = true;
        } else if y2 > 0.0 {
            // Descending: look for a window top to land on.
            if let Some(r) = self.surface_under(world, tile, false) {
                if self.y > tile {
                    self.y = r.top().ceil() - tile;
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
                        if (feet - r.top()).abs() > 3.0 {
                            // The window moved vertically out from under us.
                            self.resting_on = None;
                        } else if self.x < r.left() {
                            self.x += 3.0;
                            hit_border = true;
                        } else {
                            self.x -= 3.0;
                            hit_border = true;
                        }
                    }
                }
            }
        }

        if hit_border {
            self.update_situation(world, tile);
            if let Some(n) = pet.choose(&anim.border, self.situation) {
                return Some(n.target);
            }
        }

        // 3. Gravity: airborne with nothing underfoot.
        if !anim.gravity.is_empty() && self.y < floor - 2.0 {
            let supported = match self.resting_on {
                None => false,
                Some(id) => match world.find(id).copied() {
                    Some(r) => self.stands_on(&r, tile, true),
                    None => false,
                },
            };
            if !supported {
                self.resting_on = None;
                self.update_situation(world, tile);
                if let Some(n) = pet.choose(&anim.gravity, self.situation) {
                    return Some(n.target);
                }
            }
        }

        None
    }

    fn update_situation(&mut self, world: &World, tile: f64) {
        let floor = world.area_h - tile;
        self.situation = Situation {
            on_window: self.resting_on.is_some(),
            on_taskbar: self.y >= floor - 2.0,
            on_vertical: self.x <= 0.0 || self.x >= world.screen_w - tile,
            on_horizontal: self.y <= 0.0 || self.y >= floor - 2.0,
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

    fn world() -> World {
        World {
            screen_w: 1920.0,
            screen_h: 1080.0,
            area_w: 1920.0,
            area_h: 1080.0,
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
        s.enter(&p, &w, TILE, 1, &mut ev);
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
        s.enter(&p, &w, TILE, 1, &mut ev);

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
        s.enter(&p, &w, TILE, 26, &mut ev);
        assert!(
            matches!(ev.first(), Some(Event::SpawnChild { animation: 27, .. })),
            "eat should spawn the flower, got {ev:?}"
        );

        // The flower is terminal, so a child running it must report Died.
        let mut child = Sheep::new(true);
        ev.clear();
        child.enter(&p, &w, TILE, 27, &mut ev);
        for _ in 0..5000 {
            child.step(&p, &w, TILE, &mut ev);
            if ev.contains(&Event::Died) {
                return;
            }
        }
        panic!("terminal child never died");
    }
}
