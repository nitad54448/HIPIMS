//! Physical units as newtypes. Bare `f64` never crosses a driver or measurement
//! API; conversions are explicit (`.get()`, `from_*`, `to_*`).

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! unit {
    ($(#[$m:meta])* $name:ident, $sym:literal) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub f64);

        impl $name {
            /// Wrap a raw value expressed in the base unit.
            pub const fn new(v: f64) -> Self { Self(v) }
            /// Raw value in the base unit.
            pub const fn get(self) -> f64 { self.0 }
            /// `true` if the value is neither NaN nor infinite.
            pub fn is_finite(self) -> bool { self.0.is_finite() }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{} {}", self.0, $sym)
            }
        }
    };
}

unit!(/// Frequency in hertz.
    Hertz, "Hz");
unit!(/// Electrical potential in volts.
    Volt, "V");
unit!(/// Time in seconds.
    Second, "s");
unit!(/// Angle in radians.
    Radian, "rad");
unit!(/// Angle in degrees (the UHF PID/demod phase unit).
    Degree, "deg");
unit!(/// Quality factor (dimensionless).
    QFactor, "");
unit!(/// PID gain; the physical unit depends on the loop (Hz/deg, V/V, ...).
    PidGain, "");
unit!(/// Damping rate Gamma = 1/tau of the amplitude envelope, in 1/s.
    DampingRate, "1/s");

impl Hertz {
    pub const fn from_mhz(v: f64) -> Self {
        Self(v * 1e6)
    }
    pub fn to_mhz(self) -> f64 {
        self.0 / 1e6
    }
}

impl Second {
    pub const fn from_micros(v: f64) -> Self {
        Self(v * 1e-6)
    }
    pub const fn from_millis(v: f64) -> Self {
        Self(v * 1e-3)
    }
    pub fn to_micros(self) -> f64 {
        self.0 * 1e6
    }
    pub fn to_duration(self) -> std::time::Duration {
        if self.0.is_finite() && self.0 > 0.0 {
            std::time::Duration::from_secs_f64(self.0)
        } else {
            std::time::Duration::ZERO
        }
    }
}

impl Radian {
    pub fn to_degrees(self) -> Degree {
        Degree(self.0.to_degrees())
    }
}

impl Degree {
    pub fn to_radians(self) -> Radian {
        Radian(self.0.to_radians())
    }
}

impl QFactor {
    /// Damping rate of the amplitude envelope: Gamma = pi f0 / Q.
    pub fn damping_rate(self, f0: Hertz) -> DampingRate {
        DampingRate(std::f64::consts::PI * f0.0 / self.0)
    }
}

impl DampingRate {
    /// Quality factor for a resonance at `f0`: Q = pi f0 / Gamma.
    pub fn quality_factor(self, f0: Hertz) -> QFactor {
        QFactor(std::f64::consts::PI * f0.0 / self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn q_gamma_roundtrip() {
        let f0 = Hertz::from_mhz(6.0);
        let q = QFactor(50_000.0);
        let g = q.damping_rate(f0);
        assert!((g.quality_factor(f0).get() - 50_000.0).abs() < 1e-6);
        // tau = 1/Gamma = Q/(pi f0) ~ 2.65 ms
        assert!((1.0 / g.get() - 2.6526e-3).abs() < 1e-6);
    }

    #[test]
    fn angle_roundtrip() {
        let d = Degree(37.5);
        assert!((d.to_radians().to_degrees().get() - 37.5).abs() < 1e-12);
    }

    #[test]
    fn negative_duration_is_zero() {
        assert_eq!(Second(-1.0).to_duration(), std::time::Duration::ZERO);
    }
}
