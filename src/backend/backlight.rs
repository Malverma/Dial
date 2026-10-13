//! Laptop panel backlight via sysfs, written through systemd-logind (no root needed).

use std::path::PathBuf;

use gtk::gio;
use gtk::glib;

use super::{Backend, Kind};
use crate::curve;

pub struct Backlight {
    device: String,
    max: u32,
    gamma: f64,
}

impl Backlight {
    /// Opens the device and returns it with its current brightness in percent.
    pub fn open(device: &str, gamma: f64) -> Result<(Self, u8), String> {
        let dir = PathBuf::from("/sys/class/backlight").join(device);
        let read = |f: &str| -> Result<u32, String> {
            std::fs::read_to_string(dir.join(f))
                .map_err(|e| format!("{f}: {e}"))?
                .trim()
                .parse()
                .map_err(|e| format!("{f}: {e}"))
        };
        let max = read("max_brightness")?;
        let current = read("brightness")?;
        let pct = curve::from_native(current, max, gamma);
        Ok((Self { device: device.to_string(), max, gamma }, pct))
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    fn set_via_logind(&self, value: u32) -> Result<(), glib::Error> {
        let bus = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)?;
        bus.call_sync(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1/session/auto",
            "org.freedesktop.login1.Session",
            "SetBrightness",
            Some(&("backlight", self.device.as_str(), value).into()),
            None,
            gio::DBusCallFlags::NONE,
            1000,
            gio::Cancellable::NONE,
        )?;
        Ok(())
    }
}

impl Backend for Backlight {
    fn kind(&self) -> Kind {
        Kind::Backlight
    }

    fn set(&mut self, pct: u8) -> Result<(), String> {
        let value = curve::to_native(pct, self.max, self.gamma);
        self.set_via_logind(value).or_else(|e| {
            // Fallback for sessions without logind (e.g. a udev rule granting write access).
            let path = PathBuf::from("/sys/class/backlight").join(&self.device).join("brightness");
            std::fs::write(path, value.to_string()).map_err(|io| format!("logind: {e}; sysfs: {io}"))
        })
    }
}
