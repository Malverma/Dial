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

#[cfg(test)]
mod tests {
    use super::*;

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
