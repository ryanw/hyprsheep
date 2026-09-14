//! Hyprland IPC: where the windows are, and when they move.
//!
//! Two sockets are involved. `.socket.sock` answers one-shot JSON queries and
//! `.socket2.sock` streams a line per compositor event; we use the latter only
//! as a hint that the former is worth re-reading.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use serde_json::Value;

use crate::engine::{Rect, Screen, World};

fn socket_dir() -> Result<PathBuf, String> {
    let runtime = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| "XDG_RUNTIME_DIR is not set".to_string())?;
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .map_err(|_| "HYPRLAND_INSTANCE_SIGNATURE is not set; is Hyprland running?".to_string())?;
    Ok(PathBuf::from(runtime).join("hypr").join(sig))
}

/// Send one command down the request socket and parse the JSON reply.
fn query(command: &str) -> Result<Value, String> {
    let path = socket_dir()?.join(".socket.sock");
    let mut sock = UnixStream::connect(&path)
        .map_err(|e| format!("connect {}: {e}", path.display()))?;
    sock.write_all(command.as_bytes()).map_err(|e| format!("write {command}: {e}"))?;
    let mut buf = Vec::new();
    sock.read_to_end(&mut buf).map_err(|e| format!("read {command}: {e}"))?;
    serde_json::from_slice(&buf).map_err(|e| format!("parse {command}: {e}"))
}

fn f(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Index `i` of a JSON array of numbers.
fn at(v: &Value, key: &str, i: usize) -> f64 {
    v.get(key).and_then(|a| a.get(i)).and_then(Value::as_f64).unwrap_or(0.0)
}

/// A monitor, in global logical pixels.
#[derive(Clone, Debug)]
pub struct Monitor {
    pub id: i64,
    /// Connector name, e.g. `eDP-1`; how a wl_output is matched to it.
    pub name: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Space claimed by bars: left, top, right, bottom.
    pub reserved: (f64, f64, f64, f64),
    pub active_workspace: i64,
}

impl Monitor {
    fn screen(&self) -> Screen {
        Screen {
            id: self.id,
            x: self.x,
            y: self.y,
            w: self.width,
            h: self.height,
            reserved: self.reserved,
        }
    }
}

/// Read the monitor list, returning the focused one first.
pub fn monitors() -> Result<Vec<Monitor>, String> {
    let v = query("j/monitors")?;
    let list = v.as_array().ok_or("monitors: expected an array")?;
    let mut out = monitors_from(list);

    let focused = list
        .iter()
        .position(|m| m.get("focused").and_then(Value::as_bool).unwrap_or(false));
    if let Some(i) = focused {
        out.swap(0, i);
    }
    if out.is_empty() {
        return Err("no enabled monitors".into());
    }
    Ok(out)
}

/// Map the enabled entries of a `j/monitors` reply into the global logical
/// space every other coordinate is already in.
fn monitors_from(list: &[Value]) -> Vec<Monitor> {
    list.iter()
        .filter(|m| !m.get("disabled").and_then(Value::as_bool).unwrap_or(false))
        .map(|m| {
            // width/height are the *mode's* physical pixels, before both the
            // scale and the transform: a 2560x1440 panel hung sideways is
            // still reported 2560x1440, though it occupies 1440x2560 of the
            // layout. Everything else Hyprland reports — x/y, and every
            // window's `at` and `size` — is post-transform logical space, so
            // the monitor is the one thing that has to be turned to match.
            let scale = f(m, "scale").max(0.01);
            let transform = m.get("transform").and_then(Value::as_i64).unwrap_or(0);
            let (mw, mh) = (f(m, "width") / scale, f(m, "height") / scale);
            // The rotated transforms are the odd ones, flipped or not: 1 (90°),
            // 3 (270°), 5 (flipped 90°) and 7 (flipped 270°).
            let (width, height) = if transform % 2 == 1 { (mh, mw) } else { (mw, mh) };
            Monitor {
                id: m.get("id").and_then(Value::as_i64).unwrap_or(0),
                name: m.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
                x: f(m, "x"),
                y: f(m, "y"),
                width,
                height,
                reserved: (
                    at(m, "reserved", 0),
                    at(m, "reserved", 1),
                    at(m, "reserved", 2),
                    at(m, "reserved", 3),
                ),
                active_workspace: m
                    .get("activeWorkspace")
                    .and_then(|w| w.get("id"))
                    .and_then(Value::as_i64)
                    .unwrap_or(-1),
            }
        })
        .collect()
}

/// Build the sheep's world: every monitor, and every window visible on one.
///
/// Hyprland reports window positions in the same global logical space it lays
/// monitors out in, so no translation is needed.
pub fn snapshot(monitors: &[Monitor]) -> Result<World, String> {
    let v = query("j/clients")?;
    let list = v.as_array().ok_or("clients: expected an array")?;

    let mut windows: Vec<Rect> = list
        .iter()
        .filter(|c| {
            // Only windows actually on screen are walkable: mapped, not
            // hidden, and on the workspace their monitor is showing.
            if !c.get("mapped").and_then(Value::as_bool).unwrap_or(false)
                || c.get("hidden").and_then(Value::as_bool).unwrap_or(false)
            {
                return false;
            }
            let Some(mid) = c.get("monitor").and_then(Value::as_i64) else { return false };
            let ws = c.get("workspace").and_then(|w| w.get("id")).and_then(Value::as_i64);
            monitors.iter().any(|m| m.id == mid && Some(m.active_workspace) == ws)
        })
        .filter_map(|c| {
            let w = at(c, "size", 0);
            let h = at(c, "size", 1);
            if w < 1.0 || h < 1.0 {
                return None;
            }
            // Addresses are hex strings; they give each window a stable
            // identity so the sheep can tell "moved" from "replaced".
            let id = c
                .get("address")
                .and_then(Value::as_str)
                .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
                .unwrap_or(0);
            Some(Rect { id, x: at(c, "at", 0), y: at(c, "at", 1), w, h })
        })
        .collect();

    // Prefer landing on whatever is highest up, so the sheep walks along the
    // topmost edge rather than one buried behind it.
    windows.sort_by(|a, b| a.y.total_cmp(&b.y));

    Ok(World {
        screens: monitors.iter().map(Monitor::screen).collect(),
        windows,
        flock: Vec::new(),
    })
}

/// The current monitor layout and the world it implies.
pub fn world() -> Result<(Vec<Monitor>, World), String> {
    let m = monitors()?;
    let w = snapshot(&m)?;
    Ok((m, w))
}

/// Watch the event socket, calling `notify` whenever the layout may have moved.
///
/// Every event notifies rather than matching on specific ones: the set of
/// events that can move a window is large and version-dependent, and a
/// re-query is cheap next to getting it wrong.
pub fn watch(notify: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        loop {
            match socket_dir().map(|d| d.join(".socket2.sock")).and_then(|p| {
                UnixStream::connect(&p).map_err(|e| format!("connect {}: {e}", p.display()))
            }) {
                Ok(stream) => {
                    for line in BufReader::new(stream).lines() {
                        if line.is_err() {
                            break;
                        }
                        notify();
                    }
                }
                Err(e) => eprintln!("hyprsheep: event socket: {e}"),
            }
            // The compositor restarted or dropped us; back off and retry so the
            // sheep keeps working across a Hyprland reload.
            std::thread::sleep(std::time::Duration::from_secs(2));
            notify();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Monitor {
        let v: Value = serde_json::from_str(json).unwrap();
        // The same mapping `monitors()` applies to each entry.
        monitors_from(&[v]).pop().unwrap()
    }

    #[test]
    fn an_upright_monitor_keeps_its_mode() {
        let m = parse(
            r#"{ "id": 1, "name": "DP-3", "x": 0, "y": 0, "width": 3840,
                 "height": 1600, "scale": 1.0, "transform": 0 }"#,
        );
        assert_eq!((m.width, m.height), (3840.0, 1600.0));
    }

    /// Hyprland reports the mode either way round; only `transform` says which
    /// way up the panel is hung. Untransformed, the sheep were given a screen
    /// 2560 wide and 1440 tall for one that is really 1440 by 2560 — so they
    /// walked a floor half way down it and an edge past the right of it.
    #[test]
    fn a_rotated_monitor_is_turned_to_match_the_layout() {
        for transform in [1, 3, 5, 7] {
            let m = parse(&format!(
                r#"{{ "width": 2560, "height": 1440, "scale": 1.0,
                      "transform": {transform} }}"#
            ));
            assert_eq!((m.width, m.height), (1440.0, 2560.0), "transform {transform}");
        }
        for transform in [0, 2, 4, 6] {
            let m = parse(&format!(
                r#"{{ "width": 2560, "height": 1440, "scale": 1.0,
                      "transform": {transform} }}"#
            ));
            assert_eq!((m.width, m.height), (2560.0, 1440.0), "transform {transform}");
        }
    }

    #[test]
    fn scale_applies_before_the_swap() {
        let m = parse(
            r#"{ "width": 2560, "height": 1440, "scale": 2.0, "transform": 3 }"#,
        );
        assert_eq!((m.width, m.height), (720.0, 1280.0));
    }
}
