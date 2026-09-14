//! The pet animation format: data model and XML parsing.
//!
//! Mirrors the schema published at http://esheep.petrucci.ch/ and implemented
//! by web-esheep. Element names are matched without regard to namespace, since
//! pet files in the wild disagree about whether it is http or https.

use std::collections::HashMap;

use crate::expr::{eval_or, Ctx};

/// Context filter on a transition. The original desktop build gated moves on
/// where the sheep was standing; the JS port drops this, so we reinstate it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Only {
    /// Always eligible.
    None,
    /// Only while standing on a window edge.
    Window,
    /// Only while on the reserved bar / bottom of the work area.
    Taskbar,
    /// Only while against a vertical surface.
    Vertical,
    /// Only while against a horizontal surface.
    Horizontal,
}

impl Only {
    fn parse(s: Option<&str>) -> Self {
        match s.unwrap_or("none").trim() {
            "window" => Only::Window,
            "taskbar" => Only::Taskbar,
            "vertical" => Only::Vertical,
            // One transition in the original spells this "horizontal+".
            s if s.starts_with("horizontal") => Only::Horizontal,
            _ => Only::None,
        }
    }
}

/// Where the sheep currently is, used to evaluate [`Only`] filters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Situation {
    pub on_window: bool,
    pub on_taskbar: bool,
    pub on_vertical: bool,
    pub on_horizontal: bool,
}

impl Situation {
    fn allows(&self, only: Only) -> bool {
        match only {
            Only::None => true,
            Only::Window => self.on_window,
            Only::Taskbar => self.on_taskbar,
            Only::Vertical => self.on_vertical,
            Only::Horizontal => self.on_horizontal,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Next {
    pub probability: f64,
    pub only: Only,
    pub target: u32,
}

/// One end of the linear ramp an animation interpolates across its run.
#[derive(Clone, Debug)]
pub struct Endpoint {
    /// Per-step horizontal velocity, in pixels.
    pub x: String,
    /// Per-step vertical velocity, in pixels.
    pub y: String,
    /// Milliseconds until the next step.
    pub interval: String,
    /// Vertical draw offset. Unused by the JS reference; we honour it.
    pub offset_y: String,
    pub opacity: f64,
}

impl Default for Endpoint {
    fn default() -> Self {
        Endpoint {
            x: "0".into(),
            y: "0".into(),
            interval: "1000".into(),
            offset_y: "0".into(),
            opacity: 1.0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    /// Mirror the sprite horizontally, which also flips its x velocity.
    Flip,
}

#[derive(Clone, Debug)]
pub struct Animation {
    pub id: u32,
    pub name: String,
    pub start: Endpoint,
    pub end: Endpoint,
    /// Row-major tile indices into the sheet.
    pub frames: Vec<u32>,
    /// Expression for the number of extra loop passes.
    pub repeat: String,
    /// Frame index the loop returns to.
    pub repeat_from: usize,
    pub action: Action,
    /// Consulted when the sequence runs to completion.
    pub next: Vec<Next>,
    /// Consulted on hitting a screen edge or landing on a surface.
    pub border: Vec<Next>,
    /// Consulted while airborne with nothing underfoot.
    pub gravity: Vec<Next>,
}

impl Animation {
    /// Total steps, including repeats: `n + (n - repeat_from) * repeat`.
    pub fn steps(&self, ctx: &Ctx) -> u32 {
        let n = self.frames.len() as f64;
        if n == 0.0 {
            return 0;
        }
        let repeat = eval_or(&self.repeat, ctx, 0.0).max(0.0).trunc();
        let from = self.repeat_from as f64;
        (n + (n - from).max(0.0) * repeat).max(1.0) as u32
    }

    /// The tile index shown at `step`, applying the repeat-from loop.
    pub fn frame_at(&self, step: u32) -> u32 {
        let n = self.frames.len();
        if n == 0 {
            return 0;
        }
        let s = step as usize;
        if s < n {
            return self.frames[s];
        }
        if self.repeat_from == 0 {
            return self.frames[s % n];
        }
        let from = self.repeat_from.min(n - 1);
        let span = n - from;
        self.frames[from + (s - from) % span]
    }
}

#[derive(Clone, Debug)]
pub struct Spawn {
    pub probability: f64,
    pub x: String,
    pub y: String,
    pub next: u32,
}

/// A companion sprite created when the parent enters `animation_id`.
#[derive(Clone, Debug)]
pub struct Child {
    pub animation_id: u32,
    /// Position expressions, evaluated against the *parent's* state.
    pub x: String,
    pub y: String,
    pub next: u32,
}

#[derive(Debug)]
pub struct Pet {
    pub tiles_x: u32,
    pub tiles_y: u32,
    /// The sprite sheet carried inside the file, if it has one. Pet files are
    /// normally self-contained; the bundled one has its payload stripped since
    /// the sheet ships beside it.
    pub png: Option<Vec<u8>>,
    pub spawns: Vec<Spawn>,
    pub animations: HashMap<u32, Animation>,
    pub children: Vec<Child>,
}

impl Pet {
    pub fn get(&self, id: u32) -> Option<&Animation> {
        self.animations.get(&id)
    }

    /// Look up an animation by name, case-insensitively. Only `drag` needs this.
    pub fn by_name(&self, name: &str) -> Option<&Animation> {
        self.animations.values().find(|a| a.name.eq_ignore_ascii_case(name))
    }

    /// Weighted pick over the candidates whose [`Only`] filter the situation
    /// permits. Probabilities are weights, not percentages, and routinely sum
    /// past 100. Returns `None` for a terminal state.
    pub fn choose<'a>(&self, list: &'a [Next], situation: Situation) -> Option<&'a Next> {
        if list.is_empty() {
            return None;
        }
        let eligible: Vec<&Next> =
            list.iter().filter(|n| situation.allows(n.only)).collect();
        // Every real animation offers an unrestricted fallback, but if a pet
        // file somehow filters everything out, moving is better than freezing.
        let pool: Vec<&Next> = if eligible.is_empty() { list.iter().collect() } else { eligible };

        let total: f64 = pool.iter().map(|n| n.probability.max(0.0)).sum();
        if total <= 0.0 {
            return pool.first().copied();
        }
        let roll = fastrand::f64() * total;
        let mut acc = 0.0;
        for n in &pool {
            acc += n.probability.max(0.0);
            if acc >= roll {
                return Some(n);
            }
        }
        pool.last().copied()
    }

    /// The animation to fall into when support vanishes and the current
    /// animation has nothing to say about it.
    ///
    /// Derived from the data rather than hardcoded: it is whichever animation
    /// the pet's `<gravity>` blocks point at most often.
    pub fn fall_animation(&self) -> Option<u32> {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for a in self.animations.values() {
            for n in &a.gravity {
                *counts.entry(n.target).or_default() += 1;
            }
        }
        let mut best: Vec<(u32, usize)> = counts.into_iter().collect();
        // Sort by popularity, then by id, so the choice is stable.
        best.sort_by_key(|(id, n)| (std::cmp::Reverse(*n), *id));
        best.first().map(|(id, _)| *id).or_else(|| self.by_name("fall").map(|a| a.id))
    }

    /// Weighted pick of a spawn point.
    pub fn choose_spawn(&self) -> Option<&Spawn> {
        // The reference sums `spawns[0]` in a loop, making later spawns
        // unreachable; we sum the actual weights.
        let total: f64 = self.spawns.iter().map(|s| s.probability.max(0.0)).sum();
        if total <= 0.0 {
            return self.spawns.first();
        }
        let roll = fastrand::f64() * total;
        let mut acc = 0.0;
        for s in &self.spawns {
            acc += s.probability.max(0.0);
            if acc >= roll {
                return Some(s);
            }
        }
        self.spawns.last()
    }

    pub fn parse(xml: &str) -> Result<Pet, String> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| format!("xml: {e}"))?;
        let root = doc.root_element();

        let image = child(root, "image");
        let tiles_x = image.and_then(|n| text_of(n, "tilesx")).and_then(|t| t.parse().ok());
        let tiles_y = image.and_then(|n| text_of(n, "tilesy")).and_then(|t| t.parse().ok());
        let png = image
            .and_then(|n| child(n, "png"))
            .and_then(|n| n.text())
            .filter(|t| !t.trim().is_empty())
            .and_then(decode_base64);

        let spawns = child(root, "spawns")
            .map(|n| children(n, "spawn").map(parse_spawn).collect())
            .unwrap_or_default();

        // The root element and the animation container share the tag name
        // `animations`, so resolve the inner one explicitly rather than by
        // document-order search.
        let container = child(root, "animations").unwrap_or(root);
        let mut animations = HashMap::new();
        for node in children(container, "animation") {
            let a = parse_animation(node)?;
            animations.insert(a.id, a);
        }
        if animations.is_empty() {
            return Err("no animations found".into());
        }

        let child_list = child(root, "childs")
            .map(|n| children(n, "child").filter_map(parse_child).collect())
            .unwrap_or_default();

        Ok(Pet {
            tiles_x: tiles_x.unwrap_or(16),
            tiles_y: tiles_y.unwrap_or(11),
            png,
            spawns,
            animations,
            children: child_list,
        })
    }
}

/// Decode standard base64, ignoring whitespace. Pet files embed the sprite
/// sheet this way, sometimes wrapped in CDATA, which the XML parser unwraps.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            c if c.is_ascii_whitespace() => continue,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// -- parsing helpers; all matching is on local names, ignoring namespaces --

type Node<'a> = roxmltree::Node<'a, 'a>;

fn children<'a>(n: Node<'a>, name: &'a str) -> impl Iterator<Item = Node<'a>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn child<'a>(n: Node<'a>, name: &'a str) -> Option<Node<'a>> {
    children(n, name).next()
}

fn text_of<'a>(n: Node<'a>, name: &'a str) -> Option<String> {
    child(n, name).and_then(|c| c.text()).map(|t| t.trim().to_string())
}

fn expr_of(n: Node<'_>, name: &str, default: &str) -> String {
    text_of(n, name).filter(|s| !s.is_empty()).unwrap_or_else(|| default.to_string())
}

fn parse_endpoint(parent: Node<'_>, name: &str) -> Endpoint {
    let Some(n) = child(parent, name) else { return Endpoint::default() };
    Endpoint {
        x: expr_of(n, "x", "0"),
        y: expr_of(n, "y", "0"),
        interval: expr_of(n, "interval", "1000"),
        offset_y: expr_of(n, "offsety", "0"),
        opacity: text_of(n, "opacity").and_then(|t| t.parse().ok()).unwrap_or(1.0),
    }
}

fn parse_next_list(parent: Node<'_>) -> Vec<Next> {
    children(parent, "next")
        .filter_map(|n| {
            let target = n.text()?.trim().parse().ok()?;
            Some(Next {
                probability: n
                    .attribute("probability")
                    .and_then(|p| p.trim().parse().ok())
                    .unwrap_or(100.0),
                only: Only::parse(n.attribute("only")),
                target,
            })
        })
        .collect()
}

fn parse_animation(n: Node<'_>) -> Result<Animation, String> {
    let id: u32 = n
        .attribute("id")
        .and_then(|a| a.trim().parse().ok())
        .ok_or_else(|| "animation without a numeric id".to_string())?;

    let sequence = child(n, "sequence");
    let frames = sequence
        .map(|s| children(s, "frame").filter_map(|f| f.text()?.trim().parse().ok()).collect())
        .unwrap_or_default();

    let action = sequence
        .and_then(|s| text_of(s, "action"))
        .map(|a| if a.eq_ignore_ascii_case("flip") { Action::Flip } else { Action::None })
        .unwrap_or(Action::None);

    Ok(Animation {
        id,
        name: text_of(n, "name").unwrap_or_default(),
        start: parse_endpoint(n, "start"),
        end: parse_endpoint(n, "end"),
        frames,
        repeat: sequence
            .and_then(|s| s.attribute("repeat"))
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| "0".into()),
        repeat_from: sequence
            .and_then(|s| s.attribute("repeatfrom"))
            .and_then(|r| r.trim().parse().ok())
            .unwrap_or(0),
        action,
        next: sequence.map(parse_next_list).unwrap_or_default(),
        border: child(n, "border").map(parse_next_list).unwrap_or_default(),
        gravity: child(n, "gravity").map(parse_next_list).unwrap_or_default(),
    })
}

fn parse_spawn(n: Node<'_>) -> Spawn {
    Spawn {
        probability: n
            .attribute("probability")
            .and_then(|p| p.trim().parse().ok())
            .unwrap_or(100.0),
        x: expr_of(n, "x", "0"),
        y: expr_of(n, "y", "0"),
        next: child(n, "next")
            .and_then(|c| c.text())
            .and_then(|t| t.trim().parse().ok())
            .unwrap_or(1),
    }
}

fn parse_child(n: Node<'_>) -> Option<Child> {
    Some(Child {
        animation_id: n.attribute("animationid")?.trim().parse().ok()?,
        x: expr_of(n, "x", "0"),
        y: expr_of(n, "y", "0"),
        next: text_of(n, "next")?.parse().ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = include_str!("../assets/animations.xml");

    fn pet() -> Pet {
        Pet::parse(XML).expect("the bundled pet file must parse")
    }

    fn ctx() -> Ctx {
        Ctx {
            screen_w: 1920.0,
            screen_h: 1080.0,
            area_w: 1920.0,
            area_h: 1080.0,
            image_w: 40.0,
            image_h: 40.0,
            image_x: 0.0,
            image_y: 0.0,
            rand_s: 50.0,
        }
    }

    #[test]
    fn parses_the_original_pet() {
        let p = pet();
        assert_eq!((p.tiles_x, p.tiles_y), (16, 11));
        assert_eq!(p.animations.len(), 63);
        assert_eq!(p.spawns.len(), 4);
        assert_eq!(p.children.len(), 4);
        for id in 1..=63u32 {
            assert!(p.get(id).is_some(), "missing animation {id}");
        }
    }

    #[test]
    fn the_bundled_pet_has_no_embedded_sheet() {
        // Its payload is stripped because the sheet ships as a PNG beside it.
        assert!(pet().png.is_none());
    }

    #[test]
    fn base64_decoding_round_trips() {
        // "sheep" and a PNG magic number, the latter with embedded whitespace
        // as the pet files actually store it.
        assert_eq!(decode_base64("c2hlZXA=").unwrap(), b"sheep");
        assert_eq!(decode_base64("aVZCT1J3MEtHZ28=").unwrap(), b"iVBORw0KGgo");
        assert_eq!(decode_base64("c2hl\n  ZXA=").unwrap(), b"sheep");
        assert_eq!(decode_base64("").unwrap(), b"");
        assert!(decode_base64("not*valid").is_none());
    }

    #[test]
    fn known_animations_have_the_expected_shape() {
        let p = pet();
        let walk = p.get(1).unwrap();
        assert_eq!(walk.name, "walk");
        assert_eq!(walk.frames, vec![2, 3]);
        assert_eq!(walk.start.x, "-2");
        // walk falls back to `fall` when it runs out of ground.
        assert_eq!(walk.gravity.iter().map(|n| n.target).collect::<Vec<_>>(), vec![5]);
        assert_eq!(p.get(5).unwrap().name, "fall");
        assert_eq!(p.get(4).unwrap().name, "drag");
        assert!(p.by_name("drag").is_some());
    }

    #[test]
    fn every_transition_points_at_a_real_animation() {
        let p = pet();
        for a in p.animations.values() {
            for n in a.next.iter().chain(&a.border).chain(&a.gravity) {
                assert!(p.get(n.target).is_some(), "anim {} -> missing {}", a.id, n.target);
            }
        }
        for s in &p.spawns {
            assert!(p.get(s.next).is_some(), "spawn -> missing {}", s.next);
        }
        for c in &p.children {
            assert!(p.get(c.next).is_some(), "child -> missing {}", c.next);
            assert!(p.get(c.animation_id).is_some());
        }
    }

    #[test]
    fn every_frame_index_is_inside_the_sheet() {
        let p = pet();
        let slots = p.tiles_x * p.tiles_y;
        for a in p.animations.values() {
            for &f in &a.frames {
                assert!(f < slots, "anim {} frame {f} exceeds {slots} tiles", a.id);
            }
        }
    }

    #[test]
    fn terminal_animations_have_no_transitions() {
        let p = pet();
        // bathz, flower and blacksheepz end their chains.
        for id in [24u32, 27, 34] {
            let a = p.get(id).unwrap();
            assert!(a.next.is_empty(), "anim {id} ({}) should be terminal", a.name);
        }
    }

    #[test]
    fn only_filters_are_parsed_and_respected() {
        let p = pet();
        let walk = p.get(1).unwrap();
        assert!(walk.next.iter().any(|n| n.only == Only::Window));

        // Off a window, window-only transitions must never be selected.
        let grounded = Situation { on_taskbar: true, ..Default::default() };
        for _ in 0..500 {
            let pick = p.choose(&walk.next, grounded).unwrap();
            assert_ne!(pick.only, Only::Window);
        }
        // On a window they become reachable.
        let on_win = Situation { on_window: true, ..Default::default() };
        assert!((0..2000).any(|_| p.choose(&walk.next, on_win).unwrap().only == Only::Window));
    }

    #[test]
    fn all_four_spawns_are_reachable() {
        // The reference's spawn maths makes the last one unreachable; ours must not.
        let p = pet();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..20_000 {
            let s = p.choose_spawn().unwrap();
            seen.insert((s.x.clone(), s.y.clone()));
        }
        assert_eq!(seen.len(), 4, "expected every spawn point to be reachable");
    }

    #[test]
    fn step_counts_and_frame_looping() {
        let p = pet();
        let c = ctx();
        // walk: 2 frames, repeat 20, repeatfrom 0 -> 2 + 2*20 = 42 steps.
        let walk = p.get(1).unwrap();
        assert_eq!(walk.steps(&c), 42);
        assert_eq!(walk.frame_at(0), 2);
        assert_eq!(walk.frame_at(1), 3);
        assert_eq!(walk.frame_at(2), 2);

        // Every animation must produce a usable step count and never panic.
        for a in p.animations.values() {
            let steps = a.steps(&c);
            assert!(steps >= 1, "anim {} has no steps", a.id);
            for s in 0..steps.min(500) {
                a.frame_at(s);
            }
        }
    }

    #[test]
    fn convert_expressions_no_longer_degenerate() {
        let p = pet();
        let c = ctx();
        // Animations 29 and 32 use .NET Convert(), which the reference fails on
        // and treats as repeat=0. They should now repeat properly.
        for id in [29u32, 32] {
            let a = p.get(id).unwrap();
            assert!(a.repeat.contains("Convert"));
            assert!(a.steps(&c) > a.frames.len() as u32, "anim {id} degenerated to no repeats");
        }
    }
}
