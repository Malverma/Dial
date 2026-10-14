//! Perceptual mapping between slider percent and a backend's native range.

/// Map 0–100 % to `min_native..=max_native` using `native = min + (max-min) * (pct/100)^gamma`.
pub fn to_native(pct: u8, max_native: u32, gamma: f64) -> u32 {
    let t = (pct.min(100) as f64 / 100.0).powf(gamma);
    (t * max_native as f64).round() as u32
}

/// Inverse of [`to_native`].
pub fn from_native(native: u32, max_native: u32, gamma: f64) -> u8 {
    if max_native == 0 {
        return 100;
    }
    let t = (native.min(max_native) as f64 / max_native as f64).powf(1.0 / gamma);
    (t * 100.0).round() as u8
}

/// Per-monitor balance, so panels that differ at low levels can be matched.
pub const BALANCE_MAX: i8 = 50;

/// Exponent for a balance in `-BALANCE_MAX..=BALANCE_MAX`: positive lifts low
/// levels, negative lowers them.
fn balance_gamma(balance: i8) -> f64 {
    2f64.powf(-(balance.clamp(-BALANCE_MAX, BALANCE_MAX) as f64) / 25.0)
}

/// The level sent to a monitor for slider value `pct`. 0 % and 100 % stay put.
pub fn balanced(pct: u8, balance: i8) -> u8 {
    from_native(to_native(pct, 10_000, balance_gamma(balance)), 10_000, 1.0)
}

/// Inverse of [`balanced`], for reading a monitor's level back.
pub fn unbalanced(level: u8, balance: i8) -> u8 {
    from_native(to_native(level, 10_000, 1.0), 10_000, balance_gamma(balance))
}

/// Level below which a hardware-dimmed monitor (DDC/CI, backlight) sits at
/// its lowest backlight and gamma dims the rest. Backlights stay visibly lit
/// at their minimum, so without this they never get as dark as gamma alone.
pub const HARDWARE_SPLIT: u8 = 30;

/// Split a level into the hardware's level and a gamma level, both 0–100.
pub fn split(level: u8) -> (u8, u8) {
    let (level, s) = (level.min(100) as u32, HARDWARE_SPLIT as u32);
    if level >= s {
        ((((level - s) * 100 + (100 - s) / 2) / (100 - s)) as u8, 100)
    } else {
        (0, ((level * 100 + s / 2) / s) as u8)
    }
}

/// The level for a hardware reading, with gamma at full.
pub fn unsplit(hardware: u8) -> u8 {
    let (h, s) = (hardware.min(100) as u32, HARDWARE_SPLIT as u32);
    (s + (h * (100 - s) + 50) / 100) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_between_hardware_and_gamma() {
        assert_eq!(split(100), (100, 100));
        assert_eq!(split(HARDWARE_SPLIT), (0, 100));
        assert_eq!(split(HARDWARE_SPLIT / 2), (0, 50));
        assert_eq!(split(0), (0, 0));
        for level in [30u8, 45, 70, 100] {
            assert!(unsplit(split(level).0).abs_diff(level) <= 1, "{level}");
        }
        assert_eq!(unsplit(0), HARDWARE_SPLIT);
    }

    #[test]
    fn balance_shifts_low_levels_only() {
        assert_eq!(balanced(10, 0), 10);
        assert!(balanced(10, 25) > 25, "{}", balanced(10, 25));
        assert!(balanced(10, -25) < 5, "{}", balanced(10, -25));
        for b in [-50, -20, 0, 20, 50] {
            assert_eq!(balanced(0, b), 0);
            assert_eq!(balanced(100, b), 100);
        }
        for pct in [10u8, 30, 60] {
            assert!(unbalanced(balanced(pct, 20), 20).abs_diff(pct) <= 1);
        }
    }

    #[test]
    fn linear_endpoints() {
        assert_eq!(to_native(0, 100, 1.0), 0);
        assert_eq!(to_native(100, 100, 1.0), 100);
        assert_eq!(to_native(50, 255, 1.0), 128);
    }

    #[test]
    fn gamma_curve_is_below_linear() {
        let n = to_native(50, 1000, 2.2);
        assert!(n < 500 && n > 150, "{n}");
    }

    #[test]
    fn round_trips() {
        for gamma in [1.0, 2.2] {
            for pct in [5u8, 10, 33, 50, 77, 100] {
                let n = to_native(pct, 19200, gamma);
                assert_eq!(from_native(n, 19200, gamma), pct, "gamma {gamma} pct {pct}");
            }
        }
    }

    #[test]
    fn clamps() {
        assert_eq!(to_native(200, 100, 1.0), 100);
        assert_eq!(from_native(500, 100, 1.0), 100);
        assert_eq!(from_native(0, 0, 1.0), 100);
    }
}
