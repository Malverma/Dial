//! Brightness backends. Each monitor uses exactly one.

pub mod backlight;
pub mod ddc;
pub mod gamma;
pub mod overlay;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Backlight,
    Ddc,
    Gamma,
    Overlay,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Backlight => "Backlight",
            Kind::Ddc => "DDC/CI",
            Kind::Gamma => "Gamma",
            Kind::Overlay => "Overlay",
        }
    }

    /// Hardware backends keep their value after Dimmer exits.
    pub fn is_persistent(self) -> bool {
        matches!(self, Kind::Backlight | Kind::Ddc)
    }

    /// Software dimming below this makes the screen unreadable, so it can't be overridden.
    pub fn hard_min(self) -> u8 {
        if self.is_persistent() { 0 } else { 5 }
    }

    pub fn default_curve(self) -> f64 {
        match self {
            Kind::Backlight => 2.2,
            _ => 1.0,
        }
    }
}

pub trait Backend {
    fn kind(&self) -> Kind;
    /// Apply brightness 0–100. Must not block for long; slow backends queue the value.
    fn set(&mut self, pct: u8) -> Result<(), String>;
}
