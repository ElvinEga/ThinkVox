//! Tiny one-pole building blocks for the wet Side chain.
//!
//! Coefficient is derived with backward-Euler (`a = 1 / (1 + 2*pi*fc/sr)`),
//! which is algebraic — no `exp`/`sin`/`cos` in the per-sample loop — and
//! unconditionally stable for any positive cutoff.

use core::f32::consts::TAU;

#[derive(Clone, Copy, Default)]
pub struct OnePole {
    lp: f32,
}

impl OnePole {
    #[inline]
    pub fn reset(&mut self) {
        self.lp = 0.0;
    }

    /// Lowpass coefficient `a` for cutoff `fc` (Hz) at sample rate `sr` (Hz).
    #[inline]
    pub fn coeff(fc: f32, sr: f32) -> f32 {
        let fc = fc.clamp(10.0, sr * 0.45);
        1.0 / (1.0 + TAU * fc / sr)
    }

    /// `lp = a*lp + (1-a)*x`, returns the lowpassed value.
    #[inline]
    pub fn lowpass(&mut self, x: f32, a: f32) -> f32 {
        self.lp += (1.0 - a) * (x - self.lp);
        self.lp
    }

    /// First-order highpass (DC blocker / low-end cleanup).
    #[inline]
    pub fn highpass(&mut self, x: f32, a: f32) -> f32 {
        x - self.lowpass(x, a)
    }

    /// First-order high shelf: unity below `fc`, `gain` (linear) above it.
    /// At `gain == 1.0` this is bit-exact identity.
    #[inline]
    pub fn high_shelf(&mut self, x: f32, a: f32, gain: f32) -> f32 {
        // Always advance the state so toggling the shelf in/out is click-free.
        let low = self.lowpass(x, a);
        if gain == 1.0 {
            x // bit-exact bypass (avoids a 1-ULP round trip)
        } else {
            low + (x - low) * gain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highpass_removes_dc() {
        let sr = 48_000.0;
        let a = OnePole::coeff(120.0, sr);
        let mut hp = OnePole::default();
        let mut y = 0.0;
        for _ in 0..20_000 {
            y = hp.highpass(1.0, a);
        }
        assert!(y.abs() < 1e-4, "DC should be gone, got {y}");
    }

    #[test]
    fn highpass_keeps_high_frequencies() {
        use core::f32::consts::PI;
        let sr = 48_000.0;
        let a = OnePole::coeff(120.0, sr);
        let mut hp = OnePole::default();
        let mut peak = 0.0_f32;
        for n in 0..20_000 {
            let x = (2.0 * PI * 4000.0 * n as f32 / sr).sin();
            peak = peak.max(hp.highpass(x, a).abs());
        }
        assert!(peak > 0.99, "4 kHz should pass, peak={peak}");
    }

    #[test]
    fn unity_shelf_is_transparent() {
        let a = OnePole::coeff(4000.0, 48_000.0);
        let mut sh = OnePole::default();
        for n in 0..1000 {
            let x = (n as f32 * 0.031).sin();
            assert_eq!(sh.high_shelf(x, a, 1.0), x);
        }
    }
}
