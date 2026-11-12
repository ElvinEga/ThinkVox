//! DSP for ThickVox.
//!
//! Signal flow (see `thickener.rs` for the full block):
//!
//! ```text
//!   mono sum of input
//!        |
//!        +--> shared DelayLine --> [8 x Voice: granular micro-pitch + short delay]
//!                                        |
//!                                        v
//!                              wet L / wet R (hard-panned voices)
//!                                        |
//!                              encode M/S -> Side HPF -> Side air shelf
//!                                        |
//!                              Side *= Width  (or Side = 0 in Mid Only mode)
//!                                        |
//!                              decode M/S -> Mix -> + dry
//! ```
//!
//! Everything is pre-allocated in `Thickener::new` / `prepare`. `process_sample`
//! performs no allocation, no locking and no I/O, so it is safe on the audio
//! thread.

pub mod delay;
pub mod filter;
pub mod pitch_shifter;
pub mod thickener;
pub mod voice;
