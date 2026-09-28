//! Error type for IAMF BMFF demuxing.

use sotf_iamf::error::IamfError;
use symphonia_core::errors::{Error as SymphoniaError, SeekErrorKind};
use thiserror::Error;

/// Errors raised while demuxing an IAMF ISO-BMFF file.
#[derive(Debug, Error)]
pub enum IamfMp4Error {
    /// Underlying I/O failure while reading boxes or samples.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Box structure is malformed or truncated.
    #[error("malformed box {0}: {1}")]
    MalformedBox(&'static str, &'static str),
    /// A required box is missing.
    #[error("missing required box: {0}")]
    MissingBox(&'static str),
    /// The `ftyp` box carries no `iamf` brand.
    #[error("not an IAMF file: ftyp has no iamf brand")]
    NotIamf,
    /// Box nesting exceeds the supported depth.
    #[error("box nesting too deep")]
    NestingTooDeep,
    /// Box payload exceeds the supported in-memory limit.
    #[error("box too large: {0} bytes")]
    BoxTooLarge(u64),
    /// No audio track found in the movie box.
    #[error("no IAMF audio track found")]
    NoAudioTrack,
    /// Sample table is inconsistent (e.g. chunk offset beyond EOF).
    #[error("inconsistent sample table: {0}")]
    BadSampleTable(&'static str),
    /// Descriptor OBUs rejected by `sotf-iamf`.
    #[error("invalid IAMF descriptors: {0}")]
    BadDescriptors(#[from] IamfError),
    /// The track is encrypted: an `enca` sample entry names a protection
    /// scheme the demuxer cannot remove.
    #[error("protected IAMF track: scheme {scheme} v{version}, original format {original}")]
    Protected {
        /// `schm` scheme `FourCC` (e.g. `cenc`), lossy UTF-8.
        scheme: String,
        /// `schm` scheme version.
        version: u32,
        /// `frma` original sample-entry format, lossy UTF-8.
        original: String,
    },
    /// The file uses a feature the demuxer does not support.
    #[error("unsupported: {0}")]
    Unsupported(&'static str),
}

/// Map a demux error onto the Symphonia error taxonomy.
#[must_use]
pub fn to_symphonia_error(err: IamfMp4Error) -> SymphoniaError {
    match err {
        IamfMp4Error::Io(e) => SymphoniaError::IoError(e),
        IamfMp4Error::NotIamf => SymphoniaError::Unsupported("iamf: not an IAMF file"),
        IamfMp4Error::NoAudioTrack => SymphoniaError::Unsupported("iamf: no audio track"),
        IamfMp4Error::Unsupported(what) => SymphoniaError::Unsupported(what),
        protected @ IamfMp4Error::Protected { .. } => {
            SymphoniaError::Unsupported(Box::leak(format!("iamf: {protected}").into_boxed_str()))
        }
        other => SymphoniaError::DecodeError(Box::leak(format!("iamf: {other}").into_boxed_str())),
    }
}

/// Build a seek failure directly.
#[must_use]
pub fn seek_err(kind: SeekErrorKind) -> SymphoniaError {
    SymphoniaError::SeekError(kind)
}
