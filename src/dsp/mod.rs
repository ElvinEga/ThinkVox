//! DSP for ThickVox.
//!
//! Signal flow (see `thickener.rs` for the full block):
//!
//! ```text
//!   mono sum of input
//!        |
//!        +--> [STAGE 1] DelayStage: 10 / 24 ms ADT offsets
//!        |        |--> GrainHistory[short] --+
//!        |        |--> GrainHistory[long]  --+
//!        |                                  |
//!        |    [STAGE 2] 8 x MicroPitchShifter (one read head each,
//!        |               correlation-aligned crossfade only at a wrap)
//!        |                                  |
//!        |                        per-pair pan spread
//!        |                                  v
//!        |                            wet L / wet R
//!        |                                  |
//!        |              encode M/S -> Side HPF -> Side air shelf
//!        |                                  |
//!        |              Side *= Width  (or Mid Only folds everything to Mid)
//!        |                                  |
//!        |                    decode M/S -> Mix -> + dry
//!        |
//!        +--------------------------------------> dry (never delayed)
//! ```
//!
//! The delay and pitch stages are independent: the delay stage reads the
//! shared input history in milliseconds and knows nothing about cents, while
//! the pitch stage reads its own grain histories in samples and knows nothing
//! about the ADT delays. `DelayStage` subtracts the pitch stage's mean
//! read-back so the classic doubling offsets survive the split.
//!
//! Everything is pre-allocated in `Thickener::new` / `prepare`.
//! `process_sample` performs no allocation, no locking and no I/O, so it is
//! safe on the audio thread.

pub mod delay;
pub mod filter;
pub mod pitch_shifter;
pub mod thickener;
pub mod voice;
