//! Microphone mute state from PipeWire, via `pactl` (PipeWire's PulseAudio
//! compatibility layer). Runs only while OBS is connected: `pactl subscribe`
//! blocks silently until something changes, so this costs nothing when idle.
//!
//! We watch the default source (`@DEFAULT_SOURCE@`), which is the microphone
//! OBS's "Mic" input uses provided that input follows the system default.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;

use crate::obs::SharedObs;
use crate::Msg;

static WARNED: AtomicBool = AtomicBool::new(false);

pub(crate) struct AudioWatcher {
    child: Option<Child>,
    stop: Arc<AtomicBool>,
}

impl AudioWatcher {
    pub(crate) fn start(shared: SharedObs, tx: Sender<Msg>) -> AudioWatcher {
        let stop = Arc::new(AtomicBool::new(false));
        let spawned = Command::new("pactl")
            .arg("subscribe")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                if !WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!("cannot run pactl ({e}); PipeWire mic mute will not be shown");
                }
                return AudioWatcher { child: None, stop };
            }
        };
        let out = child.stdout.take().expect("piped stdout");
        let t_stop = Arc::clone(&stop);
        thread::spawn(move || {
            update(&shared, &tx, &t_stop);
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                // "Event 'change' on source #52" / "Event 'change' on server"
                // (not "source-output", which is a different object type)
                let words: Vec<&str> = line.split_whitespace().collect();
                if matches!(words.get(3), Some(&"source") | Some(&"server")) {
                    update(&shared, &tx, &t_stop);
                }
            }
        });
        AudioWatcher { child: Some(child), stop }
    }
}

impl Drop for AudioWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(c) = self.child.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn update(shared: &SharedObs, tx: &Sender<Msg>, stop: &AtomicBool) {
    let Some(muted) = default_source_muted() else { return };
    let mut s = shared.lock().unwrap();
    // Checked under the lock: the OBS thread sets `stop` before it clears the
    // shared state, so we can never resurrect state after a disconnect.
    if stop.load(Ordering::SeqCst) {
        return;
    }
    if s.audio_muted != muted {
        s.audio_muted = muted;
        drop(s);
        let _ = tx.send(Msg::Obs);
    }
}

fn default_source_muted() -> Option<bool> {
    let out = Command::new("pactl")
        .args(["get-source-mute", "@DEFAULT_SOURCE@"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_mute(&String::from_utf8_lossy(&out.stdout))
}

fn parse_mute(text: &str) -> Option<bool> {
    let v = text.lines().find_map(|l| l.trim().strip_prefix("Mute:"))?;
    match v.trim() {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pactl_output() {
        assert_eq!(parse_mute("Mute: yes\n"), Some(true));
        assert_eq!(parse_mute("Mute: no\n"), Some(false));
        assert_eq!(parse_mute("garbage"), None);
        assert_eq!(parse_mute(""), None);
    }
}
