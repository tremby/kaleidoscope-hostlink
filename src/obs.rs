//! OBS Studio integration over obs-websocket v5 (JSON).
//!
//! One thread keeps a connection to OBS and boils its state down to a small
//! frame for the keyboard (channel 1). It only tries to connect while a
//! keyboard is attached, and blocks (no wakeups at all) while there is none.
//! While connected it is purely event driven, with a slow re-fetch as a safety
//! net. Connecting is a single local TCP connect, which fails instantly with
//! "connection refused" when OBS isn't running, so retrying is very cheap.

use std::convert::Infallible;
use std::env;
use std::fs;
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::{Message, WebSocket};

use crate::audio::AudioWatcher;
use crate::Msg;

pub(crate) const CHANNEL_OBS: u8 = 1;

// --- What to watch (edit to taste) --------------------------------------------

const OBS_ADDR: &str = "127.0.0.1:4455";

/// The OBS audio input whose mute state matters.
const MIC_INPUT: &str = "Mic";

const SCENE_MAIN: &str = "Main scene"; // key 1
const SCENE_CAMERA: &str = "Camera scene"; // key 2
const SCENE_NOTES: &str = "Notes scene"; // key 3

/// Key 4: scene items that are normally VISIBLE when their scene is showing.
const CAMERA_ITEMS: &[&str] = &["Transparent camera"];
const CAMERA_ITEMS_NORMALLY_VISIBLE: bool = true;
/// Key 5: scene items (groups) that are normally HIDDEN when their scene is showing.
const NOTES_ITEMS: &[&str] = &["Digital notes overlay (game)", "Digital notes overlay (notes)"];
const NOTES_ITEMS_NORMALLY_VISIBLE: bool = false;

// --- Timing -------------------------------------------------------------------

/// Retry interval after losing OBS or failing to connect.
const RETRY: Duration = Duration::from_secs(2);
/// Retry interval after an authentication problem (don't hammer OBS's log).
const AUTH_RETRY: Duration = Duration::from_secs(30);
/// Re-fetch everything at least this often, as a safety net.
const RESYNC: Duration = Duration::from_secs(10);
/// Timeout for the handshake and for each request/response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// Wait this long for more events before re-fetching.
const DEBOUNCE: Duration = Duration::from_millis(15);

/// Scenes (4) | Inputs (8) | SceneItems (128). Deliberately not the high
/// volume categories (volume meters etc).
const EVENT_SUBSCRIPTIONS: u32 = (1 << 2) | (1 << 3) | (1 << 7);

const RELEVANT_EVENTS: &[&str] = &[
    "CurrentProgramSceneChanged",
    "SceneItemEnableStateChanged",
    "SceneItemCreated",
    "SceneItemRemoved",
    "SceneItemListReindexed",
    "SceneListChanged",
    "SceneNameChanged",
    "SceneRemoved",
    "InputMuteStateChanged",
    "InputNameChanged",
    "InputCreated",
    "InputRemoved",
];

// --- Frame (must match Hostlink.h, HostlinkObs) --------------------------------

/// Per-key flag bits. Frame layout: keys 1..5, then the M key.
pub(crate) const ACTIVE: u8 = 1 << 0; // keys 1-3: the associated scene is the live scene
pub(crate) const ALERT: u8 = 1 << 1; // keys 4-5: scene item in an exceptional state
pub(crate) const MUTED: u8 = 1 << 2; // M key: microphone muted

#[derive(Default, Clone, PartialEq, Debug)]
pub(crate) struct ObsState {
    pub connected: bool,
    pub keys: [u8; 5],
    pub obs_mic_muted: bool,
    pub audio_muted: bool,
}

impl ObsState {
    /// The frame to send, or None when there is no OBS connection.
    pub(crate) fn frame(&self) -> Option<[u8; 6]> {
        if !self.connected {
            return None;
        }
        let mut f = [0u8; 6];
        f[..5].copy_from_slice(&self.keys);
        if self.obs_mic_muted || self.audio_muted {
            f[5] |= MUTED;
        }
        Some(f)
    }
}

pub(crate) type SharedObs = Arc<Mutex<ObsState>>;

/// Is any matching item in an exceptional state (its visibility differs from
/// what's normal)? Only items in the live scene are passed in.
fn item_alert(items: &[(String, bool)], names: &[&str], normally_visible: bool) -> bool {
    items
        .iter()
        .any(|(n, enabled)| names.contains(&n.as_str()) && *enabled != normally_visible)
}

pub(crate) fn compute_keys(live_scene: &str, live_scene_items: &[(String, bool)]) -> [u8; 5] {
    let mut k = [0u8; 5];
    if live_scene == SCENE_MAIN {
        k[0] |= ACTIVE;
    }
    if live_scene == SCENE_CAMERA {
        k[1] |= ACTIVE;
    }
    if live_scene == SCENE_NOTES {
        k[2] |= ACTIVE;
    }
    if item_alert(live_scene_items, CAMERA_ITEMS, CAMERA_ITEMS_NORMALLY_VISIBLE) {
        k[3] |= ALERT;
    }
    if item_alert(live_scene_items, NOTES_ITEMS, NOTES_ITEMS_NORMALLY_VISIBLE) {
        k[4] |= ALERT;
    }
    k
}

// --- Password -----------------------------------------------------------------

fn default_password_file() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("kaleidoscope-hostlink").join("obs-password"))
}

/// Read fresh on every connection attempt, so editing the file needs no
/// restart. Precedence: --obs-password-file, $OBS_WEBSOCKET_PASSWORD, default file.
fn load_password(explicit_file: &Option<PathBuf>) -> Option<String> {
    let read = |p: &PathBuf| {
        fs::read_to_string(p).ok().map(|s| s.trim_end_matches(['\n', '\r']).to_string())
    };
    if let Some(p) = explicit_file {
        return read(p);
    }
    if let Ok(v) = env::var("OBS_WEBSOCKET_PASSWORD") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    default_password_file().and_then(|p| read(&p))
}

fn auth_string(password: &str, salt: &str, challenge: &str) -> String {
    let secret = STANDARD.encode(Sha256::digest(format!("{password}{salt}")));
    STANDARD.encode(Sha256::digest(format!("{secret}{challenge}")))
}

// --- Connection ---------------------------------------------------------------

enum End {
    /// Couldn't even connect (OBS not running). Expected; stay quiet.
    Refused,
    /// Authentication problem; retry slowly.
    Auth(String),
    /// Connected (or partly) and then lost / protocol trouble.
    Lost(String),
}

impl End {
    fn msg(&self) -> String {
        match self {
            End::Refused => "connection refused".into(),
            End::Auth(m) | End::Lost(m) => m.clone(),
        }
    }
}

fn lost<E: std::fmt::Display>(e: E) -> End {
    End::Lost(e.to_string())
}

type Ws = WebSocket<TcpStream>;

/// Read one JSON message. Ok(None) means the read timed out.
fn read_json(ws: &mut Ws) -> Result<Option<Value>, End> {
    loop {
        match ws.read() {
            Ok(msg @ Message::Text(_)) => {
                let text = msg.to_text().map_err(lost)?;
                return serde_json::from_str(text).map(Some).map_err(lost);
            }
            Ok(Message::Close(frame)) => {
                let _ = ws.flush(); // send our half of the close handshake
                let why = frame.map(|f| f.reason.to_string()).unwrap_or_default();
                return Err(End::Lost(format!("OBS closed the connection {why}").trim().to_string()));
            }
            Ok(_) => continue, // ping/pong/binary
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(lost(e)),
        }
    }
}

struct Conn {
    ws: Ws,
    next_id: u64,
    dirty: bool,
}

impl Conn {
    fn set_timeout(&self, d: Duration) {
        let _ = self.ws.get_ref().set_read_timeout(Some(d));
    }

    fn note_event(&mut self, v: &Value) {
        if v["op"] == 5 {
            if let Some(t) = v["d"]["eventType"].as_str() {
                if RELEVANT_EVENTS.contains(&t) {
                    self.dirty = true;
                }
            }
        }
    }

    /// Ok(None) means OBS reported the request as failed (e.g. no such input).
    fn request(&mut self, kind: &str, data: Value) -> Result<Option<Value>, End> {
        self.next_id += 1;
        let id = self.next_id.to_string();
        let msg = json!({"op": 6, "d": {"requestType": kind, "requestId": id, "requestData": data}});
        self.ws.send(Message::Text(msg.to_string().into())).map_err(lost)?;
        self.set_timeout(REQUEST_TIMEOUT);
        loop {
            let Some(v) = read_json(&mut self.ws)? else {
                return Err(End::Lost(format!("OBS did not answer {kind}")));
            };
            self.note_event(&v);
            if v["op"] == 7 && v["d"]["requestId"] == id.as_str() {
                let ok = v["d"]["requestStatus"]["result"].as_bool().unwrap_or(false);
                return Ok(ok.then(|| v["d"]["responseData"].clone()));
            }
        }
    }
}

/// Fetch the live scene, its items, and the Mic mute state; publish if changed.
fn refresh(conn: &mut Conn, shared: &SharedObs, tx: &Sender<Msg>) -> Result<(), End> {
    let scene = conn
        .request("GetCurrentProgramScene", json!({}))?
        .and_then(|d| {
            d["sceneName"]
                .as_str()
                .or_else(|| d["currentProgramSceneName"].as_str())
                .map(String::from)
        })
        .unwrap_or_default();

    let items: Vec<(String, bool)> = conn
        .request("GetSceneItemList", json!({"sceneName": scene}))?
        .map(|d| {
            d["sceneItems"]
                .as_array()
                .map(|a| a.as_slice())
                .unwrap_or(&[])
                .iter()
                .map(|i| {
                    (
                        i["sourceName"].as_str().unwrap_or("").to_string(),
                        i["sceneItemEnabled"].as_bool().unwrap_or(false),
                    )
                })
                .collect()
        })
        .unwrap_or_default();

    let mic_muted = conn
        .request("GetInputMute", json!({"inputName": MIC_INPUT}))?
        .and_then(|d| d["inputMuted"].as_bool())
        .unwrap_or(false);

    let keys = compute_keys(&scene, &items);
    let mut s = shared.lock().unwrap();
    let old = s.clone();
    s.connected = true;
    s.keys = keys;
    s.obs_mic_muted = mic_muted;
    if *s != old {
        drop(s);
        let _ = tx.send(Msg::Obs);
    }
    Ok(())
}

fn session(
    shared: &SharedObs,
    ctl: &Receiver<bool>,
    kb: &mut bool,
    tx: &Sender<Msg>,
    password_file: &Option<PathBuf>,
    connected: &mut bool,
) -> Result<Infallible, End> {
    let addr: SocketAddr = OBS_ADDR.parse().expect("valid OBS_ADDR");
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|_| End::Refused)?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(REQUEST_TIMEOUT)).map_err(lost)?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT)).map_err(lost)?;

    let mut request = format!("ws://{OBS_ADDR}")
        .into_client_request()
        .map_err(|e| End::Lost(format!("bad OBS url: {e}")))?;
    request
        .headers_mut()
        .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("obswebsocket.json"));
    let (mut ws, _) = tungstenite::client(request, stream)
        .map_err(|e| End::Lost(format!("websocket handshake failed: {e}")))?;

    let hello = match read_json(&mut ws)? {
        Some(v) if v["op"] == 0 => v,
        _ => return Err(End::Lost("no Hello from OBS".into())),
    };

    let mut identify = json!({"op": 1, "d": {"rpcVersion": 1, "eventSubscriptions": EVENT_SUBSCRIPTIONS}});
    let auth = &hello["d"]["authentication"];
    let authed = auth.is_object();
    if authed {
        let Some(pw) = load_password(password_file) else {
            return Err(End::Auth(
                "OBS requires a password: put it in ~/.config/kaleidoscope-hostlink/obs-password \
                 (or use --obs-password-file / $OBS_WEBSOCKET_PASSWORD)"
                    .into(),
            ));
        };
        let salt = auth["salt"].as_str().unwrap_or("");
        let challenge = auth["challenge"].as_str().unwrap_or("");
        identify["d"]["authentication"] = json!(auth_string(&pw, salt, challenge));
    }
    ws.send(Message::Text(identify.to_string().into())).map_err(lost)?;

    loop {
        match read_json(&mut ws) {
            Ok(Some(v)) if v["op"] == 2 => break,
            Ok(Some(_)) => continue,
            Ok(None) => return Err(End::Lost("OBS did not confirm identification".into())),
            Err(e) if authed => {
                return Err(End::Auth(format!("authentication failed (wrong password?): {}", e.msg())))
            }
            Err(e) => return Err(e),
        }
    }

    *connected = true;
    eprintln!("OBS connected");
    let mut conn = Conn { ws, next_id: 0, dirty: false };
    refresh(&mut conn, shared, tx)?;
    // Dropped (killing pactl) when this function returns, before the caller
    // clears the shared state.
    let _audio = AudioWatcher::start(Arc::clone(shared), tx.clone());

    loop {
        while let Ok(v) = ctl.try_recv() {
            *kb = v;
        }
        if !conn.dirty {
            conn.set_timeout(RESYNC);
            match read_json(&mut conn.ws)? {
                Some(v) => conn.note_event(&v),
                None => conn.dirty = true, // periodic resync
            }
        }
        if conn.dirty {
            // Swallow the rest of a burst of events before re-fetching.
            conn.set_timeout(DEBOUNCE);
            while let Some(v) = read_json(&mut conn.ws)? {
                conn.note_event(&v);
            }
            conn.dirty = false;
            refresh(&mut conn, shared, tx)?;
        }
    }
}

/// The OBS thread. `ctl` carries "keyboard attached?" from the main thread.
pub(crate) fn obs_thread(
    shared: SharedObs,
    ctl: Receiver<bool>,
    tx: Sender<Msg>,
    password_file: Option<PathBuf>,
) {
    let mut kb = false;
    let mut last_logged: Option<String> = None;
    loop {
        // No keyboard: block. No timers, no wakeups.
        while !kb {
            match ctl.recv() {
                Ok(v) => kb = v,
                Err(_) => return,
            }
        }

        let mut connected = false;
        let end = match session(&shared, &ctl, &mut kb, &tx, &password_file, &mut connected) {
            Err(e) => e,
            Ok(never) => match never {},
        };

        if connected {
            eprintln!("OBS disconnected: {}", end.msg());
            last_logged = None;
            *shared.lock().unwrap() = ObsState::default();
            let _ = tx.send(Msg::Obs);
        }
        let delay = match &end {
            End::Refused => RETRY,
            End::Auth(m) => {
                if last_logged.as_deref() != Some(m) {
                    eprintln!("OBS: {m}");
                    last_logged = Some(m.clone());
                }
                AUTH_RETRY
            }
            End::Lost(m) => {
                if !connected && last_logged.as_deref() != Some(m) {
                    eprintln!("OBS: {m}");
                    last_logged = Some(m.clone());
                }
                RETRY
            }
        };
        match ctl.recv_timeout(delay) {
            Ok(v) => kb = v,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(v: &[(&str, bool)]) -> Vec<(String, bool)> {
        v.iter().map(|(n, e)| (n.to_string(), *e)).collect()
    }

    #[test]
    fn scene_keys() {
        assert_eq!(compute_keys("Main scene", &[])[0], ACTIVE);
        assert_eq!(compute_keys("Camera scene", &[])[1], ACTIVE);
        assert_eq!(compute_keys("Notes scene", &[])[2], ACTIVE);
        assert_eq!(compute_keys("Something else", &[]), [0; 5]);
    }

    #[test]
    fn camera_item_alerts_when_hidden() {
        // normally visible: visible = fine, hidden = alert, absent = fine
        assert_eq!(compute_keys("x", &items(&[("Transparent camera", true)]))[3], 0);
        assert_eq!(compute_keys("x", &items(&[("Transparent camera", false)]))[3], ALERT);
        assert_eq!(compute_keys("x", &items(&[("Other", false)]))[3], 0);
    }

    #[test]
    fn notes_item_alerts_when_visible() {
        // normally hidden: hidden = fine, visible = alert; either name matches
        let hidden = items(&[("Digital notes overlay (game)", false)]);
        let shown_game = items(&[("Digital notes overlay (game)", true)]);
        let shown_notes = items(&[("Digital notes overlay (notes)", true)]);
        assert_eq!(compute_keys("x", &hidden)[4], 0);
        assert_eq!(compute_keys("x", &shown_game)[4], ALERT);
        assert_eq!(compute_keys("x", &shown_notes)[4], ALERT);
        assert_eq!(compute_keys("x", &[])[4], 0);
    }

    #[test]
    fn mic_muted_from_either_source() {
        let mut s = ObsState { connected: true, ..Default::default() };
        assert_eq!(s.frame().unwrap()[5], 0);
        s.obs_mic_muted = true;
        assert_eq!(s.frame().unwrap()[5], MUTED);
        s.obs_mic_muted = false;
        s.audio_muted = true;
        assert_eq!(s.frame().unwrap()[5], MUTED);
        s.connected = false;
        assert_eq!(s.frame(), None);
    }

    #[test]
    fn auth_string_matches_reference() {
        // Reference value computed independently with Python hashlib/base64.
        assert_eq!(
            auth_string("supersecretpassword", "lM1GncleQOaCu9lT1yeUZhFYnqhsLLP1G5lAGo3ixaI=", "+IxH4CnCiqpX1rM9scsNynZzbOe4KhDeYcTNS3PDaeY="),
            "1Ct943GAT+6YQUUX47Ia/ncufilbe6+oD6lY+5kaCu4="
        );
    }
}
