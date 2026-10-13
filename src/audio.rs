//! Audio outputs and the microphone through `pactl`, which talks to PulseAudio
//! or PipeWire's pulse server.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;

/// Bursts of server events (a stream starting touches several objects) are
/// coalesced into one snapshot.
const DEBOUNCE: Duration = Duration::from_millis(40);
const RETRY: Duration = Duration::from_secs(1);
const NORM: f64 = 65536.0;

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub index: u32,
    pub name: String,
    pub description: String,
    /// Average over channels, in percent.
    pub volume: u8,
    /// Something is playing through it.
    pub running: bool,
    /// For a sink, the source that records what it plays.
    pub monitor: Option<String>,
}

/// One application playback stream (sink input).
#[derive(Debug, Clone, PartialEq)]
pub struct Stream {
    pub index: u32,
    /// Groups every instance of an application: its window class, or its
    /// process binary when it has no window.
    pub app: String,
    /// Human-readable application name.
    pub app_name: String,
    pub volume: u8,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub sinks: Vec<Device>,
    /// The default source (microphone).
    pub source: Option<Device>,
    pub default_sink: Option<String>,
    pub streams: Vec<Stream>,
    /// Apps currently recording from the default source.
    pub recording: Vec<String>,
    /// Apps with an open window, `(app, name)`, whether or not they play audio.
    pub windows: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct RawVolume {
    value: u32,
}

#[derive(Deserialize)]
struct RawDevice {
    index: u32,
    name: String,
    description: String,
    volume: HashMap<String, RawVolume>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    monitor_source: Option<String>,
}

impl From<RawDevice> for Device {
    fn from(d: RawDevice) -> Self {
        Device {
            index: d.index,
            name: d.name,
            description: d.description,
            volume: percent(&d.volume),
            running: d.state == "RUNNING",
            monitor: d.monitor_source,
        }
    }
}

/// A sink input or source output.
#[derive(Deserialize)]
struct RawStream {
    index: u32,
    #[serde(default)]
    sink: Option<u32>,
    #[serde(default)]
    source: Option<u32>,
    #[serde(default)]
    volume: HashMap<String, RawVolume>,
    #[serde(default)]
    properties: HashMap<String, serde_json::Value>,
}

impl RawStream {
    fn prop(&self, key: &str) -> Option<&str> {
        self.properties.get(key)?.as_str().map(str::trim).filter(|s| !s.is_empty())
    }

    /// `(app, name)`, grouped under the window its process belongs to.
    /// `None` for our own meter.
    fn app(&self, windows: &[Window]) -> Option<(String, String)> {
        let name = self.prop("application.name");
        if name == Some(OWN_CLIENT) {
            return None;
        }
        let pid = self.prop("application.process.id").and_then(|p| p.parse().ok());
        let id = match pid.and_then(|p| window_of(p, windows)) {
            Some(class) => class,
            None => {
                let binary = self.prop("application.process.binary");
                // Shared runtimes say nothing about which app it is.
                let generic = binary.is_some_and(|b| GENERIC.iter().any(|g| b.starts_with(g)));
                let id = if generic { name.or(binary) } else { binary.or(name) };
                id.unwrap_or("unknown").to_lowercase()
            }
        };
        // Electron apps often report "Chromium"; prefer the app's own name then.
        let display = match name {
            Some(n) if n.to_lowercase().contains(&id) => n.to_string(),
            _ => class_name(&id),
        };
        Some((id, display))
    }
}

const GENERIC: &[&str] = &["electron", "python", "java", "node", "wine", "chromium"];

struct Window {
    pid: u32,
    /// Lower-case window class.
    class: String,
}

/// Mapped Hyprland windows other than ours.
fn windows() -> Vec<Window> {
    #[derive(Deserialize)]
    struct Client {
        pid: i64,
        class: String,
        #[serde(default = "yes")]
        mapped: bool,
    }
    fn yes() -> bool {
        true
    }
    let Some(text) = Command::new("hyprctl").args(["-j", "clients"]).stderr(Stdio::null()).output().ok() else {
        return Vec::new();
    };
    let own = std::process::id() as i64;
    serde_json::from_slice::<Vec<Client>>(&text.stdout)
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.mapped && c.pid > 0 && c.pid != own && !c.class.is_empty())
        .map(|c| Window { pid: c.pid as u32, class: c.class.to_lowercase() })
        .collect()
}

/// The class of the window owned by `pid` or one of its ancestors.
fn window_of(mut pid: u32, windows: &[Window]) -> Option<String> {
    for _ in 0..32 {
        if let Some(w) = windows.iter().find(|w| w.pid == pid) {
            return Some(w.class.clone());
        }
        pid = parent(pid).filter(|&p| p > 1)?;
    }
    None
}

fn parent(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // "pid (comm) state ppid ..."; comm may contain spaces or parentheses.
    stat[stat.rfind(')')? + 1..].split_whitespace().nth(1)?.parse().ok()
}

/// "org.gnome.Nautilus" -> "Nautilus", "firefox" -> "Firefox".
fn class_name(class: &str) -> String {
    title_case(class.rsplit('.').next().unwrap_or(class))
}

/// Client name the meter's `parec` uses.
pub const OWN_CLIENT: &str = "Dial";

pub fn title_case(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Average over channels, in percent.
fn percent(volume: &HashMap<String, RawVolume>) -> u8 {
    let n = volume.len().max(1) as f64;
    let avg = volume.values().map(|v| v.value as f64).sum::<f64>() / n;
    (avg * 100.0 / NORM).round().min(255.0) as u8
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn devices(kind: &str) -> Option<Vec<Device>> {
    let text = pactl(&["-f", "json", "list", kind])?;
    let raw: Vec<RawDevice> = serde_json::from_str(&text).ok()?;
    Some(raw.into_iter().map(Device::from).collect())
}

fn streams(kind: &str) -> Vec<RawStream> {
    pactl(&["-f", "json", "list", kind]).and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Current devices and streams, or `None` when no server is reachable.
fn snapshot() -> Option<Snapshot> {
    let sinks = devices("sinks")?;
    let default_sink = pactl(&["get-default-sink"]).map(|s| s.trim().to_string());
    let default_source = pactl(&["get-default-source"]).map(|s| s.trim().to_string());
    let source = default_source
        .and_then(|name| devices("sources")?.into_iter().find(|d| d.name == name))
        .filter(|d| !d.name.ends_with(".monitor"));
    let wins = windows();
    let playing = streams("sink-inputs")
        .into_iter()
        .filter(|s| s.sink.is_some())
        .filter_map(|s| {
            let (app, app_name) = s.app(&wins)?;
            Some(Stream { index: s.index, app, app_name, volume: percent(&s.volume) })
        })
        .collect();
    let recording = match &source {
        Some(src) => streams("source-outputs")
            .into_iter()
            .filter(|s| s.source == Some(src.index))
            .filter_map(|s| s.app(&wins).map(|(app, _)| app))
            .collect(),
        None => Vec::new(),
    };
    let mut windows: Vec<(String, String)> = Vec::new();
    for w in &wins {
        if !windows.iter().any(|(id, _)| *id == w.class) {
            windows.push((w.class.clone(), class_name(&w.class)));
        }
    }
    Some(Snapshot { sinks, source, default_sink, streams: playing, recording, windows })
}

/// Events that can change a device's volume or the device list.
fn relevant(line: &str) -> bool {
    ["on sink #", "on source #", "on sink-input #", "on source-output #", "on server", "on card #"].iter().any(|p| line.contains(p))
}

enum Wake {
    Changed,
    /// `pactl subscribe` exited.
    Lost,
}

/// Send a snapshot now and after every relevant server event or window
/// opening/closing. `None` means the server is unavailable.
pub fn watch(tx: async_channel::Sender<Option<Snapshot>>) {
    std::thread::spawn(move || {
        let (wtx, wrx) = mpsc::channel();
        watch_windows(wtx.clone());
        loop {
            let Ok(mut child) = Command::new("pactl")
                .arg("subscribe")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            else {
                // pactl isn't installed.
                let _ = tx.send_blocking(None);
                return;
            };
            let stdout = child.stdout.take().expect("piped stdout");
            let ltx = wtx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if relevant(&line) && ltx.send(Wake::Changed).is_err() {
                        return;
                    }
                }
                let _ = ltx.send(Wake::Lost);
            });

            let mut alive = tx.send_blocking(snapshot()).is_ok();
            let mut lost = false;
            while alive && !lost {
                if !matches!(wrx.recv(), Ok(Wake::Changed)) {
                    break;
                }
                std::thread::sleep(DEBOUNCE);
                lost = wrx.try_iter().any(|w| matches!(w, Wake::Lost));
                alive = tx.send_blocking(snapshot()).is_ok();
            }
            let _ = child.kill();
            let _ = child.wait();
            if !alive {
                return;
            }
            // The server went away (e.g. PipeWire restarted); reconnect.
            let _ = tx.send_blocking(None);
            std::thread::sleep(RETRY);
        }
    });
}

/// Wake on Hyprland window events, so apps without audio get sliders too.
fn watch_windows(tx: mpsc::Sender<Wake>) {
    let Some(socket) = crate::hypr::event_socket() else { return };
    std::thread::spawn(move || {
        for line in BufReader::new(socket).lines().map_while(Result::ok) {
            if (line.starts_with("openwindow>>") || line.starts_with("closewindow>>")) && tx.send(Wake::Changed).is_err() {
                return;
            }
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Sink,
    Source,
    /// An application stream, named by its index.
    SinkInput,
}

/// Applies volumes on a worker thread, keeping only the latest value per
/// device so dragging a slider doesn't queue up `pactl` calls.
pub struct Setter(mpsc::Sender<(Kind, String, u8)>);

impl Setter {
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<(Kind, String, u8)>();
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut jobs = vec![first];
                for job in rx.try_iter() {
                    match jobs.iter_mut().find(|j| j.0 == job.0 && j.1 == job.1) {
                        Some(j) => *j = job,
                        None => jobs.push(job),
                    }
                }
                for (kind, name, pct) in jobs {
                    let cmd = match kind {
                        Kind::Sink => "set-sink-volume",
                        Kind::Source => "set-source-volume",
                        Kind::SinkInput => "set-sink-input-volume",
                    };
                    pactl(&[cmd, &name, &format!("{pct}%")]);
                }
            }
        });
        Self(tx)
    }

    pub fn set(&self, kind: Kind, name: &str, pct: u8) {
        let _ = self.0.send((kind, name.to_string(), pct));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pactl_json() {
        let text = r#"[{"index":1,"name":"out","description":"Speakers",
            "volume":{"front-left":{"value":58974,"value_percent":"90%","db":"-2.75 dB"},
                      "front-right":{"value":58974,"value_percent":"90%","db":"-2.75 dB"}}}]"#;
        let raw: Vec<RawDevice> = serde_json::from_str(text).unwrap();
        let d = Device::from(raw.into_iter().next().unwrap());
        assert_eq!(d.description, "Speakers");
        assert_eq!(d.volume, 90);
    }

    #[test]
    fn groups_streams_by_binary() {
        let text = r#"[{"index":5,"sink":1,"volume":{"mono":{"value":32768}},
            "properties":{"application.name":"Chromium","application.process.binary":"vesktop"}},
            {"index":6,"sink":1,"volume":{"mono":{"value":65536}},
            "properties":{"application.name":"Firefox","application.process.binary":"firefox"}},
            {"index":7,"source":2,"properties":{"application.name":"Dial","application.process.binary":"pacat"}}]"#;
        let raw: Vec<RawStream> = serde_json::from_str(text).unwrap();
        assert_eq!(raw[0].app(&[]), Some(("vesktop".into(), "Vesktop".into())));
        assert_eq!(raw[1].app(&[]), Some(("firefox".into(), "Firefox".into())));
        assert_eq!(raw[2].app(&[]), None);
        assert_eq!(percent(&raw[0].volume), 50);
    }

    #[test]
    fn groups_by_window_class() {
        let text = r#"[{"index":5,"sink":1,"properties":{"application.name":"Chromium",
            "application.process.binary":"electron","application.process.id":"4242"}},
            {"index":6,"sink":1,"properties":{"application.name":"Chromium","application.process.binary":"electron"}}]"#;
        let raw: Vec<RawStream> = serde_json::from_str(text).unwrap();
        let windows = [Window { pid: 4242, class: "cursor".into() }];
        assert_eq!(raw[0].app(&windows), Some(("cursor".into(), "Cursor".into())));
        // Without a window, a shared runtime falls back to the app's name.
        assert_eq!(raw[1].app(&windows), Some(("chromium".into(), "Chromium".into())));
        assert_eq!(class_name("org.gnome.Nautilus"), "Nautilus");
    }

    #[test]
    fn reads_parent_pid() {
        assert_eq!(parent(std::process::id()), Some(std::os::unix::process::parent_id()));
    }

    #[test]
    fn filters_events() {
        assert!(relevant("Event 'change' on sink #439"));
        assert!(relevant("Event 'change' on source #309"));
        assert!(relevant("Event 'change' on server #4294967295"));
        assert!(relevant("Event 'new' on sink-input #12"));
        assert!(!relevant("Event 'new' on client #3"));
    }
}
