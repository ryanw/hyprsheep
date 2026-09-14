//! The sheep state machine: one `step()` per animation frame.
//!
//! Movement, gravity and collision all come out of the pet data rather than a
//! physics simulation - `<start>`/`<end>` carry per-step velocities that ramp
//! across the sequence, so e.g. falling accelerates because the XML says so.

use std::time::Duration;

use crate::anim::{Action, Animation, Only, Pet, Situation};
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
    pub fn bottom(&self) -> f64 {
        self.y + self.h
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

/// What the sheep looks like at one instant: where it is and how solid.
///
/// The engine moves in discrete hops - two pixels, then nothing for a tenth
/// of a second - so a sheep is kept as two of these, the pose it stepped from
/// and the pose it stepped to, and drawn somewhere between the two.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pose {
    pub x: f64,
    pub y: f64,
    pub offset_y: f64,
    pub opacity: f64,
}

impl Pose {
    /// Blend towards `to`, with `t` running 0 (here) to 1 (arrived).
    fn lerp(self, to: Pose, t: f64) -> Pose {
        let at = |a: f64, b: f64| a + (b - a) * t;
        Pose {
            x: at(self.x, to.x),
            y: at(self.y, to.y),
            offset_y: at(self.offset_y, to.offset_y),
            opacity: at(self.opacity, to.opacity),
        }
    }
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
    /// How big the sheep is drawn, as a multiple of the sprite's own size.
    /// Motion is scaled with it, so a big sheep does not crawl.
    pub scale: f64,
    /// How fast the sheep lives, as a multiple of its natural pace. It shows
    /// up as the wait between steps, so animation and movement speed up
    /// together and what the sheep chooses to do is untouched.
    pub speed: f64,
    /// Whether window sides are solid. Off, windows are ledges only and the
    /// sheep walks straight through their sides, as the reference does; on, it
    /// bumps into them and can climb them the way it climbs a screen edge.
    pub climb_windows: bool,

    /// The pose the current step set out from. Equal to the current pose
    /// whenever the sheep was placed rather than moved, which is what stops a
    /// spawn or a drag from being drawn as a glide across the screen.
    prev: Pose,

    /// Stable per-sheep personality value in 0..100.
    rand_s: f64,
    /// The window we are standing on, if any.
    resting_on: Option<u64>,
    /// A window whose side the sheep declined to climb or turn at, and is
    /// walking through. Held until it is clear of that window, so the choice
    /// is made once per encounter rather than re-rolled every step.
    passing: Option<u64>,
    situation: Situation,
    /// Cached step count for the current animation.
    steps: u32,
}

/// How far past a window's edge the sheep will still step back on. Beyond
/// this it is not a ledge any more: the window moved out from under it.
const NUDGE_REACH: f64 = 40.0;
/// How far past a window's side the sheep may be and still be turned back by
/// it. Beyond this it did not walk into the side: it was already inside the
/// window - dropped there, or the window opened around it - and walks out.
/// A scaled sheep covers more ground per step, so its reach grows with it.
const SIDE_REACH: f64 = 20.0;
/// How often a window's side that is not being climbed turns the sheep back.
/// The rest of the time it is walked through, as it would be with the sides
/// left open. Screen edges are unaffected: those are always walls.
const TURN_AT_FACE: f64 = 0.2;
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
            scale: 1.0,
            speed: 1.0,
            climb_windows: false,
            prev: Pose { x: 0.0, y: 0.0, offset_y: 0.0, opacity: 1.0 },
            rand_s: fastrand::f64() * 100.0,
            resting_on: None,
            passing: None,
            situation: Situation::default(),
            steps: 1,
        }
    }

    /// Where the sheep has stepped to.
    pub fn pose(&self) -> Pose {
        Pose { x: self.x, y: self.y, offset_y: self.offset_y, opacity: self.opacity }
    }

    /// The pose to draw `t` of the way through the current step, with `t`
    /// running 0 at the moment the step was taken to 1 when the next is due.
    /// At `t = 1` this is exactly [`pose`](Self::pose), so a host that does
    /// not interpolate simply passes 1 and gets the stepped motion back.
    pub fn pose_at(&self, t: f64) -> Pose {
        self.prev.lerp(self.pose(), t.clamp(0.0, 1.0))
    }

    /// Whether there is any ground between the last pose and this one - that
    /// is, whether drawing the sheep again before its next step would show
    /// anything different.
    pub fn gliding(&self) -> bool {
        self.prev != self.pose()
    }

    /// Declare the sheep to be where it is, with nothing to glide across.
    /// Used wherever it is placed outright rather than moved: a spawn, a
    /// drag, a companion set down by its parent.
    fn settle(&mut self) {
        self.prev = self.pose();
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

    /// The window the sheep is actually standing on.
    ///
    /// The stored reference is deliberately kept for a moment after the sheep
    /// steps off an edge, so it can be nudged back on; this reports only real
    /// support.
    pub fn standing_on(&self, world: &World, tile: f64) -> Option<u64> {
        let id = self.resting_on?;
        let r = world.find(id)?;
        self.stands_on(r, tile, true).then_some(id)
    }

    /// Start `id` directly, rather than via the spawn table. Used for
    /// companion sheep, whose animation and position the parent dictates.
    pub fn begin(&mut self, pet: &Pet, world: &World, tile: f64, id: u32, events: &mut Vec<Event>) {
        self.enter(pet, world, tile, id, events);
        self.settle();
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
        self.settle();
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
        // The pointer is the position; trailing behind it would feel broken.
        self.settle();
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

    /// Is the sheep's body alongside this window's face, rather than resting on
    /// its top edge? Standing on a ledge puts the feet exactly on `top`, which
    /// must not count as being inside the window.
    fn beside(&self, r: &Rect, tile: f64) -> bool {
        self.climb_windows && self.y + tile > r.top() + 2.0 && self.y < r.bottom()
    }

    /// Walking into a window's side: the window, and the x the sheep is held
    /// at flush against the face it hit. `dir` is the direction of travel.
    ///
    /// Only a face the sheep has just crossed stops it, so one that starts out
    /// within a window - dropped in, or the window opened around it - walks out
    /// rather than being shoved to the nearest edge. A face it has decided to
    /// walk through is not there either, until it is clear of that window.
    fn window_side(&self, world: &World, tile: f64, dir: f64) -> Option<(u64, f64)> {
        let faces = world
            .windows
            .iter()
            .filter(|r| self.beside(r, tile) && Some(r.id) != self.passing)
            .filter_map(|r| {
                let (edge, past) = if dir < 0.0 {
                    (r.right(), r.right() - self.x)
                } else {
                    (r.left() - tile, self.x + tile - r.left())
                };
                (past > 0.0 && past <= SIDE_REACH * self.scale).then_some((r.id, edge))
            });
        // Several windows can overlap the sheep; the one that stops it is the
        // one furthest back along the way it came.
        match dir {
            d if d < 0.0 => faces.max_by(|a, b| a.1.total_cmp(&b.1)),
            d if d > 0.0 => faces.min_by(|a, b| a.1.total_cmp(&b.1)),
            _ => None,
        }
    }

    /// Is the sheep pressed against a window's face, having been stopped by it?
    fn on_window_side(&self, world: &World, tile: f64) -> bool {
        world.windows.iter().any(|r| {
            self.beside(r, tile)
                && ((self.x - r.right()).abs() < 2.0 || (self.x + tile - r.left()).abs() < 2.0)
        })
    }

    /// Climbing a window's face and about to pass its top: the sheep tops out
    /// onto the ledge instead of carrying on up through the air above it.
    fn window_crest(&self, world: &World, tile: f64, dir: f64) -> Option<Rect> {
        if dir >= 0.0 || !self.climb_windows {
            return None;
        }
        let feet = self.y + tile;
        world
            .windows
            .iter()
            .find(|r| {
                self.y < r.bottom()
                    && feet <= r.top() + 2.0
                    && feet > r.top() - SIDE_REACH * self.scale
                    && ((self.x - r.right()).abs() < 2.0 || (self.x + tile - r.left()).abs() < 2.0)
            })
            .copied()
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
        self.prev = self.pose();

        // Dragging freezes physics but keeps the sprite animating, at the
        // sheep's own pace like every other animation.
        if self.dragging {
            self.step += 1;
            return self.wait(DRAG_INTERVAL.as_millis() as f64);
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
        self.x += (dx * sign * self.scale).trunc();
        self.y += (dy * self.scale).trunc();

        // The reference parses these and then never applies them; we do.
        self.offset_y = ramp(&anim.start.offset_y, &anim.end.offset_y, at) * self.scale;
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
        self.wait(ms)
    }

    /// How long to hold a step for, hurried or dawdled by `speed`. The floor
    /// is what stops a fast sheep from spinning the event loop.
    fn wait(&self, ms: f64) -> Duration {
        Duration::from_millis((ms / self.speed.max(f64::MIN_POSITIVE)).max(10.0) as u64)
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

        // A window whose side was walked through is solid again once the sheep
        // is clear of it.
        if let Some(id) = self.passing {
            let inside = world
                .find(id)
                .is_some_and(|r| self.x + tile > r.left() && self.x < r.right());
            if !inside {
                self.passing = None;
            }
        }

        // What a window's side does is settled before the sheep is held
        // against it, because walking through means there was no border here
        // at all. The pet's own table decides whether this is a climb; if it
        // is not, the side turns the sheep back only now and then, and is
        // otherwise walked through as it would be with the sides left open.
        let mut face_stop = None;
        let mut face_next = None;
        if let Some((id, edge)) = self.window_side(world, tile, x2) {
            let stood_at = self.x;
            self.x = edge;
            self.update_situation(world, tile, supported_now);
            let pick = pet.choose(&anim.border, self.situation).map(|n| (n.target, n.only));
            if matches!(pick, Some((_, Only::Vertical))) || fastrand::f64() < TURN_AT_FACE {
                face_stop = Some(edge);
                face_next = pick.map(|(target, _)| target);
            } else {
                self.x = stood_at;
                self.passing = Some(id);
            }
        }

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
        } else if let Some(x) = face_stop {
            // A window's side, which is solid only with `climb_windows` on.
            self.x = x;
            hit_border = true;
            self.situation.on_vertical = true;
        } else if y2 < 0.0 && self.y < screen.y && !continues(mid_x, self.y - 1.0) {
            self.y = screen.y;
            hit_border = true;
            self.situation.on_horizontal = true;
        } else if let Some(r) = self.window_crest(world, tile, y2) {
            // Over the top of the window it was climbing. The sideways step is
            // a tile wide - the sheep was hugging the face, and the ledge only
            // starts a tile in - and lands under the border transition, which
            // for the climb is the animation for coming over an edge.
            self.y = r.top().ceil() - tile;
            self.x = if self.x < r.left() + tile { r.left() + 1.0 } else { r.right() - tile - 1.0 };
            self.resting_on = Some(r.id);
            hit_border = true;
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
                //
                // On screen means inside *this* monitor, not y >= 0: a monitor
                // taller than its neighbour is centred against it and starts
                // at a negative y, which would otherwise make every window in
                // its top band unlandable.
                let landing = r.top().ceil() - tile;
                if landing >= screen.y {
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
            // A face has already drawn from the same table; rolling again here
            // could contradict the choice that held the sheep in the first
            // place.
            if let Some(target) = face_next {
                return Some(target);
            }
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
            on_vertical: self.x <= screen.x
                || self.x >= screen.right() - tile
                || self.on_window_side(world, tile),
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

    /// A scaled sheep covers proportionally more ground, so it walks at the
    /// same apparent speed whatever size it is drawn at.
    #[test]
    fn scale_stretches_the_stride() {
        let p = pet();
        let w = world();
        let step_of = |scale: f64, tile: f64| {
            let mut s = Sheep::new(false);
            s.scale = scale;
            let mut ev = Vec::new();
            s.x = 500.0;
            s.y = 100.0;
            // Animation 1 is `walk`, whose velocity is -2 per step.
            s.begin(&p, &w, tile, 1, &mut ev);
            let before = s.x;
            s.step(&p, &w, tile, &mut ev);
            s.x - before
        };
        assert_eq!(step_of(1.0, TILE), -2.0);
        assert_eq!(step_of(2.0, TILE * 2.0), -4.0);
        assert_eq!(step_of(0.5, TILE / 2.0), -1.0);
    }

    /// Speed is a matter of timing only: the same steps, taken sooner.
    #[test]
    fn speed_shortens_the_wait_without_changing_the_walk() {
        let p = pet();
        let w = world();
        let run = |speed: f64| {
            let mut s = Sheep::new(false);
            s.speed = speed;
            let mut ev = Vec::new();
            s.x = 500.0;
            s.y = 100.0;
            s.begin(&p, &w, TILE, 1, &mut ev);
            let d = s.step(&p, &w, TILE, &mut ev);
            (d, s.x)
        };
        let (slow, slow_x) = run(1.0);
        let (fast, fast_x) = run(2.0);
        assert_eq!(fast * 2, slow, "twice the speed is half the wait");
        assert_eq!(fast_x, slow_x, "the stride itself is unchanged");
        // Even a very fast sheep leaves the event loop time to breathe.
        assert!(run(10.0).0 >= Duration::from_millis(10));
    }

    /// Between two steps the sprite is drawn part-way along, arriving exactly
    /// as the next step falls due.
    #[test]
    fn the_sprite_is_drawn_between_the_steps_it_takes() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 500.0;
        s.y = 100.0;
        // Animation 1 is `walk`, whose velocity is -2 per step.
        s.begin(&p, &w, TILE, 1, &mut ev);
        assert!(!s.gliding(), "a sheep only just placed has nowhere to glide from");

        let from = s.pose();
        s.step(&p, &w, TILE, &mut ev);
        assert!(s.gliding(), "walking should leave something to interpolate");
        assert_eq!(s.pose_at(0.0), from, "at the start of the step, still where it was");
        assert_eq!(s.pose_at(1.0), s.pose(), "at the end of the step, arrived");
        assert_eq!(s.pose_at(0.5).x, from.x - 1.0, "and half a pixel-pair along in between");
        // The host may ask a moment late, or with a zero-length step; neither
        // may throw the sheep past where it was actually going.
        assert_eq!(s.pose_at(2.0), s.pose());
        assert_eq!(s.pose_at(-1.0), from);
    }

    /// A sheep that is put somewhere rather than walking there must simply be
    /// there, not glide across the screen to reach it.
    #[test]
    fn a_sheep_that_is_placed_never_glides_from_where_it_was() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 500.0;
        s.y = 100.0;
        s.begin(&p, &w, TILE, 1, &mut ev);
        s.step(&p, &w, TILE, &mut ev);
        assert!(s.gliding());

        // Respawning drops the sheep at a fresh spawn point.
        s.spawn(&p, &w, TILE, &mut ev);
        assert!(!s.gliding(), "a spawning sheep should appear, not fly in");

        // The pointer is the position: a dragged sheep must not trail it.
        s.grab(&p, &w, TILE);
        s.drag_to(1200.0, 300.0, TILE);
        assert!(!s.gliding(), "a dragged sheep should sit under the pointer");
        assert_eq!(s.pose_at(0.0), s.pose());
    }

    /// Interpolation must cost nothing while the sheep is still: a napping
    /// sheep leaves the host with no reason to redraw between its steps.
    #[test]
    fn a_sheep_that_is_not_moving_has_nothing_to_interpolate() {
        let p = pet();
        let w = world();
        // sleep1b is the long middle of a nap: it neither moves nor fades.
        let id = p.by_name("sleep1b").unwrap().id;
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 500.0;
        s.y = 1040.0;
        s.begin(&p, &w, TILE, id, &mut ev);
        for _ in 0..20 {
            s.step(&p, &w, TILE, &mut ev);
            if s.animation != id {
                break;
            }
            assert!(!s.gliding(), "a sleeping sheep should ask for no frames of its own");
        }
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

    /// The abduction, ported from the green sheep. The sheep screams, a saucer
    /// fades in above it, and it is lifted away; the saucer then leaves.
    #[test]
    fn the_abduction_runs_end_to_end() {
        let p = pet();
        let w = world();
        let scream = p.by_name("scream").unwrap().id;
        let ship = p.by_name("shipa").unwrap().id;

        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 800.0;
        s.y = 1040.0;
        s.begin(&p, &w, TILE, scream, &mut ev);

        // The saucer arrives as a companion, well above the sheep.
        let spawned = ev.iter().find_map(|e| match e {
            Event::SpawnChild { animation, x, y } if *animation == ship => Some((*x, *y)),
            _ => None,
        });
        let (sx, sy) = spawned.expect("scream should bring on the saucer");
        assert_eq!(sx, 800.0, "saucer should be directly above the sheep");
        assert!(sy < s.y - TILE * 3.0, "saucer should be well overhead, got {sy}");

        // The sheep is carried upwards and fades out.
        let mut rose = false;
        for _ in 0..2000 {
            s.step(&p, &w, TILE, &mut ev);
            if p.get(s.animation).unwrap().name == "kill2" && s.y <= 1020.0 {
                rose = true;
                if s.opacity < 0.2 {
                    break;
                }
            }
        }
        assert!(rose, "the sheep was never carried off");
        assert!(s.opacity < 0.2, "the sheep never faded out");

        // kill2 is terminal, so an ordinary sheep starts over rather than dying.
        let mut settled = Sheep::new(false);
        settled.x = 800.0;
        settled.y = 1040.0;
        settled.begin(&p, &w, TILE, p.by_name("kill2").unwrap().id, &mut ev);
        ev.clear();
        for _ in 0..4000 {
            settled.step(&p, &w, TILE, &mut ev);
        }
        assert!(!ev.contains(&Event::Died), "an ordinary sheep should respawn, not die");

        // The saucer fades in, beams, then flies off and is gone.
        let mut tug = Sheep::new(true);
        ev.clear();
        tug.x = 800.0;
        tug.y = 800.0;
        tug.begin(&p, &w, TILE, ship, &mut ev);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..40_000 {
            ev.clear();
            tug.step(&p, &w, TILE, &mut ev);
            seen.insert(p.get(tug.animation).unwrap().name.clone());
            if ev.contains(&Event::Died) {
                for want in ["shipb1", "shipb4", "shipc", "shipc2"] {
                    assert!(seen.contains(want), "saucer skipped {want}: {seen:?}");
                }
                assert!(tug.y < 800.0, "the saucer should leave upwards");
                return;
            }
        }
        panic!("the saucer never left (reached {seen:?})");
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

        // Solid window sides put the sheep up the face of one, which must not
        // become a way of walking about in mid-air either.
        for climb in [false, true] {
            let mut s = Sheep::new(false);
            s.climb_windows = climb;
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
                    "{airborne_run} steps in mid-air: anim {} ({}) at ({:.0},{:.0}), resting {:?}",
                    s.animation,
                    p.get(s.animation).unwrap().name,
                    s.x,
                    s.y,
                    s.resting_on
                );
            }
            assert!(worst <= 2, "worst airborne run was {worst} (climb_windows={climb})");
        }
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

    /// A window standing on the floor, for the sheep to walk into the side of.
    fn world_with_a_tall_window() -> World {
        let mut w = world();
        w.windows.push(Rect { id: 5, x: 200.0, y: 600.0, w: 800.0, h: 480.0 });
        w
    }

    #[test]
    fn a_window_side_stops_the_sheep_only_when_it_is_asked_to() {
        let p = pet();
        let w = world_with_a_tall_window();

        // Walking left along the floor, towards the window's right face: how
        // far in did it get before the face did something about it?
        let walk = |climb: bool| {
            let mut s = Sheep::new(false);
            s.climb_windows = climb;
            let mut ev = Vec::new();
            s.x = 1010.0;
            s.y = 1040.0;
            s.begin(&p, &w, TILE, 1, &mut ev);
            let mut furthest = s.x;
            for _ in 0..200 {
                s.step(&p, &w, TILE, &mut ev);
                furthest = furthest.min(s.x);
            }
            furthest
        };

        // Off, the side is not there at all: the sheep always walks straight
        // in, every time.
        for _ in 0..50 {
            assert!(walk(false) < 900.0, "sheep was stopped by a window it should ignore");
        }

        // On, the face stops it some of the time and is walked through the
        // rest, so both outcomes must show up over a run of approaches.
        let (mut stopped, mut through) = (0, 0);
        for _ in 0..50 {
            if walk(true) >= 1000.0 { stopped += 1 } else { through += 1 }
        }
        assert!(stopped > 0, "a window's side never stopped the sheep at all");
        assert!(through > 0, "a window's side always stopped the sheep");
    }

    /// Which is what makes the climbing animations eligible: `walk` offers
    /// vertical_walk_up on a border, but only for `vertical`.
    #[test]
    fn hitting_a_window_side_counts_as_being_against_a_wall() {
        let p = pet();
        let w = world_with_a_tall_window();
        // A face is walked through more often than not, so approach it until
        // one of the approaches is the kind that stops.
        for attempt in 0..200 {
            let mut s = Sheep::new(false);
            s.climb_windows = true;
            let mut ev = Vec::new();
            s.x = 1004.0;
            s.y = 1040.0;
            s.begin(&p, &w, TILE, 1, &mut ev);

            for _ in 0..4 {
                s.step(&p, &w, TILE, &mut ev);
            }
            if s.x == 1000.0 {
                assert!(s.situation.on_vertical, "a window face did not count as vertical");
                return;
            }
            assert!(attempt < 199, "never once held against the window's right face");
        }
    }

    #[test]
    fn climbing_a_window_side_tops_out_on_its_ledge() {
        let p = pet();
        let w = world_with_a_tall_window();
        // Both faces: the sheep climbs whichever one it walked into.
        for (name, x) in [("right", 1000.0), ("left", 160.0)] {
            let mut s = Sheep::new(false);
            s.climb_windows = true;
            let mut ev = Vec::new();
            s.x = x;
            s.y = 700.0;
            // vertical_walk_up, which is where a border on a face leads.
            s.begin(&p, &w, TILE, 37, &mut ev);

            let mut topped_out = false;
            for _ in 0..80 {
                s.step(&p, &w, TILE, &mut ev);
                if s.resting_on == Some(5) {
                    topped_out = true;
                    break;
                }
            }
            assert!(topped_out, "climbing the {name} face ended at ({}, {})", s.x, s.y);
            assert_eq!(s.y + TILE, 600.0, "{name}: feet are not on the window's top edge");
            assert_eq!(
                s.standing_on(&w, TILE),
                Some(5),
                "{name}: topped out beside the ledge rather than on it, at x={}",
                s.x
            );
        }
    }

    /// Only a face the sheep has just crossed stops it, so one that finds
    /// itself inside a window - dropped there, or the window opened around it -
    /// walks out rather than being shoved to the nearest edge.
    #[test]
    fn a_sheep_already_inside_a_window_is_not_trapped() {
        let w = world_with_a_tall_window();
        let mut s = Sheep::new(false);
        s.climb_windows = true;
        s.y = 1040.0;

        s.x = 600.0;
        assert_eq!(s.window_side(&w, TILE, -2.0), None, "shoved out of a window going left");
        assert_eq!(s.window_side(&w, TILE, 2.0), None, "shoved out of a window going right");

        // Just inside a face, though, is a sheep that walked into it.
        s.x = 995.0;
        assert_eq!(s.window_side(&w, TILE, -2.0), Some((5, 1000.0)));
        s.x = 165.0;
        assert_eq!(s.window_side(&w, TILE, 2.0), Some((5, 160.0)));
        // And a sheep on the ledge above is beside nothing at all.
        s.y = 560.0;
        s.x = 995.0;
        assert_eq!(s.window_side(&w, TILE, -2.0), None, "a ledge is not a face");

        // A scaled sheep takes proportionally longer strides, so what counts
        // as having just crossed a face grows with it.
        s.y = 1040.0;
        s.x = 960.0;
        assert_eq!(s.window_side(&w, TILE, -2.0), None, "walked out of a window it was inside");
        s.scale = 4.0;
        assert_eq!(s.window_side(&w, TILE, -2.0), Some((5, 1000.0)), "a big sheep walked through it");
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

    /// A monitor taller than its neighbour is centred against it, so Hyprland
    /// places it at a negative y. Windows in its top band then have a negative
    /// landing coordinate, which the "is the sheep on screen?" test used to
    /// read as off screen and refuse — leaving the biggest window on the
    /// monitor with no landable top edge at all.
    #[test]
    fn a_window_high_on_a_monitor_above_the_origin_is_landable() {
        let p = pet();
        // DP-2 as Hyprland reports it: 1440x2560 at (3840, -400).
        let w = World {
            screens: vec![Screen {
                id: 0,
                x: 3840.0,
                y: -400.0,
                w: 1440.0,
                h: 2560.0,
                reserved: (0.0, 0.0, 0.0, 0.0),
            }],
            windows: vec![Rect { id: 9, x: 3858.0, y: -26.0, w: 1404.0, h: 1242.0 }],
        };
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 4500.0;
        s.y = -300.0;
        for _ in 0..80 {
            s.step(&p, &w, TILE, &mut ev);
            if s.resting_on == Some(9) {
                assert_eq!(s.y, -66.0, "landed, but not on the window's top edge");
                return;
            }
        }
        panic!("fell straight past a window at y={} (ended at y={})", -26.0, s.y);
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

        for id in 1..=63u32 {
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

        let orphans: Vec<&str> = (1..=63u32)
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
