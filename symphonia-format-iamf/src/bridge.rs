//! IA Sequence bridge: rebuild the canonical descriptor byte stream from
//! per-track `iacb` sections.
//!
//! A BMFF file stores one descriptor set per sample entry while the raw IA
//! Sequence (`sotf-iamf`) expects a single leading descriptor section.
//! [`reassemble_ia_sequence`] concatenates every entry section across
//! tracks, keeping the first sequence-header OBU and dropping the repeated
//! sequence-header OBU of later sections so every codec config, audio
//! element, and mix presentation appears exactly once.

use symphonia_iamf_core::obu::parser::parse_obu_header;
use symphonia_iamf_core::obu::{ObuHeader, ObuType};

use crate::descriptors::{IamfTrackConfig, TrackEntry};
use crate::error::IamfMp4Error;

/// Raw descriptor OBUs of a track's first entry, verbatim from `iacb`.
#[must_use]
pub fn track_descriptor_obus(track: &IamfTrackConfig) -> &[u8] {
    track
        .entries
        .first()
        .map_or(&[], |entry| entry.obu_section.as_slice())
}

/// Raw descriptor OBUs of one sample entry, verbatim from `iacb`.
#[must_use]
pub fn entry_descriptor_obus(entry: &TrackEntry) -> &[u8] {
    &entry.obu_section
}

/// Rebuild the canonical IA Sequence descriptor section from demuxed tracks.
///
/// The first entry's OBU section (sequence-header OBU first) is emitted
/// whole; each later entry contributes its section minus its leading
/// sequence-header OBU. All entries must agree on the sequence header —
/// a mismatch is rejected rather than silently picked.
///
/// # Errors
///
/// Rejects empty track/entry lists, sections without a leading
/// sequence-header OBU, and sequence-header mismatches across entries.
pub fn reassemble_ia_sequence(tracks: &[IamfTrackConfig]) -> Result<Vec<u8>, IamfMp4Error> {
    let sections: Vec<&[u8]> = tracks
        .iter()
        .flat_map(|track| {
            track
                .entries
                .iter()
                .map(|entry| entry.obu_section.as_slice())
        })
        .collect();
    let first = sections.first().ok_or(IamfMp4Error::NoAudioTrack)?;
    let (first_header, first_len) = obu_span(first)?;
    if first_header.obu_type != ObuType::SequenceHeader {
        return Err(IamfMp4Error::MalformedBox(
            "iacb",
            "first OBU is not a sequence header",
        ));
    }

    let mut out = first.to_vec();
    for section in sections.iter().skip(1) {
        let (header, header_len) = obu_span(section)?;
        if header.obu_type != ObuType::SequenceHeader {
            return Err(IamfMp4Error::MalformedBox(
                "iacb",
                "entry section without leading sequence header",
            ));
        }
        if section[..header_len] != first[..first_len] {
            return Err(IamfMp4Error::MalformedBox(
                "iacb",
                "sequence header mismatch across entries",
            ));
        }
        out.extend_from_slice(&section[header_len..]);
    }
    Ok(out)
}

/// Byte span of the first OBU in `section` (header + payload).
fn obu_span(section: &[u8]) -> Result<(ObuHeader, usize), IamfMp4Error> {
    let (header, header_size) = parse_obu_header(section).map_err(IamfMp4Error::BadDescriptors)?;
    let total = header_size
        .checked_add(header.payload_size)
        .ok_or(IamfMp4Error::MalformedBox("iacb", "obu size overflow"))?;
    if total > section.len() {
        return Err(IamfMp4Error::MalformedBox("iacb", "truncated obu span"));
    }
    Ok((header, total))
}
