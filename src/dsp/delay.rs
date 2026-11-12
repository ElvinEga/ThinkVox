//! Fixed-capacity fractional delay line + the ADT delay stage.
//!
//! The ring buffer is shared wherever eight voices need the same history,
//! which keeps the memory footprint small. Wrapping is a single bitmask
//! because capacity is always a power of two.

/// Circular buffer with 4-point cubic (Catmull-Rom) fractional reads.
///
/// Capacity is always a power of two, so wrapping is a single bitmask.
pub struct DelayLine {
    buf: Vec<f32>,
    mask: usize,
    write: usize,
}

impl DelayLine {
    /// `min_capacity_samples` is rounded up to the next power of two.
    pub fn new(min_capacity_samples: usize) -> Self {
        let cap = min_capacity_samples.max(16).next_power_of_two();
        Self {
            buf: vec![0.0; cap],
            mask: cap - 1,
            write: 0,
        }
    }

    /// Allocated capacity in samples (always a power of two).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
        self.write = 0;
    }

    /// Push one sample. After this call, `read(1.0)` returns the *previous*
    /// sample and `read(0.0)` would return the one just pushed.
    #[inline]
    pub fn push(&mut self, x: f32) {
        self.buf[self.write] = x;
        self.write = (self.write + 1) & self.mask;
    }

    /// Read the signal `d` samples behind the most recently pushed sample.
    ///
    /// Cubic interpolation needs four neighbours, so `d` is clamped to
    /// `[1.0, capacity - 4]`. Callers keep the nominal delays well inside
    /// that range; the clamp is a safety net against extreme parameter combos.
    #[inline]
    pub fn read(&self, d: f32) -> f32 {
        let max_d = (self.buf.len() - 4) as f32;
        let d = d.clamp(1.0, max_d);
        let i = d as usize; // floor, since d >= 1.0
        let f = d - i as f32;

        let w = self.write;
        // x[n - k] lives at buf[(write - 1 - k) & mask]. wrapping_sub keeps
        // the arithmetic valid even when k > write.
        let at = |k: usize| -> f32 { self.buf[w.wrapping_sub(k + 1) & self.mask] };

        // Interpolate between x[n-i] (p1) and x[n-i-1] (p2) at fraction f.
        let p0 = at(i - 1);
        let p1 = at(i);
        let p2 = at(i + 1);
        let p3 = at(i + 2);

        let c0 = p1;
        let c1 = 0.5 * (p2 - p0);
        let c2 = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
        let c3 = 0.5 * (p3 - p0) + 1.5 * (p1 - p2);
        c0 + f * (c1 + f * (c2 + f * c3))
    }

    /// Nearest-neighbour read, used only by the pitch stage's occasional
    /// correlation search. A similarity measure does not need fractional
    /// accuracy, and skipping interpolation makes that bounded loop cheap.
    #[inline]
    pub fn sample_nearest(&self, d: f32) -> f32 {
        let k = (d.max(0.0) as usize).saturating_add(1);
        self.buf[self.write.wrapping_sub(k) & self.mask]
    }
}

/// Stage 1 of the wet chain: the ADT / doubling delay.
///
/// This stage knows nothing about pitch shifting. It owns the shared input
/// history and hands back exactly two delayed mono streams (10 ms and 24 ms
/// at Delay Spread = 100%), which are then fed to the pitch stage. Keeping
/// the two concerns separate means the grain size can be changed without
/// touching the classic doubling times, and vice versa.
pub struct DelayStage {
    line: DelayLine,
}

impl DelayStage {
    pub fn new(min_capacity_samples: usize) -> Self {
        Self {
            line: DelayLine::new(min_capacity_samples),
        }
    }

    /// Feed the mono sum of the input, once per sample.
    #[inline]
    pub fn push(&mut self, mono: f32) {
        self.line.push(mono);
    }

    /// The input as it was `ms` milliseconds ago, cubic-interpolated.
    #[inline]
    pub fn read_ms(&self, ms: f32, sr: f32) -> f32 {
        self.line.read(ms.max(0.0) * 0.001 * sr)
    }

    pub fn clear(&mut self) {
        self.line.clear();
    }

    pub fn capacity(&self) -> usize {
        self.line.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_delays_are_exact() {
        let mut line = DelayLine::new(64);
        for n in 0..100 {
            line.push(n as f32);
        }
        // Last pushed sample is 99.
        assert!((line.read(1.0) - 98.0).abs() < 1e-5, "d=1 -> x[n-1]");
        assert!((line.read(2.0) - 97.0).abs() < 1e-5, "d=2 -> x[n-2]");
        assert!((line.read(10.0) - 89.0).abs() < 1e-5, "d=10 -> x[n-10]");
    }

    #[test]
    fn nearest_matches_cubic_at_integer_delays() {
        let mut line = DelayLine::new(64);
        for n in 0..100 {
            line.push(n as f32 * 0.5);
        }
        for d in [1.0_f32, 2.0, 10.0, 30.0] {
            assert!((line.read(d) - line.sample_nearest(d)).abs() < 1e-5);
        }
    }

    #[test]
    fn cubic_is_exact_on_a_ramp() {
        let mut line = DelayLine::new(64);
        for n in 0..200 {
            line.push(n as f32 * 0.25);
        }
        // A cubic Hermite interpolant reproduces a linear ramp exactly.
        for d in [1.25_f32, 1.5, 1.75, 3.3, 7.9] {
            let got = line.read(d);
            let want = (199.0 - d) * 0.25;
            assert!((got - want).abs() < 1e-3, "d={d}: {got} vs {want}");
        }
    }

    #[test]
    fn wraps_without_glitches() {
        let mut line = DelayLine::new(32);
        for n in 0..500 {
            line.push((n as f32 * 0.1).sin());
        }
        assert!(line.read(20.0).is_finite());
        assert!(line.read(20.0).abs() <= 1.001);
    }

    #[test]
    fn clamps_out_of_range_delays() {
        let mut line = DelayLine::new(32);
        line.push(1.0);
        assert!(line.read(0.0).is_finite());
        assert!(line.read(10_000.0).is_finite());
    }

    #[test]
    fn delay_stage_is_a_plain_delay() {
        let sr = 48_000.0;
        let mut stage = DelayStage::new(8192);
        // 10 ms = 480 samples.
        for n in 0..2000 {
            stage.push((2.0 * core::f32::consts::PI * 220.0 * n as f32 / sr).sin());
        }
        let n = 1999.0_f32;
        let got = stage.read_ms(10.0, sr);
        let want = (2.0 * core::f32::consts::PI * 220.0 * (n - 480.0) / sr).sin();
        assert!((got - want).abs() < 1e-3, "{got} vs {want}");
    }
}
