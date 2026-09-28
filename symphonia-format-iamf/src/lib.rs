//! Pure-Rust IAMF (Immersive Audio Model and Formats) demuxer for Symphonia.
//!
//! Reads IAMF presentations stored in ISO-BMFF (`.mp4`) containers as
//! specified by IAMF v1.1.0 clause 8: a `ftyp` box with an `iamf` compatible
//! brand, one `iacb` box per audio track carrying the descriptor OBUs, and
//! `iamf`-branded sample entries whose samples are raw audio frames.
//!
//! Use [`IamfFormatReader`] directly, or register it on a Symphonia
//! [`Probe`](symphonia_core::formats::probe::Probe) with [`register_all`].
//!
//! [`reassemble_ia_sequence`] rebuilds the canonical IA Sequence byte stream
//! (sequence-header OBU first, then the per-track descriptor OBUs) from a
//! parsed file so it can be fed to `sotf-iamf` for rendering.

#![forbid(unsafe_code)]

mod boxes;
mod bridge;
mod descriptors;
mod error;
mod reader;
mod register;
mod sample_table;

pub use bridge::{reassemble_ia_sequence, track_descriptor_obus};
pub use descriptors::{IamfTrackConfig, parse_iacb};
pub use error::{IamfMp4Error, to_symphonia_error};
pub use reader::IamfFormatReader;
pub use register::{register_all, register_decoders};
pub use sample_table::{MediaHeader, SampleRun, SampleTable};

/// `FourCC` of the IAMF sample entries (`iamf`).
pub const IAMF_SAMPLE_ENTRY: [u8; 4] = *b"iamf";

/// Compatible brand that marks a BMFF file as carrying IAMF audio.
pub const IAMF_BRAND: [u8; 4] = *b"iamf";
