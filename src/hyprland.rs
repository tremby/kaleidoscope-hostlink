//! Hyprland: instance discovery, event handling, and workspace state.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::{Msg, RETRY};

// --- Protocol constants (must match Hostlink.h) -----------------------------

pub(crate) const CHANNEL_WORKSPACES: u8 = 0;
pub(crate) const NUM_KEYS: usize = 11; // workspaces 1..10 + special

const CLIENTS: u8 = 1 << 0;
const FULLSCREEN: u8 = 1 << 1;
const URGENT: u8 = 1 << 2;
const VISIBLE: u8 = 1 << 3;
const FOCUSED: u8 = 1 << 4;

pub(crate) type UrgentSet = Arc<Mutex<HashSet<String>>>;

// --- Hyprland ---------------------------------------------------------------

/// Find the Hyprland instance directory. Prefers $HYPRLAND_INSTANCE_SIGNATURE
/// if it points at a live instance, otherwise the most recently created one
/// (systemd user services don't inherit that variable).
fn find_instance() -> Option<PathBuf> {
    let base = PathBuf::from(env::var_os("XDG_RUNTIME_DIR")?).join("hypr");
    if let Some(sig) = env::var_os("HYPRLAND_INSTANCE_SIGNATURE") {
        let dir = base.join(sig);
        if dir.join(".socket2.sock").exists() {
            return Some(dir);
        }
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(&base).ok()?.flatten() {
        let sock = entry.path().join(".socket2.sock");
        if let Ok(mtime) = fs::metadata(&sock).and_then(|m| m.modified()) {
            if best.as_ref().map_or(true, |(t, _)| mtime > *t) {
                best = Some((mtime, entry.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn hypr_query(dir: &Path, cmd: &str) -> io::Result<Value> {
    let mut s = UnixStream::connect(dir.join(".socket.sock"))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(cmd.as_bytes())?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf)?;
    serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Hyprland addresses appear as "0xABC" in JSON and "abc" in events.
/// Only the first comma-separated field is the address, so extra fields added
/// to an event in a future Hyprland version don't break matching.
fn norm_addr(s: &str) -> String {
    let first = s.split(',').next().unwrap_or("");
    first.trim().trim_start_matches("0x").to_ascii_lowercase()
}

/// Update urgent tracking for one event line; return true if the workspace
/// picture may have changed and the main thread should recompute.
fn handle_event(line: &str, urgent: &UrgentSet) -> bool {
    let (name, data) = line.split_once(">>").unwrap_or((line, ""));
    match name {
        "urgent" => {
            urgent.lock().unwrap().insert(norm_addr(data));
            true
        }
        // Focusing a window clears its urgent hint; there is no other signal.
        // Focus changes otherwise don't affect any flag, so only recompute if
        // the set actually changed.
        "activewindowv2" => {
            let a = norm_addr(data);
            !a.is_empty() && urgent.lock().unwrap().remove(&a)
        }
        "closewindow" => {
            urgent.lock().unwrap().remove(&norm_addr(data));
            true
        }
        "workspace" | "workspacev2" | "focusedmon" | "focusedmonv2" | "createworkspace"
        | "createworkspacev2" | "destroyworkspace" | "destroyworkspacev2" | "moveworkspace"
        | "moveworkspacev2" | "renameworkspace" | "activespecial" | "activespecialv2"
        | "openwindow" | "movewindow" | "movewindowv2" | "fullscreen" | "monitoradded"
        | "monitoraddedv2" | "monitorremoved" | "monitorremovedv2" | "configreloaded" => true,
        _ => false,
    }
}

pub(crate) fn event_thread(urgent: UrgentSet, tx: Sender<Msg>) {
    let mut announced = false;
    loop {
        if let Some(dir) = find_instance() {
            if let Ok(mut stream) = UnixStream::connect(dir.join(".socket2.sock")) {
                eprintln!("connected to Hyprland events at {}", dir.display());
                announced = false;
                if tx.send(Msg::Dirty).is_err() {
                    return;
                }
                let mut pending: Vec<u8> = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            pending.extend_from_slice(&buf[..n]);
                            let mut dirty = false;
                            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                                let line: Vec<u8> = pending.drain(..=pos).collect();
                                let line = String::from_utf8_lossy(&line[..line.len() - 1]);
                                dirty |= handle_event(&line, &urgent);
                            }
                            if dirty && tx.send(Msg::Dirty).is_err() {
                                return;
                            }
                        }
                    }
                }
                eprintln!("Hyprland event socket closed");
                urgent.lock().unwrap().clear();
                let _ = tx.send(Msg::Dirty); // main thread will fail its query and drop the frame
            }
        }
        if !announced {
            eprintln!("waiting for Hyprland...");
            announced = true;
        }
        thread::sleep(RETRY);
    }
}

// --- State computation (pure; unit tested) ------------------------------------

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(|a| a.as_slice()).unwrap_or(&[])
}

fn is_special(name: &str) -> bool {
    name.starts_with("special")
}

/// Map a workspace {id, name} to its key index: 1..=10 -> 0..=9, any special
/// workspace -> 10, anything else (named/higher workspaces) -> None.
fn ws_index(id: i64, name: &str) -> Option<usize> {
    if is_special(name) {
        Some(10)
    } else if (1..=10).contains(&id) {
        Some((id - 1) as usize)
    } else {
        None
    }
}

fn ws_ref_index(v: &Value) -> Option<usize> {
    ws_index(v["id"].as_i64().unwrap_or(0), v["name"].as_str().unwrap_or(""))
}

fn compute(
    workspaces: &Value,
    clients: &Value,
    monitors: &Value,
    urgent: &HashSet<String>,
) -> [u8; NUM_KEYS] {
    let mut f = [0u8; NUM_KEYS];

    for w in arr(workspaces) {
        if let Some(i) = ws_ref_index(w) {
            if w["windows"].as_i64().unwrap_or(0) > 0 {
                f[i] |= CLIENTS;
            }
            if w["hasfullscreen"].as_bool().unwrap_or(false) {
                f[i] |= FULLSCREEN;
            }
        }
    }

    for c in arr(clients) {
        if urgent.contains(&norm_addr(c["address"].as_str().unwrap_or(""))) {
            if let Some(i) = ws_ref_index(&c["workspace"]) {
                f[i] |= URGENT;
            }
        }
    }

    for m in arr(monitors) {
        let focused = m["focused"].as_bool().unwrap_or(false);
        let special_shown = is_special(m["specialWorkspace"]["name"].as_str().unwrap_or(""));
        if let Some(i) = ws_ref_index(&m["activeWorkspace"]) {
            f[i] |= VISIBLE;
            // A shown special workspace covers the regular one: not focused.
            if focused && !special_shown {
                f[i] |= FOCUSED;
            }
        }
        if special_shown {
            f[10] |= VISIBLE;
            if focused {
                f[10] |= FOCUSED;
            }
        }
    }
    f
}

pub(crate) fn query_frame(urgent: &UrgentSet) -> io::Result<[u8; NUM_KEYS]> {
    let dir = find_instance().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Hyprland"))?;
    // Snapshot first, so an urgent event arriving during the queries is never
    // pruned by a stale client list.
    let before: HashSet<String> = urgent.lock().unwrap().clone();
    let workspaces = hypr_query(&dir, "j/workspaces")?;
    let clients = hypr_query(&dir, "j/clients")?;
    let monitors = hypr_query(&dir, "j/monitors")?;

    let live: HashSet<String> = arr(&clients)
        .iter()
        .map(|c| norm_addr(c["address"].as_str().unwrap_or("")))
        .collect();
    {
        let mut set = urgent.lock().unwrap();
        for gone in before.difference(&live) {
            set.remove(gone);
        }
    }
    let urgent_now = urgent.lock().unwrap().clone();
    Ok(compute(&workspaces, &clients, &monitors, &urgent_now))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn set(addrs: &[&str]) -> HashSet<String> {
        addrs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn basic_flags_and_priority_inputs() {
        let ws = json!([
            {"id":1,"name":"1","windows":2,"hasfullscreen":false},
            {"id":2,"name":"2","windows":1,"hasfullscreen":true},
            {"id":3,"name":"3","windows":1,"hasfullscreen":false},
            {"id":-99,"name":"special:magic","windows":1,"hasfullscreen":false},
        ]);
        let cl = json!([
            {"address":"0xaaa","workspace":{"id":3,"name":"3"}},
        ]);
        let mon = json!([
            {"focused":true,"activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0,"name":""}},
            {"focused":false,"activeWorkspace":{"id":2,"name":"2"},"specialWorkspace":{"id":0,"name":""}},
        ]);
        let f = compute(&ws, &cl, &mon, &set(&["aaa"]));
        assert_eq!(f[0], CLIENTS | VISIBLE | FOCUSED);
        assert_eq!(f[1], CLIENTS | FULLSCREEN | VISIBLE);
        assert_eq!(f[2], CLIENTS | URGENT);
        assert_eq!(f[3], 0);
        assert_eq!(f[10], CLIENTS);
    }

    #[test]
    fn special_covers_regular_workspace() {
        let ws = json!([{"id":4,"name":"4","windows":1,"hasfullscreen":false},
                        {"id":-98,"name":"special:s","windows":1,"hasfullscreen":false}]);
        let mon = json!([{"focused":true,"activeWorkspace":{"id":4,"name":"4"},
                          "specialWorkspace":{"id":-98,"name":"special:s"}}]);
        let f = compute(&ws, &json!([]), &mon, &set(&[]));
        assert_eq!(f[3], CLIENTS | VISIBLE); // covered: visible, not focused
        assert_eq!(f[10], CLIENTS | VISIBLE | FOCUSED);
    }

    #[test]
    fn special_on_unfocused_monitor_is_only_visible() {
        let mon = json!([
            {"focused":true,"activeWorkspace":{"id":1,"name":"1"},"specialWorkspace":{"id":0,"name":""}},
            {"focused":false,"activeWorkspace":{"id":2,"name":"2"},"specialWorkspace":{"id":-99,"name":"special:x"}},
        ]);
        let f = compute(&json!([]), &json!([]), &mon, &set(&[]));
        assert_eq!(f[0], VISIBLE | FOCUSED);
        assert_eq!(f[1], VISIBLE);
        assert_eq!(f[10], VISIBLE);
    }

    #[test]
    fn urgent_tracking_events() {
        let u: UrgentSet = Arc::new(Mutex::new(HashSet::new()));
        assert!(handle_event("urgent>>5b11eb027030", &u));
        assert!(u.lock().unwrap().contains("5b11eb027030"));
        assert!(!handle_event("activewindowv2>>ffffffff", &u)); // unrelated focus
        assert!(handle_event("activewindowv2>>5b11eb027030", &u)); // cleared
        assert!(u.lock().unwrap().is_empty());
        assert!(!handle_event("activewindow>>kitty,title", &u));
        assert!(!handle_event("activewindowv2>>", &u));
    }

    #[test]
    fn tolerates_unknown_events_and_extra_fields() {
        let u: UrgentSet = Arc::new(Mutex::new(HashSet::new()));
        assert!(!handle_event("someneweventfromthefuture>>1,2,3", &u));
        assert!(!handle_event("no separator at all", &u));
        assert!(!handle_event("", &u));
        assert!(handle_event("urgent>>0xABC,extra,fields", &u));
        assert!(u.lock().unwrap().contains("abc"));
        assert!(handle_event("activewindowv2>>abc,more", &u));
        assert!(u.lock().unwrap().is_empty());
    }

    #[test]
    fn named_and_high_workspaces_ignored() {
        assert_eq!(ws_index(11, "11"), None);
        assert_eq!(ws_index(-1337, "scratch"), None);
        assert_eq!(ws_index(10, "10"), Some(9));
    }
}
