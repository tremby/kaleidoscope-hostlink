//! kaleidoscope-hostlink: push desktop state to a Kaleidoscope keyboard for LED effects.
//!
//! Threads:
//!   * event thread   - reads Hyprland's .socket2.sock, tracks urgent windows,
//!                      and tells the main thread when something changed
//!   * signal thread  - turns SIGINT/SIGTERM/SIGHUP into a Quit message
//!   * main thread    - debounces, queries Hyprland, builds the frame, and
//!                      owns the serial port (connect, send, heartbeat, clear)

mod hyprland;
mod keyboard;

use std::collections::HashSet;
use std::env;
use std::fs::File;
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

use hyprland::{event_thread, query_frame, UrgentSet, CHANNEL_WORKSPACES, NUM_KEYS};
use keyboard::{exchange, find_keyboard, format_set, open_keyboard};

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
pub(crate) const RETRY: Duration = Duration::from_secs(1);

pub(crate) enum Msg {
    Dirty,
    Quit,
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

