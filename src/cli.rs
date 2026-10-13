//! `linuxdimmer list|get|set`. Runs inside the GUI when it's open (commands are
//! forwarded over D-Bus by GApplication), otherwise headless for hardware backends.

use crate::backend::backlight::Backlight;
use crate::backend::ddc::DdcDevice;
use crate::backend::{Backend, Kind};
use crate::config::{BackendPref, Config, DEFAULT_MIN};
use crate::discovery;
use crate::model::{self, Target};

pub const USAGE: &str = "\
Usage:
  linuxdimmer                         Open the window
  linuxdimmer list [--verbose]        List monitors, backend and brightness
  linuxdimmer get [MONITOR]           Print brightness
  linuxdimmer set <MONITOR|all> <N>   Set brightness: 60, +10 or -10

MONITOR is a connector name (DP-1), a configured name, or a 1-based index.
Gamma and overlay dimming need the window to be running; commands are then
forwarded to it.";

pub struct Row {
    pub connector: String,
    pub label: String,
    pub description: String,
    pub backend: String,
    pub value: Option<u8>,
    pub detail: Option<String>,
    pub note: Option<String>,
}

pub trait Controller {
    fn rows(&self) -> Vec<Row>;
    fn set(&mut self, connector: &str, target: Target) -> Result<u8, String>;
}

/// Runs a command; returns the exit code.
pub fn run(ctrl: &mut dyn Controller, args: &[String], out: &mut dyn FnMut(&str), err: &mut dyn FnMut(&str)) -> i32 {
    let rows = ctrl.rows();
    match args.first().map(String::as_str) {
        Some("list") => {
            let verbose = args.iter().any(|a| a == "--verbose" || a == "-v");
            if rows.is_empty() {
                err("no monitors found\n");
                return 1;
            }
            for (i, r) in rows.iter().enumerate() {
                let value = r.value.map(|v| format!("{v}%")).unwrap_or("–".into());
                let name = if r.label != r.connector { format!("{} ({})", r.label, r.connector) } else { r.connector.clone() };
                out(&format!("{}  {:<20} {:>5}  {:<11} {}\n", i + 1, name, value, r.backend, r.description));
                if verbose {
                    if let Some(d) = &r.detail {
                        out(&format!("     device: {d}\n"));
                    }
                    if let Some(n) = &r.note {
                        out(&format!("     note:   {n}\n"));
                    }
                }
            }
            0
        }
        Some("get") => {
            let selected = match args.get(1) {
                Some(sel) => match select(&rows, sel) {
                    Ok(s) => s,
                    Err(e) => {
                        err(&format!("{e}\n"));
                        return 2;
                    }
                },
                None => (0..rows.len()).collect(),
            };
            let single = args.get(1).is_some() && selected.len() == 1;
            let mut code = 0;
            for i in selected {
                let r = &rows[i];
                match (r.value, single) {
                    (Some(v), true) => out(&format!("{v}\n")),
                    (Some(v), false) => out(&format!("{} {v}\n", r.connector)),
                    (None, _) => {
                        err(&format!("{}: {}\n", r.connector, r.note.as_deref().unwrap_or("brightness unknown")));
                        code = 1;
                    }
                }
            }
            code
        }
        Some("set") => {
            let (Some(sel), Some(val)) = (args.get(1), args.get(2)) else {
                err(&format!("set needs a monitor and a value\n\n{USAGE}\n"));
                return 2;
            };
            let Some(target) = Target::parse(val) else {
                err(&format!("invalid value '{val}': use 0–100, +N or -N\n"));
                return 2;
            };
            let selected = match select(&rows, sel) {
                Ok(s) => s,
                Err(e) => {
                    err(&format!("{e}\n"));
                    return 2;
                }
            };
            let mut ok = 0;
            for &i in &selected {
                match ctrl.set(&rows[i].connector, target) {
                    Ok(_) => ok += 1,
                    Err(e) => err(&format!("{}: {e}\n", rows[i].connector)),
                }
            }
            if ok == 0 { 1 } else { 0 }
        }
        Some(other) => {
            err(&format!("unknown command '{other}'\n\n{USAGE}\n"));
            2
        }
        None => 0,
    }
}

fn select(rows: &[Row], sel: &str) -> Result<Vec<usize>, String> {
    if sel.eq_ignore_ascii_case("all") {
        return Ok((0..rows.len()).collect());
    }
    if let Some(i) = rows.iter().position(|r| r.connector.eq_ignore_ascii_case(sel) || r.label.eq_ignore_ascii_case(sel)) {
        return Ok(vec![i]);
    }
    match sel.parse::<usize>() {
        Ok(n) if (1..=rows.len()).contains(&n) => Ok(vec![n - 1]),
        _ => Err(format!("no monitor named '{sel}' (see `linuxdimmer list`)")),
    }
}

enum Device {
    Backlight(Backlight),
    Ddc(DdcDevice),
    None,
}

struct HeadlessMonitor {
    connector: String,
    label: String,
    description: String,
    device: Device,
    value: Option<u8>,
    floor: u8,
    detail: Option<String>,
    note: Option<String>,
}

/// Hardware-only controller used when the GUI isn't running.
pub struct Headless {
    monitors: Vec<HeadlessMonitor>,
}

impl Headless {
    pub fn discover(config: &Config) -> Self {
        let monitors = discovery::gdk_monitors()
            .into_iter()
            .map(|(info, _)| {
                let s = config.settings_for(&info);
                let mut m = HeadlessMonitor {
                    connector: info.connector.clone(),
                    label: s.name.clone().unwrap_or_else(|| info.connector.clone()),
                    description: info.pretty_name(),
                    device: Device::None,
                    value: None,
                    floor: model::floor(s.min, DEFAULT_MIN, 0),
                    detail: None,
                    note: None,
                };
                let curve = |k: Kind| s.curve_gamma.unwrap_or(k.default_curve());
                let mut notes = Vec::new();
                if matches!(s.backend, BackendPref::Auto | BackendPref::Backlight)
                    && let Some(dev) = discovery::backlight_for(&info.connector) {
                        match Backlight::open(&dev, curve(Kind::Backlight)) {
                            Ok((b, v)) => {
                                m.detail = Some(format!("/sys/class/backlight/{dev}"));
                                m.device = Device::Backlight(b);
                                m.value = Some(v);
                                return m;
                            }
                            Err(e) => notes.push(format!("backlight: {e}")),
                        }
                    }
                if matches!(s.backend, BackendPref::Auto | BackendPref::Ddc) {
                    match DdcDevice::open(&info.connector, curve(Kind::Ddc)).and_then(|mut d| d.get().map(|v| (d, v))) {
                        Ok((d, v)) => {
                            m.detail = Some(d.bus().display().to_string());
                            m.device = Device::Ddc(d);
                            m.value = Some(v);
                            return m;
                        }
                        Err(e) => notes.push(format!("DDC/CI unavailable: {e}")),
                    }
                }
                notes.push("software dimming requires the LinuxDimmer window to be running".into());
                m.note = Some(notes.join("; "));
                m
            })
            .collect();
        Self { monitors }
    }
}

impl Controller for Headless {
    fn rows(&self) -> Vec<Row> {
        self.monitors
            .iter()
            .map(|m| Row {
                connector: m.connector.clone(),
                label: m.label.clone(),
                description: m.description.clone(),
                backend: match m.device {
                    Device::Backlight(_) => Kind::Backlight.label(),
                    Device::Ddc(_) => Kind::Ddc.label(),
                    Device::None => "Software",
                }
                .to_string(),
                value: m.value,
                detail: m.detail.clone(),
                note: m.note.clone(),
            })
            .collect()
    }

    fn set(&mut self, connector: &str, target: Target) -> Result<u8, String> {
        let m = self.monitors.iter_mut().find(|m| m.connector == connector).ok_or("no such monitor")?;
        let value = target.resolve(m.value.unwrap_or(100), m.floor);
        match &mut m.device {
            Device::Backlight(b) => b.set(value)?,
            Device::Ddc(d) => d.set(value)?,
            Device::None => return Err(m.note.clone().unwrap_or_default()),
        }
        m.value = Some(value);
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<(String, Option<u8>)>);

    impl Controller for Fake {
        fn rows(&self) -> Vec<Row> {
            self.0
                .iter()
                .map(|(c, v)| Row {
                    connector: c.clone(),
                    label: c.clone(),
                    description: String::new(),
                    backend: "Gamma".into(),
                    value: *v,
                    detail: None,
                    note: None,
                })
                .collect()
        }

        fn set(&mut self, connector: &str, target: Target) -> Result<u8, String> {
            let e = self.0.iter_mut().find(|(c, _)| c == connector).unwrap();
            let v = target.resolve(e.1.ok_or("unsupported")?, 10);
            e.1 = Some(v);
            Ok(v)
        }
    }

    fn run_cmd(ctrl: &mut Fake, args: &[&str]) -> (i32, String, String) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (String::new(), String::new());
        let code = run(ctrl, &args, &mut |s| out.push_str(s), &mut |s| err.push_str(s));
        (code, out, err)
    }

    fn fake() -> Fake {
        Fake(vec![("DP-1".into(), Some(50)), ("HDMI-A-1".into(), Some(80)), ("DP-2".into(), None)])
    }

    #[test]
    fn set_all_relative() {
        let mut f = fake();
        let (code, _, err) = run_cmd(&mut f, &["set", "all", "+30"]);
        assert_eq!(code, 0);
        assert!(err.contains("DP-2"));
        assert_eq!(f.0[0].1, Some(80));
        assert_eq!(f.0[1].1, Some(100));
    }

    #[test]
    fn set_by_name_and_index() {
        let mut f = fake();
        assert_eq!(run_cmd(&mut f, &["set", "hdmi-a-1", "5"]).0, 0);
        assert_eq!(f.0[1].1, Some(10), "clamped to floor");
        assert_eq!(run_cmd(&mut f, &["set", "1", "-10"]).0, 0);
        assert_eq!(f.0[0].1, Some(40));
    }

    #[test]
    fn get_single_prints_bare_value() {
        let mut f = fake();
        assert_eq!(run_cmd(&mut f, &["get", "DP-1"]), (0, "50\n".into(), String::new()));
    }

    #[test]
    fn errors() {
        let mut f = fake();
        assert_eq!(run_cmd(&mut f, &["set", "DP-9", "50"]).0, 2);
        assert_eq!(run_cmd(&mut f, &["set", "DP-1", "abc"]).0, 2);
        assert_eq!(run_cmd(&mut f, &["set", "DP-2", "50"]).0, 1);
        assert_eq!(run_cmd(&mut f, &["frobnicate"]).0, 2);
    }
}
