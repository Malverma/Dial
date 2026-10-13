//! Monitor enumeration and sysfs lookups (EDID, backlight device, I²C bus).

use std::path::{Path, PathBuf};

use gtk::gdk;
use gtk::prelude::*;

use crate::edid;

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorInfo {
    /// DRM connector name, e.g. `DP-1`. Used as the monitor's key.
    pub connector: String,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub description: Option<String>,
    /// Position in the desktop layout, used for left-to-right ordering.
    pub x: i32,
    pub y: i32,
}

impl MonitorInfo {
    pub fn from_gdk(monitor: &gdk::Monitor) -> Option<Self> {
        let connector = monitor.connector()?.to_string();
        let geometry = monitor.geometry();
        let parsed = edid_bytes(&connector).and_then(|d| edid::parse(&d)).unwrap_or_default();
        Some(Self {
            make: parsed.make.or_else(|| monitor.manufacturer().map(Into::into)),
            model: parsed.model.or_else(|| monitor.model().map(Into::into)),
            serial: parsed.serial,
            description: monitor.description().map(Into::into),
            x: geometry.x(),
            y: geometry.y(),
            connector,
        })
    }

    /// Human-readable make and model for tooltips.
    pub fn pretty_name(&self) -> String {
        match (&self.make, &self.model) {
            (Some(a), Some(b)) => format!("{a} {b}"),
            (Some(a), None) | (None, Some(a)) => a.clone(),
            _ => self.description.clone().unwrap_or_else(|| self.connector.clone()),
        }
    }
}

/// All monitors GDK knows about, sorted left to right, then top to bottom.
pub fn gdk_monitors() -> Vec<(MonitorInfo, gdk::Monitor)> {
    let Some(display) = gdk::Display::default() else { return Vec::new() };
    let list = display.monitors();
    let mut out: Vec<_> = (0..list.n_items())
        .filter_map(|i| list.item(i).and_downcast::<gdk::Monitor>())
        .filter_map(|m| MonitorInfo::from_gdk(&m).map(|info| (info, m)))
        .collect();
    out.sort_by_key(|(i, _)| (i.x, i.y));
    out
}

/// `/sys/class/drm/cardN-<connector>`.
pub fn drm_connector_dir(connector: &str) -> Option<PathBuf> {
    std::fs::read_dir("/sys/class/drm").ok()?.flatten().find_map(|e| {
        let name = e.file_name();
        let name = name.to_str()?;
        let (card, conn) = name.split_once('-')?;
        (card.starts_with("card") && conn == connector).then(|| e.path())
    })
}

pub fn edid_bytes(connector: &str) -> Option<Vec<u8>> {
    let data = std::fs::read(drm_connector_dir(connector)?.join("edid")).ok()?;
    (!data.is_empty()).then_some(data)
}

/// Connectors whose EDID starts with `edid` (its first 128 bytes).
pub fn connectors_with_edid(edid: &[u8]) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let (card, conn) = name.split_once('-')?;
            if !card.starts_with("card") {
                return None;
            }
            let data = std::fs::read(e.path().join("edid")).ok()?;
            (data.len() >= edid.len() && data[..edid.len()] == *edid).then(|| conn.to_string())
        })
        .collect()
}

/// The `/sys/class/backlight` device driving this connector, if any.
pub fn backlight_for(connector: &str) -> Option<String> {
    let devices: Vec<String> = std::fs::read_dir("/sys/class/backlight")
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    if devices.is_empty() {
        return None;
    }
    if let Some(dir) = drm_connector_dir(connector) {
        for d in &devices {
            if dir.join(d).exists() {
                return Some(d.clone());
            }
        }
    }
    // Drivers like nvidia/acpi_video don't link the backlight to a connector.
    // Fall back to the only backlight when this is the only internal panel.
    let internal = ["eDP", "LVDS", "DSI"];
    if internal.iter().any(|p| connector.starts_with(p)) && devices.len() == 1 {
        return devices.into_iter().next();
    }
    None
}

/// The I²C bus sysfs links to this connector (works for i915/amdgpu, not nvidia).
pub fn i2c_for(connector: &str) -> Option<PathBuf> {
    let dir = drm_connector_dir(connector)?;
    if let Ok(target) = std::fs::read_link(dir.join("ddc"))
        && let Some(name) = target.file_name().and_then(|n| n.to_str()) {
            return dev_path(name);
        }
    std::fs::read_dir(&dir).ok()?.flatten().find_map(|e| {
        let name = e.file_name().into_string().ok()?;
        name.starts_with("i2c-").then(|| dev_path(&name)).flatten()
    })
}

fn dev_path(bus: &str) -> Option<PathBuf> {
    let p = Path::new("/dev").join(bus);
    p.exists().then_some(p)
}

/// Candidate `/dev/i2c-N` buses that may host a monitor, skipping SMBus and
/// other non-display adapters that must never be written to.
pub fn display_i2c_buses() -> Vec<PathBuf> {
    const SKIP: [&str; 6] = ["SMBus", "SMU", "smu", "i2c-designware", "Synopsys", "PIIX4"];
    let Ok(entries) = std::fs::read_dir("/sys/class/i2c-dev") else { return Vec::new() };
    let mut buses: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            let name = std::fs::read_to_string(e.path().join("name")).unwrap_or_default();
            !SKIP.iter().any(|s| name.contains(s))
        })
        .filter_map(|e| dev_path(e.file_name().to_str()?))
        .collect();
    buses.sort();
    buses
}

/// Whether the i2c-dev module is loaded at all.
pub fn i2c_dev_loaded() -> bool {
    Path::new("/sys/class/i2c-dev").exists()
}
