//! kaleidoscope-hostlink: push desktop state to a Kaleidoscope keyboard for LED effects.
//!
//! Threads:
//!   * event thread   - reads Hyprland's .socket2.sock, tracks urgent windows,
//!                      and tells the main thread when something changed
//!   * OBS thread     - keeps an obs-websocket connection (only while a
//!                      keyboard is attached) and publishes OBS state; while
//!                      connected it also runs a `pactl subscribe` watcher
//!   * signal thread  - turns SIGINT/SIGTERM/SIGHUP into a Quit message
//!   * main thread    - debounces, queries Hyprland, builds the frames, and
//!                      owns the serial port (connect, send, heartbeat, clear)
//!
//! Channels sent to the keyboard: 0 = Hyprland workspaces, 1 = OBS.

mod audio;
mod hyprland;
mod keyboard;
mod obs;

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
use obs::{obs_thread, ObsState, SharedObs, CHANNEL_OBS};

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
    /// Hyprland state may have changed.
    Dirty,
    /// OBS / microphone state changed (read it from the shared state).
    Obs,
    Quit,
}

/// Send one command; on a serial error drop the port so it gets reopened.
/// Returns false if the port was dropped.
/// Send one command. `answered_ok` is per channel, so log lines only appear
/// when *that channel's* behaviour changes (e.g. old firmware without the OBS
/// channel answers `err` for it while workspaces keep working).
fn transmit(
    port: &mut Option<File>,
    line: &str,
    label: &str,
    answered_ok: &mut bool,
    next_connect: &mut Instant,
) -> bool {
    let Some(p) = port.as_mut() else { return false };
    match exchange(p, line) {
        Ok(true) => {
            if !*answered_ok {
                eprintln!("keyboard accepting {label} frames");
            }
            *answered_ok = true;
            true
        }
        Ok(false) => {
            if *answered_ok {
                eprintln!("keyboard did not answer ok to {label} frames (is the current Hostlink.h flashed?)");
            }
            *answered_ok = false;
            true
        }
        Err(e) => {
            eprintln!("keyboard I/O error: {e}; will reconnect");
            *port = None;
            *next_connect = Instant::now() + RETRY;
            false
        }
    }
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
    let mut obs_password_file: Option<PathBuf> = None;
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--device" => explicit_device = args.next().map(PathBuf::from),
            "--obs-password-file" => obs_password_file = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!(
                    "usage: kaleidoscope-hostlink [--device /dev/serial/by-id/...] \
                     [--obs-password-file PATH]\n\
                     OBS password: --obs-password-file, else $OBS_WEBSOCKET_PASSWORD, \
                     else ~/.config/kaleidoscope-hostlink/obs-password"
                );
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
    let obs_shared: SharedObs = Arc::new(Mutex::new(ObsState::default()));
    // Tells the OBS thread whether a keyboard is attached.
    let (obs_ctl_tx, obs_ctl_rx) = mpsc::channel::<bool>();

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
    {
        let shared = Arc::clone(&obs_shared);
        let tx = tx.clone();
        thread::spawn(move || obs_thread(shared, obs_ctl_rx, tx, obs_password_file));
    }

    let mut port: Option<File> = None;
    let mut frame: Option<[u8; NUM_KEYS]> = None;
    let mut last_obs: Option<[u8; 6]> = None; // what the keyboard was last told on the OBS channel
    let mut last_send = Instant::now();
    let mut next_connect = Instant::now();
    let mut last_query = Instant::now();
    let mut kb_ok = true; // for log-on-change (finding/opening the keyboard)
    let mut answered_ok = [true; 2]; // per channel: [workspaces, obs]
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
            Ok(Msg::Obs) => {} // picked up from the shared state below
            Ok(Msg::Dirty) => {
                // Debounce: swallow the rest of the burst.
                loop {
                    match rx.recv_timeout(DEBOUNCE) {
                        Ok(Msg::Dirty) | Ok(Msg::Obs) => {}
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
                    last_obs = None;
                    let _ = obs_ctl_tx.send(true);
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

        if port.is_some() {
            let heartbeat_due = last_send.elapsed() >= HEARTBEAT;
            let obs_now = obs_shared.lock().unwrap().frame();

            // (line, channel index, label)
            let mut lines: Vec<(String, usize, &str)> = Vec::new();
            let mut sent_workspaces = false;
            if let Some(f) = frame {
                if recompute || force_send || heartbeat_due {
                    lines.push((format_set(CHANNEL_WORKSPACES, &f), 0, "workspace"));
                    sent_workspaces = true;
                }
            }
            if obs_now != last_obs || (obs_now.is_some() && (force_send || heartbeat_due)) {
                let line = match obs_now {
                    Some(f) => format_set(CHANNEL_OBS, &f),
                    // OBS went away: cancel its lighting immediately.
                    None => format!("hostlink.clear {}\n", CHANNEL_OBS),
                };
                lines.push((line, 1, "OBS"));
                last_obs = obs_now;
            }

            for (line, ch, label) in &lines {
                if !transmit(&mut port, line, label, &mut answered_ok[*ch], &mut next_connect) {
                    kb_ok = true;
                    answered_ok = [true; 2];
                    last_obs = None;
                    let _ = obs_ctl_tx.send(false);
                    break;
                }
            }
            if heartbeat_due || sent_workspaces {
                last_send = Instant::now();
            }
        }
    }

    // Graceful exit: blank the overlays immediately instead of waiting for the
    // stale timeout.
    if let Some(mut p) = port {
        let _ = exchange(&mut p, &format!("hostlink.clear {}\n", CHANNEL_WORKSPACES));
        let _ = exchange(&mut p, &format!("hostlink.clear {}\n", CHANNEL_OBS));
    }
    eprintln!("exiting");
}
