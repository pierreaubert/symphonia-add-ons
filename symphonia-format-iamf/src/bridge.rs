//! IA Sequence bridge: rebuild the canonical descriptor byte stream from
//! per-track `iacb` sections.
//!
//! A BMFF file stores one descriptor set per audio track while the raw IA
//! Sequence (`sotf-iamf`) expects a single leading descriptor section.
//! [`reassemble_ia_sequence`] concatenates the per-track OBU sections,
//! keeping the first track's sequence-header OBU and dropping the repeated
//! sequence-header OBU of later tracks so every codec config, audio
//! element, and mix presentation appears exactly once.

use sotf_iamf::obu::parser::parse_obu_header;
use sotf_iamf::obu::{ObuHeader, ObuType};

use crate::descriptors::IamfTrackConfig;
use crate::error::IamfMp4Error;

/// Raw descriptor OBUs of one track, verbatim from its `iacb` box.
#[must_use]
pub fn track_descriptor_obus(track: &IamfTrackConfig) -> &[u8] {
    &track.obu_section
}

/// Rebuild the canonical IA Sequence descriptor section from demuxed tracks.
///
/// The first track's OBU section (sequence-header OBU first) is emitted
/// whole; each later track contributes its section minus its leading
/// sequence-header OBU. All tracks must agree on the sequence header —
/// a mismatch is rejected rather than silently picked.
///
/// # Errors
///
/// Rejects empty track lists, sections without a leading sequence-header
/// OBU, and sequence-header mismatches across tracks.
pub fn reassemble_ia_sequence(tracks: &[IamfTrackConfig]) -> Result<Vec<u8>, IamfMp4Error> {
    let first = tracks.first().ok_or(IamfMp4Error::NoAudioTrack)?;
    let (first_header, first_len) = obu_span(&first.obu_section)?;
    if first_header.obu_type != ObuType::SequenceHeader {
        return Err(IamfMp4Error::MalformedBox(
            "iacb",
            "first OBU is not a sequence header",
        ));
    }

    let mut out = first.obu_section.clone();
    for track in &tracks[1..] {
        let (header, header_len) = obu_span(&track.obu_section)?;
        if header.obu_type != ObuType::SequenceHeader {
            return Err(IamfMp4Error::MalformedBox(
                "iacb",
                "track section without leading sequence header",
            ));
        }
        let this_header_bytes = &track.obu_section[..header_len];
        if this_header_bytes != &first.obu_section[..first_len] {
            return Err(IamfMp4Error::MalformedBox(
                "iacb",
                "sequence header mismatch across tracks",
            ));
        }
        out.extend_from_slice(&track.obu_section[header_len..]);
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
