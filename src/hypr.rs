//! Hyprland IPC: where the windows are, and when they move.
//!
//! Two sockets are involved. `.socket.sock` answers one-shot JSON queries and
//! `.socket2.sock` streams a line per compositor event; we use the latter only
//! as a hint that the former is worth re-reading.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::engine::{Rect, World};

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

/// The monitor we render on, in logical pixels.
#[derive(Clone, Copy, Debug)]
pub struct Monitor {
    pub id: i64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Space claimed by bars: left, top, right, bottom.
    pub reserved: (f64, f64, f64, f64),
    pub active_workspace: i64,
}

impl Monitor {
    /// Screen height minus any bottom bar; this is the sheep's floor, and
    /// standing on it is what the pet data calls the taskbar.
    fn floor(&self) -> f64 {
        self.height - self.reserved.3
    }
}

/// Read the monitor list, returning the focused one first.
fn monitors() -> Result<Vec<Monitor>, String> {
    let v = query("j/monitors")?;
    let list = v.as_array().ok_or("monitors: expected an array")?;
    let mut out: Vec<Monitor> = list
        .iter()
        .filter(|m| !m.get("disabled").and_then(Value::as_bool).unwrap_or(false))
        .map(|m| {
            // width/height are physical pixels; everything else Hyprland
            // reports, and everything we draw, is logical.
            let scale = f(m, "scale").max(0.01);
            Monitor {
                id: m.get("id").and_then(Value::as_i64).unwrap_or(0),
                x: f(m, "x"),
                y: f(m, "y"),
                width: f(m, "width") / scale,
                height: f(m, "height") / scale,
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
        .collect();

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

/// Build the sheep's world from the current window layout on `monitor`.
pub fn snapshot(monitor: &Monitor) -> Result<World, String> {
    let v = query("j/clients")?;
    let list = v.as_array().ok_or("clients: expected an array")?;

    let mut windows: Vec<Rect> = list
        .iter()
        .filter(|c| {
            // Only windows actually on screen are walkable: mapped, not
            // hidden, on this monitor, and on the workspace it is showing.
            c.get("mapped").and_then(Value::as_bool).unwrap_or(false)
                && !c.get("hidden").and_then(Value::as_bool).unwrap_or(false)
                && c.get("monitor").and_then(Value::as_i64) == Some(monitor.id)
                && c.get("workspace").and_then(|w| w.get("id")).and_then(Value::as_i64)
                    == Some(monitor.active_workspace)
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
            Some(Rect {
                id,
                x: at(c, "at", 0) - monitor.x,
                y: at(c, "at", 1) - monitor.y,
                w,
                h,
            })
        })
        .collect();

    // Prefer landing on whatever is highest up, so the sheep walks along the
    // topmost edge rather than one buried behind it.
    windows.sort_by(|a, b| a.y.total_cmp(&b.y));

    Ok(World {
        screen_w: monitor.width,
        screen_h: monitor.height,
        area_w: monitor.width - monitor.reserved.0 - monitor.reserved.2,
        area_h: monitor.floor(),
        windows,
    })
}

/// The monitor the overlay should live on, plus its initial world.
pub fn current() -> Result<(Monitor, World), String> {
    let m = *monitors()?.first().ok_or("no monitors")?;
    let w = snapshot(&m)?;
    Ok((m, w))
}

/// Re-read the monitor by id, to pick up bar or workspace changes.
pub fn refresh_monitor(id: i64) -> Result<Monitor, String> {
    monitors()?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| format!("monitor {id} went away"))
}

/// Watch the event socket, raising `dirty` whenever the layout may have moved.
///
/// Every event sets the flag rather than matching on specific ones: the set of
/// events that can move a window is large and version-dependent, and a
/// re-query is cheap next to getting it wrong.
pub fn watch(dirty: Arc<AtomicBool>) {
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
                        dirty.store(true, Ordering::Relaxed);
                    }
                }
                Err(e) => eprintln!("hyprsheep: event socket: {e}"),
            }
            // The compositor restarted or dropped us; back off and retry so the
            // sheep keeps working across a Hyprland reload.
            std::thread::sleep(std::time::Duration::from_secs(2));
            dirty.store(true, Ordering::Relaxed);
        }
    });
}
