//! The sheep state machine: one `step()` per animation frame.
//!
//! Movement, gravity and collision all come out of the pet data rather than a
//! physics simulation - `<start>`/`<end>` carry per-step velocities that ramp
//! across the sequence, so e.g. falling accelerates because the XML says so.

use std::sync::atomic::{AtomicU64, Ordering};
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
    /// The workspace this monitor is showing at the moment. A sheep is on
    /// show only while this is the workspace it belongs to.
    pub workspace: i64,
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
    /// Where the sheep are, one square each, including whichever sheep is
    /// being stepped: a sheep picks itself out of the list by id. The host
    /// refreshes it every frame, so it is as current as the drawing is.
    pub flock: Vec<Rect>,
    /// Where the mouse pointer is resting, if it is resting anywhere.
    ///
    /// A pointer in motion is on its way somewhere and is not in here: the
    /// host puts it in the world only once it has held still, and takes it
    /// out again the moment it moves. So the sheep react to a mouse left
    /// alone beside them and not to one crossing the screen, which would look
    /// like being pushed around by something invisible.
    pub pointer: Option<(f64, f64)>,
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
    ///
    /// The workspace is the parent's too, and for a plainer reason: a bathtub
    /// must be on the same workspace as the sheep diving into it.
    ///
    /// `rand_s` is the parent's, not a fresh one: a companion's animations are
    /// written to be timed against the sheep that called it on, and the pet
    /// file times them with `randS`. The bathtub waits out the dive by
    /// counting the steps the dive takes, which is a length the parent's
    /// `randS` decides; with a roll of its own the tub splashes early or late.
    SpawnChild { animation: u32, x: f64, y: f64, rand_s: f64, workspace: Option<i64> },
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
    /// Tells this sheep apart from the rest of the flock, so that it can
    /// leave itself out of the crowd it walks into.
    pub id: u64,
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
    /// Whether this sheep belongs to a workspace at all. Off, it has none and
    /// is on every one of them, which is what the reference does and what a
    /// browser page can do.
    pub workspaces: bool,
    /// The workspace the sheep is on, if it is on one. It is only ever drawn
    /// on a monitor showing this workspace; the rest of the time it carries
    /// on living out of sight.
    pub workspace: Option<i64>,

    /// The pose the current step set out from. Equal to the current pose
    /// whenever the sheep was placed rather than moved, which is what stops a
    /// spawn or a drag from being drawn as a glide across the screen.
    prev: Pose,

    /// Stable per-sheep personality value in 0..100.
    rand_s: f64,
    /// The window we are standing on, if any.
    resting_on: Option<u64>,
    /// Velocity of a throw in flight, in logical pixels per second. Set when
    /// the sheep is let go of with the mouse still moving, and cleared the
    /// moment it hits anything.
    toss: Option<(f64, f64)>,
    /// A window whose side the sheep declined to climb or turn at, and is
    /// walking through. Held until it is clear of that window, so the choice
    /// is made once per encounter rather than re-rolled every step.
    passing: Option<u64>,
    /// A sheep this one has decided to walk on past rather than turn at. Held
    /// until the two are clear of each other, for the same reason.
    passing_sheep: Option<u64>,
    /// The screen the sheep was on as of its last step. Crossing onto
    /// another one is what moves it between workspaces.
    on_screen: i64,
    /// A resting place of the pointer this sheep has already been over to
    /// look at. It stops being interesting once looked at, so that a sheep
    /// does not spend its life pacing around an abandoned mouse; move the
    /// pointer and it is news again.
    met_pointer: Option<(f64, f64)>,
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
/// How often meeting another sheep turns this one back. The rest of the time
/// it walks on past: a sheep is not a wall to another sheep, and a flock that
/// always turned would never mingle.
const TURN_AT_SHEEP: f64 = 0.75;
/// How far off a resting pointer can be and still be worth walking over to
/// look at, in logical pixels. Scaled with the sheep, like every other
/// distance in here.
const POINTER_REACH: f64 = 250.0;
/// How far apart two sheep may be vertically and still be in each other's way,
/// as a fraction of the sprite. Further apart, one is on a ledge above or
/// below the other rather than in front of it.
const MEET_REACH: f64 = 0.5;
/// While being dragged the original ignores physics and ticks at a fixed rate.
const DRAG_INTERVAL: Duration = Duration::from_millis(50);
/// How often a thrown sheep is moved along its arc. The pet file's own steps
/// are far too coarse for one, so a throw runs on its own clock - and unlike
/// every other animation, `speed` does not touch it. A throw is the one thing
/// the sheep does not choose to do: it is the mouse's velocity carried on, and
/// watching a hurried sheep cross the screen faster than it was flung looks
/// wrong in a way that a hurried walk does not.
const TOSS_INTERVAL: Duration = Duration::from_millis(25);
/// Downward acceleration of a thrown sheep, in logical pixels per second
/// squared. Scaled with the sheep, like every other motion in here, so a big
/// one arcs like a small one seen from closer up.
const TOSS_GRAVITY: f64 = 2400.0;
/// What fraction of its speed a thrown sheep keeps each second. It matters
/// mostly sideways: it is what stops a hard flick from sailing the width of
/// three monitors.
const TOSS_DRAG: f64 = 0.7;
/// How fast the mouse must still be moving at the moment it lets go for that
/// to be a throw rather than a drop, in logical pixels per second.
const TOSS_MIN: f64 = 250.0;
/// The fastest a sheep can be thrown, however hard the flick.
const TOSS_MAX: f64 = 2600.0;

/// Hand out the next sheep's identity. Only uniqueness matters.
fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Sheep {
    pub fn new(is_child: bool) -> Self {
        Sheep {
            id: next_id(),
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
            workspaces: false,
            workspace: None,
            prev: Pose { x: 0.0, y: 0.0, offset_y: 0.0, opacity: 1.0 },
            rand_s: fastrand::f64() * 100.0,
            toss: None,
            resting_on: None,
            passing: None,
            passing_sheep: None,
            met_pointer: None,
            on_screen: i64::MIN,
            situation: Situation::default(),
            steps: 1,
        }
    }

    /// Adopt a companion's `randS` from the sheep that called it on, so the
    /// two agree on the lengths their animations are timed against.
    pub fn set_rand_s(&mut self, rand_s: f64) {
        self.rand_s = rand_s;
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
                workspace: -1,
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

    /// The same context measured in strides rather than pixels.
    ///
    /// A repeat count is a number of strides, and a stride grows with the
    /// sheep, so any length a repeat count is worked out from has to be
    /// measured in the sheep's own units. Half a screen is half a screen at
    /// any size, but at scale 2 it is only half as many strides away. Without
    /// this the two sheep of the black-sheep meeting each walk the pixel count
    /// of an unscaled screen *times* the scale, and stride straight past each
    /// other instead of meeting in the middle.
    fn in_strides(&self, ctx: &Ctx) -> Ctx {
        let s = self.scale.max(f64::MIN_POSITIVE);
        Ctx {
            screen_w: ctx.screen_w / s,
            screen_h: ctx.screen_h / s,
            area_w: ctx.area_w / s,
            area_h: ctx.area_h / s,
            image_w: ctx.image_w / s,
            image_h: ctx.image_h / s,
            image_x: ctx.image_x / s,
            image_y: ctx.image_y / s,
            rand_s: ctx.rand_s,
        }
    }

    /// Enter `id`, resetting the sequence and spawning any companion it declares.
    fn enter(&mut self, pet: &Pet, world: &World, tile: f64, id: u32, events: &mut Vec<Event>) {
        self.animation = id;
        self.step = 0;
        let ctx = self.ctx(world, pet, tile);
        self.steps = pet.get(id).map(|a| a.steps(&self.in_strides(&ctx))).unwrap_or(1);

        let screen = self.screen(world, tile);
        for c in pet.children.iter().filter(|c| c.animation_id == id) {
            // Child coordinates are evaluated against the *parent's* state,
            // in the parent's screen-local space.
            let mut x = eval_or(&c.x, &ctx, 0.0);
            // A companion placed beside its parent belongs beside the sprite
            // as drawn: the file can only say which side the unflipped sheep
            // has it on, so a flipped one has it mirrored. Both are a tile
            // wide, so mirroring the two boxes about the parent's centre
            // comes out as simply reflecting x in the parent's own. Without
            // this the flower sprouts behind the sheep's tail whenever it
            // came to eat facing right, which is half the time: eating is
            // reached from the border turn, and it turns at either border.
            if self.flipped && c.beside_parent() {
                x = 2.0 * ctx.image_x - x;
            }
            events.push(Event::SpawnChild {
                animation: c.next,
                x: screen.x + x,
                y: screen.y + eval_or(&c.y, &ctx, 0.0),
                rand_s: self.rand_s,
                workspace: self.workspace,
            });
        }
    }

    /// Whether anyone can see the sheep: whether the screen it is on is
    /// showing the workspace it belongs to.
    ///
    /// A sheep that belongs to no workspace - which is every sheep with
    /// `workspaces = false` - is always on show.
    pub fn shown(&self, world: &World, tile: f64) -> bool {
        match self.workspace {
            None => true,
            Some(ws) => ws == self.screen(world, tile).workspace,
        }
    }

    /// Notice having crossed onto another screen, and join whatever that
    /// screen is showing.
    ///
    /// This is what keeps a sheep in sight as it walks over a seam: two
    /// monitors can be showing different workspaces, and a sheep crossing
    /// between them belongs to the one it has walked onto. It is the other
    /// way back into view for a sheep that is out of sight, besides giving up
    /// on its own workspace and coming out on the current one.
    fn follow_screen(&mut self, world: &World, tile: f64) {
        let screen = self.screen(world, tile);
        if screen.id == self.on_screen {
            return;
        }
        self.on_screen = screen.id;
        if self.workspace.is_some() {
            self.workspace = Some(screen.workspace);
        }
    }

    /// Give up on a workspace nobody is looking at and come out onto the one
    /// they are.
    ///
    /// Only ever called on a sheep that is out of sight, which is what lets
    /// it be moved: it walks in from the near edge of the screen rather than
    /// appearing out of nowhere in the middle of it. Nobody saw where it was,
    /// so nothing is lost by deciding it was over there all along.
    pub fn wander_on(&mut self, pet: &Pet, world: &World, tile: f64) {
        let screen = self.screen(world, tile);
        self.workspace = Some(screen.workspace);
        self.on_screen = screen.id;

        // Without a walk in the pet file there is nothing to walk in with, so
        // the sheep simply steps into view where it stands.
        let Some(walk) = pet.by_name("walk") else { return };
        let id = walk.id;
        let ctx = self.ctx(world, pet, tile);
        let dx = eval_or(&walk.end.x, &ctx, 0.0);

        let from_left = self.x + tile / 2.0 < screen.x + screen.w / 2.0;
        self.x = if from_left { screen.x } else { screen.right() - tile };
        self.y = screen.floor() - tile;
        // Turned to face into the screen rather than straight back off it.
        self.flipped = (dx < 0.0) == from_left;
        self.resting_on = None;
        self.toss = None;
        self.passing = None;
        self.passing_sheep = None;
        self.met_pointer = None;
        let mut ignored = Vec::new();
        self.enter(pet, world, tile, id, &mut ignored);
        self.settle();
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
        // Placed rather than arrived, so this is where it already was: the
        // first step must not read it as having crossed onto a new screen.
        self.on_screen = self.screen(world, tile).id;
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
        // A sheep turns up on whatever the screen it arrives on is showing -
        // which at the very edge of one can be the next screen along, since
        // the spawn points are the file's and reach the whole width of it.
        let arrived = self.screen(world, tile);
        self.on_screen = arrived.id;
        if self.workspaces {
            self.workspace = Some(arrived.workspace);
        }
        self.enter(pet, world, tile, next, events);
        self.settle();
    }

    /// Begin a drag: the mouse takes over positioning until released.
    pub fn grab(&mut self, pet: &Pet, world: &World, tile: f64) {
        self.dragging = true;
        self.resting_on = None;
        self.toss = None;
        if let Some(drag) = pet.by_name("drag") {
            let id = drag.id;
            let mut ignored = Vec::new();
            self.enter(pet, world, tile, id, &mut ignored);
        }
    }

    /// End a drag. `vx` and `vy` are how fast the mouse was still travelling
    /// as it let go, in logical pixels per second; let go of a sheep while
    /// moving and it is thrown rather than dropped.
    pub fn release(
        &mut self,
        pet: &Pet,
        world: &World,
        tile: f64,
        vx: f64,
        vy: f64,
        events: &mut Vec<Event>,
    ) {
        self.dragging = false;
        let speed = vx.hypot(vy);
        if let Some(fall) = pet.fall_animation().filter(|_| speed >= TOSS_MIN) {
            // Keep the direction and only cap the pace, so a hard flick still
            // goes where it was aimed.
            let scale = TOSS_MAX.min(speed) / speed;
            self.toss = Some((vx * scale, vy * scale));
            self.flipped = vx < 0.0;
            self.begin(pet, world, tile, fall, events);
            return;
        }
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

    /// The square this sheep takes up, as the rest of the flock sees it.
    pub fn bounds(&self, tile: f64) -> Rect {
        Rect { id: self.id, x: self.x, y: self.y, w: tile, h: tile }
    }

    /// Are the two sheep at the same height - near enough to be face to face
    /// rather than one of them standing on a ledge over the other?
    fn level_with(&self, r: &Rect, tile: f64) -> bool {
        (self.y - r.top()).abs() < tile * MEET_REACH
    }

    /// Walking into another sheep: that sheep, and the x this one is held at,
    /// nose to nose with it. `dir` is the direction of travel.
    ///
    /// Read the same way as [`window_side`](Self::window_side): only a sheep
    /// just walked into is in the way, so two that start out overlapping -
    /// spawned together, or dropped on one another - walk apart rather than
    /// being shoved aside, and one already being walked past stays passable.
    fn sheep_face(&self, world: &World, tile: f64, dir: f64) -> Option<(u64, f64)> {
        // Out of sight is out of the way: a sheep on a workspace nobody is
        // looking at shares the screen with the flock but not the world. The
        // host leaves it out of the flock for the same reason.
        if !self.shown(world, tile) {
            return None;
        }
        let faces = world
            .flock
            .iter()
            .filter(|r| r.id != self.id && Some(r.id) != self.passing_sheep)
            .filter(|r| self.level_with(r, tile))
            .filter_map(|r| {
                let (edge, past) = if dir < 0.0 {
                    (r.right(), r.right() - self.x)
                } else {
                    (r.left() - tile, self.x + tile - r.left())
                };
                (past > 0.0 && past <= SIDE_REACH * self.scale).then_some((r.id, edge))
            });
        // Whichever of them it ran into first, if it is in a crowd.
        match dir {
            d if d < 0.0 => faces.max_by(|a, b| a.1.total_cmp(&b.1)),
            d if d > 0.0 => faces.min_by(|a, b| a.1.total_cmp(&b.1)),
            _ => None,
        }
    }

    /// The resting pointer this sheep still finds interesting, if any.
    fn pointer(&self, world: &World) -> Option<(f64, f64)> {
        world.pointer.filter(|p| self.met_pointer != Some(*p))
    }

    /// Is a point at the sheep's own height - in front of its face, rather
    /// than over its head or below its feet?
    fn level_at(&self, y: f64, tile: f64) -> bool {
        y > self.y - tile * MEET_REACH && y < self.y + tile * (1.0 + MEET_REACH)
    }

    /// Walking into the resting pointer: the x the sheep is held at, nose to
    /// it. `dir` is the direction of travel.
    ///
    /// Read like [`sheep_face`](Self::sheep_face): the pointer has to be
    /// something the sheep has just walked into, so one that comes to rest on
    /// top of a sheep is not in its way and it walks out from under it.
    fn pointer_face(&self, world: &World, tile: f64, dir: f64) -> Option<f64> {
        let (px, py) = self.pointer(world)?;
        if !self.level_at(py, tile) {
            return None;
        }
        let (edge, past) =
            if dir < 0.0 { (px, px - self.x) } else { (px - tile, self.x + tile - px) };
        (past > 0.0 && past <= SIDE_REACH * self.scale).then_some(edge)
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

    /// Climbing a screen edge and about to pass the floor of the monitor
    /// beside it: the sheep steps out onto that monitor rather than carrying
    /// on up a wall that has stopped being one.
    ///
    /// This is what a monitor layout that is not one straight row needs. Where
    /// a tall screen sits beside a short one, the tall screen's edge is solid
    /// only up to the short screen's floor; above that line there is a desktop
    /// to step onto, exactly as the top of a window is a ledge to step onto.
    /// Without it the sheep climbs the whole edge and tops out at the sky,
    /// never finding the screen it spent the climb walking past.
    fn screen_crest(&self, world: &World, tile: f64, dir: f64) -> Option<Screen> {
        if dir >= 0.0 {
            return None;
        }
        let feet = self.y + tile;
        world
            .screens
            .iter()
            .find(|s| {
                // Hugging one of its outer faces, with the feet arriving at
                // its floor line from below. The faces are measured from
                // outside the screen, so the monitor the sheep is climbing
                // inside of can never match its own edge.
                let floor = s.floor();
                self.y < s.bottom()
                    && feet <= floor + 2.0
                    && feet > floor - SIDE_REACH * self.scale
                    && ((self.x - s.right()).abs() < 2.0 || (self.x + tile - s.x).abs() < 2.0)
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
        let wait = self.take_step(pet, world, tile, events);
        // Joining the workspace of the screen it has ended up on belongs
        // here, after the movement rather than before it: a step that has
        // carried the sheep over a seam but not yet onto the new screen's
        // workspace is a step where it cannot be seen at all, and a sheep
        // that blinks out for a step in the middle of a crossing is worse
        // than one that takes a step to notice.
        self.follow_screen(world, tile);
        wait
    }

    /// The step itself: everything the pet file has to say about it.
    fn take_step(
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

        // A sheep in flight moves under its own physics rather than the pet
        // file's: no animation in there describes an arc, so the falling frames
        // are borrowed and the motion is ours. Borders are still resolved from
        // the animation's own tables, so the throw ends in whatever the file
        // says landing looks like.
        if let Some((vx, vy)) = self.toss {
            let dt = TOSS_INTERVAL.as_secs_f64();
            self.x += vx * dt;
            self.y += vy * dt;
            self.offset_y = 0.0;
            self.opacity = 1.0;
            self.step += 1;
            let drag = TOSS_DRAG.powf(dt);
            self.toss = Some((vx * drag, (vy + TOSS_GRAVITY * self.scale * dt) * drag));
            if let Some(id) = self.resolve(pet, world, tile, &anim, vx, vy, events) {
                self.enter(pet, world, tile, id, events);
            }
            return TOSS_INTERVAL;
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
            // A throw that outlasts the falling sequence keeps falling, rather
            // than resolving into something else in mid-air.
            if self.toss.is_some() {
                self.step = 0;
                return None;
            }
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
        //
        //    Windows are ours; the reference only ever had the edges of the
        //    browser. So an animation that declares no `<border>` table is not
        //    asking to be stopped by one, and windows are not there for it:
        //    three of them descend without one - the dive into the bath,
        //    `jump_down` and `fall_winb` - and each has a distance of its own
        //    to cover. Clamped onto a ledge partway down, there is nothing for
        //    the engine to move to, and the animation plays out its remaining
        //    steps sliding along the top of the window. Screen edges and the
        //    floor still stop them: those are the borders the file was written
        //    against.
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
        // A sheep that was walked past is in the way again once the two have
        // come apart - or once it is gone from the flock altogether.
        if let Some(id) = self.passing_sheep {
            let touching = world.flock.iter().any(|r| {
                r.id == id
                    && self.level_with(r, tile)
                    && self.x + tile > r.left()
                    && self.x < r.right()
            });
            if !touching {
                self.passing_sheep = None;
            }
        }

        let mut face_stop = None;
        // A transition settled outside the border table below, either because
        // the roll has already been made or because the table's answer is for
        // a border this is not.
        let mut forced_next = None;
        if let Some((id, edge)) = self.window_side(world, tile, x2).filter(|_| !anim.border.is_empty())
        {
            let stood_at = self.x;
            self.x = edge;
            self.update_situation(world, tile, supported_now);
            let pick = pet.choose(&anim.border, self.situation).map(|n| (n.target, n.only));
            if matches!(pick, Some((_, Only::Vertical))) || fastrand::f64() < TURN_AT_FACE {
                face_stop = Some(edge);
                forced_next = pick.map(|(target, _)| target);
            } else {
                self.x = stood_at;
                self.passing = Some(id);
            }
        }

        // Another sheep in the way is a softer thing than a window: there is
        // nothing to climb and no ledge to peer over, so the reaction is
        // whatever the pet file does on running into something flat - for a
        // walking sheep, turning round. A sheep in flight is not meeting
        // anyone; it is passing through.
        let mut met_at = None;
        let mut met_next = None;
        if face_stop.is_none()
            && self.toss.is_none()
            && let Some((id, edge)) = self.sheep_face(world, tile, x2)
        {
            if fastrand::f64() < TURN_AT_SHEEP {
                met_at = Some(edge);
                met_next = pet.choose(&anim.border, Situation::default()).map(|n| n.target);
            } else {
                self.passing_sheep = Some(id);
            }
        }

        // A pointer left still is a body in the world as well, and met the
        // same way - windows' own caveat included, that an animation with no
        // border table of its own is not asking to be stopped by anything, so
        // a sheep mid-dive is not halted in mid-air by a mouse. Except that
        // the pointer always stops the sheep. A sheep walking
        // on past another is two animals mingling; a mouse parked in front of
        // one is a person asking for its attention, and being ignored four
        // times in five would read as the feature not working.
        if face_stop.is_none()
            && met_at.is_none()
            && self.toss.is_none()
            && !anim.border.is_empty()
            && let Some(edge) = self.pointer_face(world, tile, x2)
        {
            met_at = Some(edge);
            met_next = pet.choose(&anim.border, Situation::default()).map(|n| n.target);
            // Looked at. Until the mouse moves again, walk through it.
            self.met_pointer = world.pointer;
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
        } else if let Some(x) = met_at {
            // Nose to nose with another sheep.
            self.x = x;
            hit_border = true;
        } else if y2 < 0.0 && self.y < screen.y && !continues(mid_x, self.y - 1.0) {
            self.y = screen.y;
            hit_border = true;
            self.situation.on_horizontal = true;
        } else if let Some(s) = self.screen_crest(world, tile, y2) {
            // Up past the floor of the monitor beside this edge. The sideways
            // step is a tile wide, as it is coming over a window's top: the
            // sheep was hugging the face, so a tile clears it.
            let hugging_right = (self.x - s.right()).abs() < 2.0;
            self.y = s.floor() - tile;
            self.x = if hugging_right { s.right() - tile - 1.0 } else { s.x + 1.0 };
            self.resting_on = None;
            hit_border = true;
            // Not the climb's own border transition: that one is the top of
            // the screen, and would flip the sheep onto a ceiling that is not
            // there. This is ground.
            forced_next = pet.landing_animation();
        } else if let Some(r) = self.window_crest(world, tile, y2) {
            // Over the top of the window it was climbing. The sideways step
            // is a tile wide - the sheep was hugging the face, and the ledge
            // only starts a tile in.
            self.y = r.top().ceil() - tile;
            self.x = if self.x < r.left() + tile { r.left() + 1.0 } else { r.right() - tile - 1.0 };
            self.resting_on = Some(r.id);
            hit_border = true;
            forced_next = pet.landing_animation();
        } else if y2 > 0.0 && self.y > floor && !continues(mid_x, self.y + tile + 1.0) {
            self.y = floor;
            self.resting_on = None;
            hit_border = true;
        } else if y2 > 0.0 && !anim.border.is_empty() {
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
            // A throw is over the moment it hits something.
            self.toss = None;
            self.update_situation(world, tile, supported_now || nudged);
            // A face has already drawn from the same table; rolling again here
            // could contradict the choice that held the sheep in the first
            // place.
            if let Some(target) = forced_next.or(met_next) {
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

        // 4. Curiosity: a pointer resting within reach that the sheep is
        //    walking away from turns it round to come and look. Last, so that
        //    a border or a fall this same step wins - what the mouse is doing
        //    is never more urgent than the floor.
        self.notice_pointer(pet, world, tile, anim, x2, y2)
    }

    /// Turn towards a resting pointer, if there is one worth turning for.
    ///
    /// This is ours rather than the file's: the pet format has nothing to say
    /// about the mouse beyond being dragged by it. Only the decision to turn
    /// is an addition, though - the turn itself is whatever the animation's
    /// own border table does, which for a walking sheep is its about-face, so
    /// it comes about the way it comes about at a wall rather than snapping
    /// round on the spot.
    fn notice_pointer(
        &mut self,
        pet: &Pet,
        world: &World,
        tile: f64,
        anim: &Animation,
        x2: f64,
        y2: f64,
    ) -> Option<u32> {
        // Only a sheep travelling along the ground notices. Climbing and
        // falling have a y of their own and are busy; sitting, sleeping and
        // eating do not move and have nowhere to turn to. A companion's
        // course is its parent's business, not the mouse's.
        if self.is_child || self.toss.is_some() || self.dragging {
            return None;
        }
        if x2 == 0.0 || y2 != 0.0 || anim.action == Action::Flip {
            return None;
        }
        let (px, py) = self.pointer(world)?;
        if !self.level_at(py, tile) {
            return None;
        }
        let away = px - (self.x + tile / 2.0);
        // Out of reach, or already close enough to be about to bump into it.
        if away.abs() > POINTER_REACH * self.scale || away.abs() <= SIDE_REACH * self.scale {
            return None;
        }
        // Already on its way there.
        if away.signum() == x2.signum() {
            return None;
        }
        match pet.choose(&anim.border, Situation::default()) {
            Some(n) => Some(n.target),
            None => {
                // Nothing in the file to turn round with: mirror it in place.
                self.flipped = !self.flipped;
                None
            }
        }
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

    /// A monitor showing workspace 1, which is what the sheep in most of
    /// these tests belong to when they belong to one at all.
    fn screen(id: i64, x: f64, w: f64, h: f64) -> Screen {
        Screen { id, x, y: 0.0, w, h, reserved: (0.0, 0.0, 0.0, 0.0), workspace: 1 }
    }

    fn world() -> World {
        World {
            screens: vec![screen(0, 0.0, 1920.0, 1080.0)],
            windows: vec![],
            flock: vec![],
            pointer: None,
        }
    }

    /// Two monitors side by side, the second narrower.
    fn two_screens() -> World {
        World {
            screens: vec![screen(0, 0.0, 1920.0, 1080.0), screen(1, 1920.0, 960.0, 1080.0)],
            windows: vec![],
            flock: vec![],
            pointer: None,
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
            Event::SpawnChild { animation, x, y, .. } if *animation == ship => Some((*x, *y)),
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

    /// A sheep standing still on the floor at `x`, for another to walk into.
    fn world_with_a_sheep_at(x: f64, y: f64) -> World {
        let mut w = world();
        w.flock.push(Rect { id: 77, x, y, w: TILE, h: TILE });
        w
    }

    /// Walk left off `from` for 200 steps and report the furthest it reached.
    fn approach(p: &Pet, w: &World, from: f64) -> f64 {
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = from;
        s.y = 1040.0;
        s.begin(p, w, TILE, 1, &mut ev);
        let mut furthest = s.x;
        for _ in 0..200 {
            s.step(p, w, TILE, &mut ev);
            furthest = furthest.min(s.x);
        }
        furthest
    }

    /// Two monitors side by side showing different workspaces, which is the
    /// whole point of the seam: a sheep crossing it changes which one it is
    /// on, and stays in sight either way.
    fn two_workspaces() -> World {
        let mut w = two_screens();
        w.screens[0].workspace = 1;
        w.screens[1].workspace = 5;
        w
    }

    /// A sheep that belongs to a workspace, placed on the floor at `x`.
    fn sheep_on(ws: i64, x: f64) -> Sheep {
        let mut s = Sheep::new(false);
        s.workspaces = true;
        s.workspace = Some(ws);
        s.x = x;
        s.y = 1040.0;
        s
    }

    #[test]
    fn a_sheep_arrives_on_whatever_its_screen_is_showing() {
        let p = pet();
        let w = two_workspaces();
        for _ in 0..40 {
            let mut s = Sheep::new(false);
            s.workspaces = true;
            let mut ev = Vec::new();
            s.spawn(&p, &w, TILE, &mut ev);
            let screen = s.screen(&w, TILE);
            assert_eq!(
                s.workspace,
                Some(screen.workspace),
                "arrived on screen {} without joining the workspace it shows",
                screen.id
            );
            assert!(s.shown(&w, TILE), "a sheep that has just arrived is not on show");
        }
    }

    /// The reference has no workspaces and neither does a sheep with them
    /// turned off: it belongs to none, and is on all of them.
    #[test]
    fn with_workspaces_off_a_sheep_is_on_every_one() {
        let p = pet();
        let mut w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);
        assert_eq!(s.workspace, None);
        assert!(s.shown(&w, TILE));
        w.screens[0].workspace = 9;
        assert!(s.shown(&w, TILE), "a sheep on no workspace was taken off screen by a switch");
    }

    /// Switching the workspace out from under a sheep takes it off screen
    /// without stopping it: it carries on walking where nobody can see it, as
    /// it does behind a fullscreen window.
    #[test]
    fn a_workspace_switch_takes_the_sheep_off_screen_but_not_out_of_the_world() {
        let p = pet();
        let mut w = world();
        let mut s = sheep_on(1, 900.0);
        let mut ev = Vec::new();
        s.begin(&p, &w, TILE, 1, &mut ev);
        assert!(s.shown(&w, TILE));

        w.screens[0].workspace = 2;
        assert!(!s.shown(&w, TILE), "still on show after the workspace changed under it");

        let before = s.x;
        for _ in 0..40 {
            s.step(&p, &w, TILE, &mut ev);
        }
        assert_ne!(before, s.x, "a sheep out of sight stopped living");
        assert_eq!(s.workspace, Some(1), "it left its own workspace without being asked to");
    }

    #[test]
    fn walking_across_a_seam_joins_the_next_screens_workspace() {
        let p = pet();
        let w = two_workspaces();
        // Far enough back that its middle - which is what decides the screen
        // it is on - is still on the first monitor.
        let mut s = sheep_on(1, 1850.0);
        let mut ev = Vec::new();
        // Facing right, a few steps from the seam.
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);

        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
            // In sight the whole way over: it is the crossing itself that
            // moves it between workspaces, so there is no step where it
            // belongs to the screen it has left.
            assert!(s.shown(&w, TILE), "went out of sight at x={}", s.x);
            if s.screen(&w, TILE).id == 1 {
                assert_eq!(s.workspace, Some(5), "crossed the seam without joining ws 5");
                return;
            }
        }
        panic!("never crossed the seam (ended at x={})", s.x);
    }

    /// The other way round: a sheep nobody can see walks onto the next screen
    /// and is in sight there, having wandered in from the neighbouring one.
    #[test]
    fn a_sheep_out_of_sight_can_walk_onto_a_screen_where_it_is_seen() {
        let p = pet();
        let mut w = two_workspaces();
        // The first monitor has switched away from the sheep's workspace.
        w.screens[0].workspace = 3;
        let mut s = sheep_on(1, 1850.0);
        let mut ev = Vec::new();
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);
        assert!(!s.shown(&w, TILE));

        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
            if s.screen(&w, TILE).id == 1 {
                assert!(s.shown(&w, TILE), "walked onto a screen showing ws 5 and stayed hidden");
                return;
            }
        }
        panic!("never crossed the seam (ended at x={})", s.x);
    }

    #[test]
    fn a_sheep_out_of_sight_comes_out_onto_the_workspace_being_shown() {
        let p = pet();
        let mut w = world();
        w.screens[0].workspace = 2;
        let mut s = sheep_on(1, 900.0);
        let mut ev = Vec::new();
        s.begin(&p, &w, TILE, 1, &mut ev);
        assert!(!s.shown(&w, TILE));

        s.wander_on(&p, &w, TILE);
        assert_eq!(s.workspace, Some(2), "came out onto the wrong workspace");
        assert!(s.shown(&w, TILE), "came out and still could not be seen");
        // In from the side, on the floor, rather than appearing out of nowhere
        // in the middle of the screen.
        let screen = s.screen(&w, TILE);
        assert!(
            s.x == screen.x || s.x == screen.right() - TILE,
            "came out at x={} rather than at an edge",
            s.x
        );
        assert_eq!(s.y, screen.floor() - TILE, "came out somewhere other than the floor");
        // And walking inwards, not straight back off the screen.
        let before = s.x;
        for _ in 0..4 {
            s.step(&p, &w, TILE, &mut ev);
        }
        let inward = if before == screen.x { s.x > before } else { s.x < before };
        assert!(inward, "walked off the edge it came in at (x={before} to {})", s.x);
    }

    /// It comes in from the nearer side, having plausibly been over there.
    #[test]
    fn it_comes_out_at_the_edge_it_was_nearest() {
        let p = pet();
        let mut w = world();
        w.screens[0].workspace = 2;
        for (x, edge) in [(200.0, 0.0), (1700.0, 1920.0 - TILE)] {
            let mut s = sheep_on(1, x);
            let mut ev = Vec::new();
            s.begin(&p, &w, TILE, 1, &mut ev);
            s.wander_on(&p, &w, TILE);
            assert_eq!(s.x, edge, "a sheep at x={x} came out at the far edge");
        }
    }

    /// A sheep nobody can see is not in the way of one that can be seen, and
    /// nor is the flock in its way. The host leaves it out of the flock; this
    /// is the other half, so that it does not turn at sheep it cannot meet.
    #[test]
    fn a_sheep_out_of_sight_is_not_stopped_by_the_flock() {
        let p = pet();
        let mut w = world_with_a_sheep_at(900.0, 1040.0);
        w.screens[0].workspace = 2;
        let mut s = sheep_on(1, 1011.0);
        let mut ev = Vec::new();
        s.begin(&p, &w, TILE, 1, &mut ev);
        let mut furthest = s.x;
        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
            furthest = furthest.min(s.x);
        }
        assert!(furthest < 900.0, "a sheep out of sight was stopped by one it cannot meet");
    }

    #[test]
    fn a_companion_is_on_its_parents_workspace() {
        let p = pet();
        let mut w = world();
        // The parent is out of sight, so its bathtub must be too - a tub on
        // the workspace being looked at would be a tub with no sheep in it.
        w.screens[0].workspace = 2;
        let dive = p.by_name("batha").expect("the pet dives into a bath").id;
        let mut s = sheep_on(1, 900.0);
        s.y = 200.0;
        let mut ev = Vec::new();
        s.begin(&p, &w, TILE, dive, &mut ev);
        let Some(Event::SpawnChild { workspace, .. }) = ev.first() else {
            panic!("the dive did not call on a bathtub: {ev:?}");
        };
        assert_eq!(*workspace, Some(1), "the bathtub was left on another workspace");
    }

    /// A pointer resting at a point, for a sheep to notice.
    fn world_with_the_pointer_at(x: f64, y: f64) -> World {
        World { pointer: Some((x, y)), ..world() }
    }

    /// A sheep walking left into a resting pointer is stopped by it every
    /// time, unlike another sheep, which it walks past one time in four.
    #[test]
    fn a_resting_pointer_always_stops_a_sheep_walking_into_it() {
        let p = pet();
        let w = world_with_the_pointer_at(900.0, 1040.0);
        // Nose to it is the pointer's own x: the sheep's leading edge stops
        // there. Only the first encounter is judged - once it has had its
        // look the spot is open again, which is its own test below - and a
        // run that never gets that far is one where the sheep chose to stop
        // walking on the way, which is its own business.
        let mut reached = 0;
        for _ in 0..80 {
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.x = 1011.0;
            s.y = 1040.0;
            s.begin(&p, &w, TILE, 1, &mut ev);
            for _ in 0..200 {
                s.step(&p, &w, TILE, &mut ev);
                if s.met_pointer.is_some() {
                    reached += 1;
                    break;
                }
                assert!(
                    s.x >= 900.0,
                    "a sheep walked through a pointer resting in front of it, to {}",
                    s.x
                );
            }
        }
        assert!(reached > 0, "no sheep ever walked as far as the pointer");
    }

    /// The same test the flock gets: a thing a whole sprite higher up is
    /// something the sheep is under, not something in front of its face.
    #[test]
    fn a_pointer_over_the_sheeps_head_is_not_in_the_way() {
        let p = pet();
        let w = world_with_the_pointer_at(900.0, 970.0);
        for _ in 0..30 {
            assert!(
                approach(&p, &w, 1011.0) < 900.0,
                "a pointer resting above the sheep stopped it walking below"
            );
        }
    }

    /// A pointer in motion is never in the world at all - the host puts it
    /// there only once it has held still - so an empty pointer must leave the
    /// walk untouched.
    #[test]
    fn a_pointer_the_host_is_not_offering_does_nothing() {
        let p = pet();
        let w = world();
        for _ in 0..30 {
            assert!(approach(&p, &w, 1011.0) < 900.0, "stopped by a pointer that is not there");
        }
    }

    /// Walking away from a resting pointer within reach, the sheep turns
    /// round, comes back, and ends up nose to it.
    #[test]
    fn a_sheep_walking_away_turns_round_to_come_and_look() {
        let p = pet();
        let w = world_with_the_pointer_at(800.0, 1040.0);
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 1000.0;
        s.y = 1040.0;
        // Flipped, `walk` travels right: away from the pointer at 800.
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);

        let mut furthest_right = s.x;
        for _ in 0..300 {
            s.step(&p, &w, TILE, &mut ev);
            furthest_right = furthest_right.max(s.x);
            if s.x == 800.0 {
                assert!(
                    furthest_right < 1000.0 + POINTER_REACH,
                    "the sheep left the pointer's reach before turning back"
                );
                return;
            }
        }
        panic!("walked off and never came back to look: ended at x={}", s.x);
    }

    /// Nothing drags a sheep across the screen from the far side.
    #[test]
    fn a_pointer_out_of_reach_is_not_noticed() {
        let p = pet();
        // Well beyond POINTER_REACH of a sheep starting at 1000.
        let w = world_with_the_pointer_at(100.0, 1040.0);
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 1000.0;
        s.y = 1040.0;
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);
        for _ in 0..20 {
            s.step(&p, &w, TILE, &mut ev);
        }
        assert!(s.x > 1000.0, "a sheep turned back for a pointer far out of reach");
    }

    /// Once it has had its look the spot is old news, and walked through.
    #[test]
    fn a_pointer_the_sheep_has_looked_at_stops_being_interesting() {
        let p = pet();
        let w = world_with_the_pointer_at(900.0, 1040.0);
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 1011.0;
        s.y = 1040.0;
        s.begin(&p, &w, TILE, 1, &mut ev);
        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
        }
        assert_eq!(s.met_pointer, Some((900.0, 1040.0)), "never reached the pointer at all");

        // Set it walking at the same spot again: this time it walks through.
        s.x = 1011.0;
        s.y = 1040.0;
        s.flipped = false;
        s.begin(&p, &w, TILE, 1, &mut ev);
        let mut furthest = s.x;
        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
            furthest = furthest.min(s.x);
        }
        assert!(furthest < 900.0, "a pointer it had already looked at stopped it again");
    }

    /// A sheep asleep, eating or sitting has nowhere to turn to and is not
    /// disturbed by the mouse; only one on the move goes over to look.
    #[test]
    fn a_pointer_does_not_disturb_a_sheep_that_is_not_walking() {
        let p = pet();
        let w = world_with_the_pointer_at(900.0, 1040.0);
        let sleep = p.by_name("sleep2a").expect("the pet has a sleeping animation").id;
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 1000.0;
        s.y = 1040.0;
        s.begin(&p, &w, TILE, sleep, &mut ev);
        for _ in 0..20 {
            s.step(&p, &w, TILE, &mut ev);
            if s.animation != sleep {
                break;
            }
            assert_eq!(s.x, 1000.0, "a sleeping sheep set off towards the pointer");
        }
    }

    #[test]
    fn a_sheep_in_the_way_turns_another_back_most_of_the_time() {
        let p = pet();
        let w = world_with_a_sheep_at(900.0, 1040.0);

        // Nose to nose is the far sheep's right edge, a tile short of it.
        let (mut stopped, mut through) = (0, 0);
        for _ in 0..80 {
            if approach(&p, &w, 1010.0) >= 940.0 { stopped += 1 } else { through += 1 }
        }
        assert!(stopped > 0, "one sheep never noticed another at all");
        assert!(through > 0, "one sheep never walked on past another");
        assert!(stopped > through, "sheep walked past each other more often than not");
    }

    #[test]
    fn a_sheep_a_ledge_higher_is_not_in_the_way() {
        let p = pet();
        // Standing a whole sprite higher: on something, not in front.
        let w = world_with_a_sheep_at(900.0, 1000.0);
        for _ in 0..30 {
            assert!(
                approach(&p, &w, 1010.0) < 940.0,
                "a sheep on another level stopped one walking below it"
            );
        }
    }

    #[test]
    fn a_sheep_does_not_walk_into_itself() {
        let p = pet();
        let mut s = Sheep::new(false);
        let mut w = world();
        // The sheep's own square, sitting in the flock where it is headed.
        w.flock.push(Rect { id: s.id, x: 900.0, y: 1040.0, w: TILE, h: TILE });

        let mut ev = Vec::new();
        s.x = 1010.0;
        s.y = 1040.0;
        s.begin(&p, &w, TILE, 1, &mut ev);
        let mut furthest = s.x;
        for _ in 0..200 {
            s.step(&p, &w, TILE, &mut ev);
            furthest = furthest.min(s.x);
        }
        assert!(furthest < 940.0, "a sheep was stopped by itself");
    }

    #[test]
    fn a_sheep_stopped_by_another_turns_round_and_walks_off() {
        let p = pet();
        let w = world_with_a_sheep_at(900.0, 1040.0);
        // A meeting turns the sheep back most times but not every time, so
        // approach until one of the approaches is the kind that stops.
        for attempt in 0..100 {
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.x = 1010.0;
            s.y = 1040.0;
            s.begin(&p, &w, TILE, 1, &mut ev);

            let mut met = false;
            for _ in 0..60 {
                s.step(&p, &w, TILE, &mut ev);
                met |= s.x == 940.0;
                if met && s.flipped {
                    // Turned about, and on its way back the other side.
                    assert!(s.x >= 940.0, "walked through the sheep it turned at");
                    return;
                }
            }
            assert!(attempt < 99, "never once ended up nose to nose");
        }
    }

    /// The flock as the host keeps it: two live sheep, each seeing where the
    /// other actually is, stepped together for a long while.
    #[test]
    fn two_sheep_sharing_a_floor_keep_moving() {
        let p = pet();
        let mut w = world();
        let mut ev = Vec::new();
        let mut flock = [Sheep::new(false), Sheep::new(false)];
        for (s, x) in flock.iter_mut().zip([600.0, 900.0]) {
            s.x = x;
            s.y = 1040.0;
            s.begin(&p, &w, TILE, 1, &mut ev);
        }

        let mut ground = [0.0f64, 0.0];
        for _ in 0..4000 {
            w.flock = flock.iter().map(|s| s.bounds(TILE)).collect();
            for (s, covered) in flock.iter_mut().zip(ground.iter_mut()) {
                let was = s.x;
                s.step(&p, &w, TILE, &mut ev);
                *covered += (s.x - was).abs();
                assert!(s.x.is_finite() && s.y.is_finite(), "position went non-finite");
            }
            ev.retain(|e| *e != Event::Died);
        }
        for covered in ground {
            assert!(covered > 1000.0, "a sheep spent the run pinned against the other");
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
            flock: vec![],
            pointer: None,
        };
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 1900.0;
        s.y = 1040.0;
        s.flipped = true;
        s.begin(&p, &w, TILE, 1, &mut ev);

        for _ in 0..300 {
            s.step(&p, &w, TILE, &mut ev);
            // Past the seam is allowed only up on the short monitor, which the
            // sheep can reach by climbing; beside it there is nothing to stand
            // on and nothing to step onto.
            if s.x > 1880.0 {
                let feet = s.y + TILE;
                assert!(feet <= 542.0, "sheep walked off into the gap (x={}, feet={})", s.x, feet);
            }
        }
    }

    /// A sideways T: a wide monitor on the left, centred against a tall one on
    /// the right. The tall screen's left edge is a wall only below the wide
    /// screen's floor; above that line the sheep steps out onto it instead of
    /// climbing the rest of the edge.
    #[test]
    fn climbing_an_edge_steps_onto_the_monitor_beside_it() {
        let p = pet();
        let mut wide = screen(0, 0.0, 1920.0, 1080.0);
        wide.y = 500.0;
        let w = World {
            screens: vec![wide, screen(1, 1920.0, 1440.0, 2560.0)],
            windows: vec![],
            flock: vec![],
            pointer: None,
        };
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        // Hugging the tall screen's left edge, climbing, a little below the
        // wide screen's floor at y = 1580.
        s.x = 1920.0;
        s.y = 1600.0 - TILE;
        s.begin(&p, &w, TILE, 37, &mut ev);

        let mut arrived = false;
        for _ in 0..40 {
            s.step(&p, &w, TILE, &mut ev);
            if s.screen(&w, TILE).id == 0 {
                assert_eq!(s.y + TILE, 1580.0, "stepped across but not onto the floor");
                assert!(s.x < 1880.0, "topped out beside the floor, at x={}", s.x);
                // The climb's own border transition is the top of the screen,
                // where the sheep flips over and walks the ceiling. Arriving
                // on ground has to end the climb the way climbing down does.
                assert_eq!(
                    Some(s.animation),
                    p.landing_animation(),
                    "came onto the floor with the animation for the top of the screen"
                );
                arrived = true;
                break;
            }
        }
        assert!(arrived, "climbed past the monitor beside it (x={}, y={})", s.x, s.y);

        // And it stays there: upside down on a ceiling that is not there, the
        // sheep used to walk straight back out over the tall screen.
        for _ in 0..60 {
            s.step(&p, &w, TILE, &mut ev);
            assert!(
                s.x < 1920.0 || s.y + TILE > 1582.0,
                "drifted out over the tall screen at ({}, {}) in anim {}",
                s.x,
                s.y,
                s.animation
            );
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
                workspace: 1,
            }],
            windows: vec![Rect { id: 9, x: 3858.0, y: -26.0, w: 1404.0, h: 1242.0 }],
            flock: vec![],
            pointer: None,
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
            flock: vec![],
            pointer: None,
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
    fn the_dive_into_the_bath_is_not_caught_on_a_window() {
        let p = pet();
        let mut w = world();
        w.windows.push(Rect { id: 1, x: 200.0, y: 600.0, w: 900.0, h: 400.0 });
        let mut s = Sheep::new(false);
        s.climb_windows = true;
        let mut ev = Vec::new();
        s.x = 900.0;
        s.y = 200.0;
        // 21 (batha) is the dive: down and to the left, for a distance of its
        // own working out, and with no <border> table to be stopped by.
        s.begin(&p, &w, TILE, 21, &mut ev);

        // A dive long enough to reach the floor is held there, which is right:
        // the floor is a border the file was written against. Anywhere above
        // it the sheep should still be descending.
        let floor = w.screens[0].floor() - TILE;
        let mut pinned = 0;
        let mut last_y = s.y;
        while p.get(s.animation).unwrap().name == "batha" {
            s.step(&p, &w, TILE, &mut ev);
            if s.y == last_y && s.y < floor - 2.0 {
                pinned += 1;
            }
            last_y = s.y;
            assert!(s.resting_on.is_none(), "the dive landed on a window");
        }
        assert_eq!(pinned, 0, "the dive stopped descending {pinned} steps early");
        // It carried on past the window and down to the floor.
        assert!(s.y > 600.0, "the dive never got below the window, y {}", s.y);
    }

    #[test]
    fn a_window_top_is_still_a_ledge_for_anything_that_asks() {
        let p = pet();
        let mut w = world();
        w.windows.push(Rect { id: 1, x: 200.0, y: 600.0, w: 900.0, h: 400.0 });
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.x = 700.0;
        s.y = 400.0;
        // `fall` does declare a <border>, so it lands as it always has.
        s.begin(&p, &w, TILE, p.fall_animation().unwrap(), &mut ev);
        for _ in 0..60 {
            s.step(&p, &w, TILE, &mut ev);
            if s.resting_on == Some(1) {
                assert_eq!(s.y, 560.0, "landed somewhere other than the ledge");
                return;
            }
        }
        panic!("a falling sheep no longer lands on a window, y {}", s.y);
    }

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

        s.release(&p, &w, TILE, 0.0, 0.0, &mut ev);
        s.step(&p, &w, TILE, &mut ev);
        assert_ne!(p.get(s.animation).unwrap().name, "drag", "drag never ended");
    }

    #[test]
    fn letting_go_of_a_still_mouse_drops_the_sheep() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);
        s.grab(&p, &w, TILE);
        s.drag_to(500.0, 300.0, TILE);
        s.release(&p, &w, TILE, 0.0, 0.0, &mut ev);

        let x = s.x;
        for _ in 0..20 {
            s.step(&p, &w, TILE, &mut ev);
        }
        assert!(s.y > 300.0, "a dropped sheep should fall, got y {}", s.y);
        assert!((s.x - x).abs() < 40.0, "a dropped sheep should not fly sideways");
    }

    #[test]
    fn a_flick_throws_the_sheep_and_it_lands() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);
        s.grab(&p, &w, TILE);
        s.drag_to(500.0, 300.0, TILE);
        // Let go travelling up and to the right.
        s.release(&p, &w, TILE, 1200.0, -600.0, &mut ev);
        assert_eq!(p.get(s.animation).unwrap().name, "fall");
        assert!(!s.flipped, "a sheep thrown right should face right");

        let start = (s.x, s.y);
        s.step(&p, &w, TILE, &mut ev);
        assert!(s.y < start.1, "a sheep thrown upwards should rise first");

        let mut highest = s.y;
        let mut steps = 0;
        while s.toss.is_some() && steps < 500 {
            s.step(&p, &w, TILE, &mut ev);
            highest = highest.min(s.y);
            steps += 1;
        }
        assert!(s.toss.is_none(), "the throw never ended");
        assert!(highest < start.1 - 40.0, "the arc never went anywhere");
        assert!(s.x > start.0 + 100.0, "the sheep was not carried to the right");
        // It ends on the floor, in whatever the pet file says landing is.
        assert!(s.y >= w.screens[0].floor() - TILE - 2.0, "it did not come down, y {}", s.y);
    }

    #[test]
    fn speed_leaves_a_throw_alone() {
        let p = pet();
        let w = world();
        let arc = |speed: f64| {
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.speed = speed;
            s.begin(&p, &w, TILE, 1, &mut ev);
            s.x = 300.0;
            s.y = 400.0;
            s.grab(&p, &w, TILE);
            s.release(&p, &w, TILE, 900.0, -300.0, &mut ev);
            let mut flight = Duration::ZERO;
            while s.toss.is_some() && flight < Duration::from_secs(10) {
                flight += s.step(&p, &w, TILE, &mut ev);
            }
            (s.x, s.y, flight)
        };
        // Same arc, and the same time taken over it, however fast the sheep
        // is otherwise living.
        assert_eq!(arc(1.0), arc(4.0));
        assert_eq!(arc(1.0), arc(0.25));
    }

    #[test]
    fn a_throw_is_capped_but_keeps_its_direction() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);
        s.grab(&p, &w, TILE);
        s.drag_to(900.0, 400.0, TILE);
        s.release(&p, &w, TILE, -9000.0, -9000.0, &mut ev);

        let (vx, vy) = s.toss.expect("a hard flick is still a throw");
        assert!(vx.hypot(vy) <= TOSS_MAX + 1.0, "the throw was not capped");
        assert!((vx - vy).abs() < 1.0, "the aim was not kept");
        assert!(s.flipped, "a sheep thrown left should face left");
    }

    #[test]
    fn picking_a_thrown_sheep_out_of_the_air_ends_the_throw() {
        let p = pet();
        let w = world();
        let mut s = Sheep::new(false);
        let mut ev = Vec::new();
        s.spawn(&p, &w, TILE, &mut ev);
        s.grab(&p, &w, TILE);
        s.drag_to(500.0, 300.0, TILE);
        s.release(&p, &w, TILE, 1500.0, 0.0, &mut ev);
        s.step(&p, &w, TILE, &mut ev);

        s.grab(&p, &w, TILE);
        assert!(s.toss.is_none(), "a caught sheep is not still in flight");
        let (x, y) = (s.x, s.y);
        for _ in 0..20 {
            s.step(&p, &w, TILE, &mut ev);
        }
        assert_eq!((s.x, s.y), (x, y), "a caught sheep moved on its own");
    }

    /// The bathtub does not watch the sheep come down; it counts out the
    /// steps of the dive and splashes when they run out. The dive's length is
    /// decided by the diver's `randS`, so the tub has to be counting with the
    /// same number, or it splashes before the sheep is in it.
    #[test]
    fn the_splash_waits_for_the_sheep_to_land() {
        let p = pet();
        let w = world();
        let mut delays = Vec::new();
        for rand_s in [0.0, 7.0, 23.0, 50.0, 84.0, 99.0] {
            let mut ev = Vec::new();

            // Spawn 3: in from the right, partway down a screen whose height
            // this sheep's randS decides.
            let mut diver = Sheep::new(false);
            diver.set_rand_s(rand_s);
            diver.x = 1930.0;
            diver.y = 1080.0 / 2.0 - (rand_s * 540.0) / 120.0 - TILE;
            diver.begin(&p, &w, TILE, 21, &mut ev);

            let Some(&Event::SpawnChild { animation, x, y, rand_s: childs, .. }) = ev.first()
            else {
                panic!("the dive did not call on a bathtub: {ev:?}");
            };
            let mut tub = Sheep::new(true);
            tub.set_rand_s(childs);
            tub.x = x;
            tub.y = y;
            ev.clear();
            tub.begin(&p, &w, TILE, animation, &mut ev);

            // Both run at 30ms a step, so they step together.
            let mut landed = None;
            let mut steps = 0;
            while p.get(tub.animation).unwrap().name != "bathz" && steps < 4000 {
                diver.step(&p, &w, TILE, &mut ev);
                tub.step(&p, &w, TILE, &mut ev);
                steps += 1;
                if landed.is_none() && p.get(diver.animation).unwrap().name != "batha" {
                    landed = Some(steps);
                }
            }
            assert_eq!(p.get(tub.animation).unwrap().name, "bathz", "randS {rand_s}: no splash");
            let landed =
                landed.unwrap_or_else(|| panic!("randS {rand_s}: the tub splashed mid-dive"));
            delays.push(steps - landed);
        }

        // However far the sheep had to fall, the water breaks the same number
        // of steps into its arrival.
        assert!(
            delays.windows(2).all(|d| d[0] == d[1]),
            "the splash drifts with randS: {delays:?}"
        );
    }

    /// The black-sheep meeting is two walks timed to end nose to nose in the
    /// middle of the screen. The distance each has to cover is a screen width,
    /// which does not change with the sheep, so neither should the meeting.
    #[test]
    fn the_two_sheep_meet_in_the_middle_at_any_size() {
        let p = pet();
        let w = world();
        for scale in [0.5, 1.0, 2.0, 3.0] {
            let tile = TILE * scale;
            let mut ev = Vec::new();

            // The white sheep arrives from the right (spawn 4), the black one
            // from off the left edge, where the <child> entry puts it.
            let mut white = Sheep::new(false);
            white.scale = scale;
            white.flipped = false;
            white.x = 1920.0;
            white.y = 1080.0 - tile;
            white.begin(&p, &w, tile, 28, &mut ev);

            let mut black = Sheep::new(true);
            black.scale = scale;
            black.x = -tile;
            black.y = white.y;
            black.begin(&p, &w, tile, 31, &mut ev);

            // Walk them both until each settles into its greeting.
            for _ in 0..5000 {
                if p.get(white.animation).unwrap().name != "blacksheepc" {
                    white.step(&p, &w, tile, &mut ev);
                }
                if p.get(black.animation).unwrap().name != "blacksheepy" {
                    black.step(&p, &w, tile, &mut ev);
                }
            }
            assert_eq!(p.get(white.animation).unwrap().name, "blacksheepc");
            assert_eq!(p.get(black.animation).unwrap().name, "blacksheepy");

            // Nose to nose: the black sheep to the left, about a sheep apart,
            // and the pair of them near the middle of the screen.
            let gap = white.x - black.x;
            assert!(
                (0.0..480.0).contains(&gap),
                "scale {scale}: the sheep ended {gap} apart, not nose to nose"
            );
            let middle = (white.x + black.x) / 2.0 + tile / 2.0;
            assert!(
                (middle - 960.0).abs() < tile * 2.0,
                "scale {scale}: they met at {middle}, not in the middle"
            );
        }
    }

    /// The flower belongs in front of the sheep's nose, whichever way round
    /// it came to eat.
    #[test]
    fn the_flower_is_on_the_side_the_sheep_is_facing() {
        let p = pet();
        let w = world();
        let eat = p.by_name("eat").expect("the pet eats").id;

        let mut left = Sheep::new(false);
        let mut ev = Vec::new();
        left.x = 900.0;
        left.y = 1040.0;
        left.flipped = false;
        left.begin(&p, &w, TILE, eat, &mut ev);
        let Some(&Event::SpawnChild { x: facing_left, .. }) = ev.first() else {
            panic!("eat should spawn the flower, got {ev:?}");
        };
        assert!(facing_left < left.x, "the flower sprouted behind a sheep facing left");

        let mut right = Sheep::new(false);
        ev.clear();
        right.x = 900.0;
        right.y = 1040.0;
        right.flipped = true;
        right.begin(&p, &w, TILE, eat, &mut ev);
        let Some(&Event::SpawnChild { x: facing_right, .. }) = ev.first() else {
            panic!("eat should spawn the flower, got {ev:?}");
        };
        assert!(
            facing_right > right.x,
            "the flower sprouted behind a sheep facing right, at {facing_right}"
        );

        // Mirrored, so the same distance from the nose either way round. The
        // spawn point is a corner, so the two are compared centre to centre.
        let centre = right.x + TILE / 2.0;
        let (l, r) = (facing_left + TILE / 2.0 - centre, facing_right + TILE / 2.0 - centre);
        assert!(
            (l + r).abs() < 1e-6,
            "the flower sits closer on one side: {l} away and {r} away"
        );
    }

    /// A companion that belongs at a place on the screen rather than beside
    /// the sheep stays put when the sheep is facing the other way.
    #[test]
    fn a_screen_placed_companion_is_not_mirrored() {
        let p = pet();
        let w = world();
        let bath = p.by_name("batha").expect("the pet takes a bath").id;
        let place = |flipped| {
            let mut s = Sheep::new(false);
            let mut ev = Vec::new();
            s.x = 900.0;
            s.y = 1040.0;
            s.flipped = flipped;
            // The bath's place is drawn from randS: the same for both sheep,
            // so that only the facing differs.
            s.rand_s = 60.0;
            s.begin(&p, &w, TILE, bath, &mut ev);
            match ev.first() {
                Some(&Event::SpawnChild { x, .. }) => x,
                _ => panic!("batha should spawn the bath, got {ev:?}"),
            }
        };
        assert_eq!(place(false), place(true), "the bath moved with the sheep");
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


