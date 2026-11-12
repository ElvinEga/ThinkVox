//! Stereo imager DSP support and scope ring buffer.
//!
//! Provides a lock-free, zero-allocation ring buffer ([`StereoScopeBuffer`])
//! transferring downsampled stereo frames from the audio thread (`process`) to
//! the GUI visualizer thread (`Goniometer`).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub use noda_audio::ui::visualizers::StereoSample;

/// Default capacity for the goniometer ring buffer (512 samples).
///
/// At a downsampling factor of 6 and 48 kHz sample rate, 512 samples represent
/// 3072 audio frames (~64 ms), providing smooth 30–60 fps vectorscope animation.
pub const DEFAULT_SCOPE_CAPACITY: usize = 512;

/// A lock-free, allocation-free ring buffer of recent stereo samples for the
/// goniometer visualizer.
pub struct StereoScopeBuffer {
    buffer: Box<[AtomicU64]>,
    write_pos: AtomicUsize,
    capacity: usize,
}

impl Default for StereoScopeBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_SCOPE_CAPACITY)
    }
}

impl StereoScopeBuffer {
    /// Create a new scope buffer holding `capacity` stereo samples.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        let mut v = Vec::with_capacity(cap);
        for _ in 0..cap {
            v.push(AtomicU64::new(0));
        }
        Self {
            buffer: v.into_boxed_slice(),
            write_pos: AtomicUsize::new(0),
            capacity: cap,
        }
    }

    /// Number of samples this buffer stores.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Push one stereo sample into the ring buffer.
    ///
    /// Wait-free and zero-allocation. Left and right channels are packed into a
    /// single 64-bit word so the GUI never observes a torn sample.
    #[inline]
    pub fn push(&self, left: f32, right: f32) {
        let pos = self.write_pos.load(Ordering::Relaxed);
        let val = pack_stereo(left, right);
        self.buffer[pos].store(val, Ordering::Relaxed);
        let next = if pos + 1 >= self.capacity { 0 } else { pos + 1 };
        self.write_pos.store(next, Ordering::Release);
    }

    /// Read the recent samples in chronological order (oldest to newest) into
    /// the provided vector, reusing its allocation.
    pub fn read_samples(&self, out: &mut Vec<StereoSample>) {
        out.clear();
        out.reserve(self.capacity);
        let head = self.write_pos.load(Ordering::Acquire);
        for i in 0..self.capacity {
            let idx = (head + i) % self.capacity;
            let val = self.buffer[idx].load(Ordering::Relaxed);
            out.push(unpack_stereo(val));
        }
    }

    /// Read a snapshot of the recent samples as a newly allocated vector.
    pub fn snapshot(&self) -> Vec<StereoSample> {
        let mut v = Vec::with_capacity(self.capacity);
        self.read_samples(&mut v);
        v
    }

    /// Clear all samples back to silence.
    pub fn clear(&self) {
        for slot in self.buffer.iter() {
            slot.store(0, Ordering::Relaxed);
        }
        self.write_pos.store(0, Ordering::Release);
    }
}

#[inline]
fn pack_stereo(left: f32, right: f32) -> u64 {
    let l = (left.to_bits() as u64) << 32;
    let r = (right.to_bits() as u64) & 0xFFFF_FFFF;
    l | r
}

#[inline]
fn unpack_stereo(val: u64) -> StereoSample {
    let l = (val >> 32) as u32;
    let r = (val & 0xFFFF_FFFF) as u32;
    StereoSample {
        left: f32::from_bits(l),
        right: f32::from_bits(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_and_unpack_preserves_floats() {
        let pairs = [
            (0.0_f32, 0.0_f32),
            (1.0, -1.0),
            (0.3541, 0.9821),
            (-0.125, 0.5),
            (f32::MIN_POSITIVE, f32::MAX),
        ];
        for (l, r) in pairs {
            let packed = pack_stereo(l, r);
            let unpacked = unpack_stereo(packed);
            assert_eq!(unpacked.left, l);
            assert_eq!(unpacked.right, r);
        }
    }

    #[test]
    fn buffer_pushes_and_orders_chronologically() {
        let buf = StereoScopeBuffer::new(4);
        buf.push(1.0, 1.0);
        buf.push(2.0, 2.0);
        buf.push(3.0, 3.0);
        buf.push(4.0, 4.0);

        let snap = buf.snapshot();
        assert_eq!(snap.len(), 4);
        assert_eq!(snap[0].left, 1.0);
        assert_eq!(snap[3].left, 4.0);

        // Wrap around by pushing two more
        buf.push(5.0, 5.0);
        buf.push(6.0, 6.0);

        let snap2 = buf.snapshot();
        assert_eq!(snap2.len(), 4);
        assert_eq!(snap2[0].left, 3.0);
        assert_eq!(snap2[1].left, 4.0);
        assert_eq!(snap2[2].left, 5.0);
        assert_eq!(snap2[3].left, 6.0);
    }
}
