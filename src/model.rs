//! Pure brightness logic shared by the GUI and CLI.

/// A requested change: absolute percent or a relative step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Absolute(u8),
    Relative(i16),
}

impl Target {
    /// Parses `60`, `60%`, `+10` or `-10`.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim().trim_end_matches('%');
        if let Some(rest) = s.strip_prefix('+') {
            rest.parse::<u8>().ok().map(|n| Target::Relative(n as i16))
        } else if let Some(rest) = s.strip_prefix('-') {
            rest.parse::<u8>().ok().map(|n| Target::Relative(-(n as i16)))
        } else {
            s.parse::<u8>().ok().filter(|&n| n <= 100).map(Target::Absolute)
        }
    }

    /// Resolve against the current value and clamp to `floor..=100`.
    pub fn resolve(self, current: u8, floor: u8) -> u8 {
        let raw = match self {
            Target::Absolute(v) => v as i16,
            Target::Relative(d) => current as i16 + d,
        };
        raw.clamp(floor as i16, 100) as u8
    }
}

/// Effective floor: the configured minimum, but never below the backend's hard minimum.
pub fn floor(configured: Option<u8>, default_min: u8, hard_min: u8) -> u8 {
    configured.unwrap_or(default_min).max(hard_min).min(100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_targets() {
        assert_eq!(Target::parse("60"), Some(Target::Absolute(60)));
        assert_eq!(Target::parse("60%"), Some(Target::Absolute(60)));
        assert_eq!(Target::parse("+5"), Some(Target::Relative(5)));
        assert_eq!(Target::parse("-10"), Some(Target::Relative(-10)));
        assert_eq!(Target::parse("101"), None);
        assert_eq!(Target::parse("abc"), None);
        assert_eq!(Target::parse(""), None);
    }

    #[test]
    fn resolves_with_clamping() {
        assert_eq!(Target::Absolute(60).resolve(10, 10), 60);
        assert_eq!(Target::Absolute(3).resolve(50, 10), 10);
        assert_eq!(Target::Relative(10).resolve(95, 10), 100);
        assert_eq!(Target::Relative(-10).resolve(15, 10), 10);
        assert_eq!(Target::Relative(-5).resolve(50, 0), 45);
    }

    #[test]
    fn floor_respects_hard_min() {
        assert_eq!(floor(None, 10, 0), 10);
        assert_eq!(floor(Some(2), 10, 5), 5);
        assert_eq!(floor(Some(20), 10, 5), 20);
        assert_eq!(floor(Some(0), 10, 0), 0);
    }
}
