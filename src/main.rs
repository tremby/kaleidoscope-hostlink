//! kaleidoscope-hostlink: push desktop state to a Kaleidoscope keyboard for LED effects.
//!
//! Threads:
//!   * event thread   - reads Hyprland's .socket2.sock, tracks urgent windows,
//!                      and tells the main thread when something changed
//!   * signal thread  - turns SIGINT/SIGTERM/SIGHUP into a Quit message
//!   * main thread    - debounces, queries Hyprland, builds the frame, and
//!                      owns the serial port (connect, send, heartbeat, clear)

use std::collections::HashSet;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

// --- Protocol constants (must match Hostlink.h) -----------------------------

const CHANNEL_WORKSPACES: u8 = 0;
const NUM_KEYS: usize = 11; // workspaces 1..10 + special

const CLIENTS: u8 = 1 << 0;
const FULLSCREEN: u8 = 1 << 1;
const URGENT: u8 = 1 << 2;
const VISIBLE: u8 = 1 << 3;
const FOCUSED: u8 = 1 << 4;

// --- Timing -----------------------------------------------------------------

/// Resend the full state this often; must be well under the firmware's 6 s
/// stale timeout.
const HEARTBEAT: Duration = Duration::from_secs(2);
/// Re-query Hyprland at least this often even without events, as a safety net
/// against event types this program doesn't know about.
const RESYNC: Duration = Duration::from_secs(10);
/// How long to wait for more events before querying Hyprland.
const DEBOUNCE: Duration = Duration::from_millis(15);
/// Retry interval for finding the keyboard / Hyprland.
const RETRY: Duration = Duration::from_secs(1);
/// How long to wait for the keyboard's reply to a command.
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

enum Msg {
    Dirty,
    Quit,
}

type UrgentSet = Arc<Mutex<HashSet<String>>>;

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

fn event_thread(urgent: UrgentSet, tx: Sender<Msg>) {
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

fn query_frame(urgent: &UrgentSet) -> io::Result<[u8; NUM_KEYS]> {
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

// --- Keyboard ---------------------------------------------------------------

fn find_keyboard(explicit: &Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return p.exists().then(|| p.clone());
    }
    let mut hits: Vec<PathBuf> = fs::read_dir("/dev/serial/by-id")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().to_ascii_lowercase().contains("keyboardio"))
        .map(|e| e.path())
        .collect();
    hits.sort();
    hits.into_iter().next()
}

/// Open the tty in raw mode (no echo, no CR/LF translation) with a 100 ms read
/// timeout (VMIN=0, VTIME=1: read() returns 0 bytes on timeout).
fn open_keyboard(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)?;
    let fd = file.as_raw_fd();
    // SAFETY: plain termios/fcntl calls on a valid, owned fd.
    unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::cfmakeraw(&mut t);
        t.c_cflag |= libc::CLOCAL | libc::CREAD;
        t.c_cflag &= !libc::HUPCL;
        libc::cfsetispeed(&mut t, libc::B115200);
        libc::cfsetospeed(&mut t, libc::B115200);
        t.c_cc[libc::VMIN] = 0;
        t.c_cc[libc::VTIME] = 1;
        if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
            return Err(io::Error::last_os_error());
        }
        libc::tcflush(fd, libc::TCIOFLUSH);
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(file)
}

/// Send one Focus command line and read the reply through its terminating
/// "\r\n.\r\n". Returns whether the reply contained "ok".
fn exchange(port: &mut File, line: &str) -> io::Result<bool> {
    port.write_all(line.as_bytes())?;
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut reply = Vec::new();
    let mut tmp = [0u8; 64];
    loop {
        match port.read(&mut tmp) {
            Ok(n) if n > 0 => {
                reply.extend_from_slice(&tmp[..n]);
                if reply.ends_with(b"\r\n.\r\n") || reply.ends_with(b"\n.\n") {
                    return Ok(reply.windows(2).any(|w| w == b"ok"));
                }
            }
            Ok(_) => {} // read timeout
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
        if Instant::now() > deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "no reply from keyboard"));
        }
    }
}

fn format_set(channel: u8, data: &[u8]) -> String {
    let mut s = format!("hostlink.set {} {}", channel, data.len());
    for b in data {
        s.push(' ');
        s.push_str(&b.to_string());
    }
    s.push('\n');
    s
}

// --- Main -------------------------------------------------------------------

fn main() {
    // If any thread panics, take the whole process down (after the normal
    // panic message) so the service manager restarts us, rather than limping
    // on with, say, a dead event thread.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        std::process::exit(101);
    }));

    let mut explicit_device: Option<PathBuf> = env::var_os("HOSTLINK_DEVICE").map(PathBuf::from);
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--device" => explicit_device = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("usage: kaleidoscope-hostlink [--device /dev/serial/by-id/...]");
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    let urgent: UrgentSet = Arc::new(Mutex::new(HashSet::new()));

    {
        let tx = tx.clone();
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP]).expect("signal setup");
        thread::spawn(move || {
            if signals.forever().next().is_some() {
                let _ = tx.send(Msg::Quit);
            }
        });
    }
    {
        let urgent = Arc::clone(&urgent);
        let tx = tx.clone();
        thread::spawn(move || event_thread(urgent, tx));
    }

    let mut port: Option<File> = None;
    let mut frame: Option<[u8; NUM_KEYS]> = None;
    let mut last_send = Instant::now();
    let mut next_connect = Instant::now();
    let mut last_query = Instant::now();
    let mut kb_ok = true; // for log-on-change
    let mut hypr_ok = true;

    'main: loop {
        let now = Instant::now();
        let timeout = if port.is_some() {
            (last_send + HEARTBEAT).saturating_duration_since(now)
        } else {
            next_connect.saturating_duration_since(now)
        };

        let mut recompute = false;
        match rx.recv_timeout(timeout) {
            Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => break 'main,
            Ok(Msg::Dirty) => {
                // Debounce: swallow the rest of the burst.
                loop {
                    match rx.recv_timeout(DEBOUNCE) {
                        Ok(Msg::Dirty) => {}
                        Ok(Msg::Quit) | Err(RecvTimeoutError::Disconnected) => break 'main,
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }
                recompute = true;
            }
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Nothing to update without a keyboard; a fresh query happens on connect.
        if port.is_some() && last_query.elapsed() >= RESYNC {
            recompute = true;
        }

        // (Re)connect the keyboard.
        let mut force_send = false;
        if port.is_none() && Instant::now() >= next_connect {
            match find_keyboard(&explicit_device).map(|p| (open_keyboard(&p), p)) {
                Some((Ok(p), path)) => {
                    eprintln!("keyboard connected at {}", path.display());
                    port = Some(p);
                    recompute = true;
                    force_send = true;
                }
                Some((Err(e), path)) => {
                    if kb_ok {
                        eprintln!("cannot open {}: {e}", path.display());
                    }
                    kb_ok = false;
                    next_connect = Instant::now() + RETRY;
                }
                None => {
                    if kb_ok {
                        eprintln!("waiting for keyboard...");
                    }
                    kb_ok = false;
                    next_connect = Instant::now() + RETRY;
                }
            }
        }

        if recompute && port.is_some() {
            last_query = Instant::now();
            match query_frame(&urgent) {
                Ok(f) => {
                    if !hypr_ok {
                        eprintln!("Hyprland state available again");
                    }
                    hypr_ok = true;
                    frame = Some(f);
                }
                Err(e) => {
                    if hypr_ok {
                        eprintln!("cannot query Hyprland: {e}");
                    }
                    hypr_ok = false;
                    frame = None; // let the keyboard's stale timeout blank it
                }
            }
        }

        if let (Some(p), Some(f)) = (port.as_mut(), frame) {
            if recompute || force_send || last_send.elapsed() >= HEARTBEAT {
                match exchange(p, &format_set(CHANNEL_WORKSPACES, &f)) {
                    Ok(true) => {
                        if !kb_ok {
                            eprintln!("keyboard accepting frames");
                        }
                        kb_ok = true;
                    }
                    Ok(false) => {
                        if kb_ok {
                            eprintln!("keyboard did not answer ok (is Hostlink flashed?)");
                        }
                        kb_ok = false;
                    }
                    Err(e) => {
                        eprintln!("keyboard I/O error: {e}; will reconnect");
                        port = None;
                        kb_ok = true;
                        next_connect = Instant::now() + RETRY;
                    }
                }
                last_send = Instant::now();
            }
        } else if port.is_some() && last_send.elapsed() >= HEARTBEAT {
            last_send = Instant::now(); // no frame to send (Hyprland down); avoid busy loop
        }
    }

    // Graceful exit: blank the overlay immediately instead of waiting for the
    // stale timeout.
    if let Some(mut p) = port {
        let _ = exchange(&mut p, &format!("hostlink.clear {}\n", CHANNEL_WORKSPACES));
    }
    eprintln!("exiting");
}

// --- Tests ------------------------------------------------------------------

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
    fn frame_format() {
        assert_eq!(format_set(0, &[17, 1, 0]), "hostlink.set 0 3 17 1 0\n");
    }

    #[test]
    fn named_and_high_workspaces_ignored() {
        assert_eq!(ws_index(11, "11"), None);
        assert_eq!(ws_index(-1337, "scratch"), None);
        assert_eq!(ws_index(10, "10"), Some(9));
    }
}
