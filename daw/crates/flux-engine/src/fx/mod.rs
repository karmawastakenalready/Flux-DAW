//! The processors past the basic channel strip in [`crate::dsp`]: the full compressor, the master's two-stage
//! true-peak limiter and multiband compressor, the feedback-delay-network reverbs, dynamic EQ and oversampled
//! saturation.
//!
//! Like `dsp`, each is a streaming processor that live playback runs block by block and the offline render runs over
//! whole buffers, so both paths share one implementation. Those whose output is a gain (the compressors, the dynamic
//! EQ, the limiter) can report it per sample, so a part of the mix (a layer or a track stem) can be given exactly its
//! share of the result.

pub mod dynamics;
pub mod dyneq;
pub mod exciter;
pub mod imager;
pub mod limiter;
pub mod linphase;
pub mod multiband;
pub mod reverb;
pub mod saturate;
