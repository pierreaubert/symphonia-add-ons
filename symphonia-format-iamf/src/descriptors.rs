//! IAMF-specific box parsing: `ftyp` brand check, `trak` walk, `iacb`
//! descriptor OBUs, and `iamf` sample entries.
//!
//! The demuxer never interprets codec payloads here; per-track descriptor
//! OBUs are validated with `sotf-iamf`'s own descriptor parser and the raw
//! audio frames flow through to Symphonia packets untouched.

use std::io::{Read, Seek, SeekFrom};

use sotf_iamf::error::IamfError;
use sotf_iamf::obu::parse_descriptors;
use sotf_iamf::obu::parser::IamfDescriptors;
use sotf_iamf::types::CodecConfig;

use crate::boxes::{BoxHeader, SliceReader, read_full_box, walk_children};
use crate::error::IamfMp4Error;
use crate::sample_table::{SampleTable, parse_elst_media_time, parse_mdhd, parse_stbl};
use crate::{IAMF_BRAND, IAMF_SAMPLE_ENTRY};

/// Highest IAMF major version this demuxer accepts from `iacb`.
pub const IACB_VERSION: u8 = 1;

/// Per-track configuration extracted from one `trak` box.
#[derive(Debug, Clone)]
pub struct IamfTrackConfig {
    /// 1-based track number inside `moov` (also the Symphonia track id).
    pub track_number: u32,
    /// Track id from `tkhd` (0 when the header is absent).
    pub tkhd_id: u32,
    /// Codec configuration referenced by the `iamf` sample entry.
    pub codec_config: CodecConfig,
    /// All descriptor OBUs parsed from this track's `iacb` box.
    pub descriptors: IamfDescriptors,
    /// Raw OBU section bytes (sequence-header OBU first), kept verbatim
    /// for [`reassemble_ia_sequence`](crate::reassemble_ia_sequence).
    pub obu_section: Vec<u8>,
    /// Track timescale from `mdia/mdhd` (media units per second).
    pub timescale: u32,
    /// First edit's media time from `edts/elst` (0 when absent).
    pub start_media_time: i64,
    /// Expanded sample table from `mdia/minf/stbl`.
    pub sample_table: SampleTable,
}

/// Check the `ftyp` payload for the IAMF brand (major or compatible).
pub fn check_ftyp(payload: &[u8]) -> Result<(), IamfMp4Error> {
    let mut r = SliceReader::new(payload);
    let major = r.fourcc()?;
    r.skip(4)?; // minor_version
    if major == IAMF_BRAND {
        return Ok(());
    }
    while r.remaining() >= 4 {
        if r.fourcc()? == IAMF_BRAND {
            return Ok(());
        }
    }
    Err(IamfMp4Error::NotIamf)
}

/// Parse an `iacb` payload: `FullBox` version/flags, then descriptor OBUs.
///
/// The OBU section (sequence-header OBU first) is validated with
/// `sotf-iamf`. The box must end exactly at the last descriptor OBU:
/// trailing bytes would be misread as another OBU, so the referenced
/// codec config is selected by the caller (from the sample entry or by
/// uniqueness), not from `iacb` tail bytes. Returns the descriptors and
/// the raw OBU section bytes for lossless reassembly.
///
/// # Errors
///
/// Rejects `iacb` versions above [`IACB_VERSION`], descriptor sections
/// `sotf-iamf` refuses, and boxes with trailing bytes past the last OBU.
pub fn parse_iacb(payload: &[u8]) -> Result<(IamfDescriptors, Vec<u8>), IamfMp4Error> {
    let (version, _flags, rest) = read_full_box(payload)?;
    if version > IACB_VERSION {
        return Err(IamfMp4Error::Unsupported("iacb version > 1"));
    }
    let (descriptors, consumed) = parse_descriptors(rest)?;
    if consumed != rest.len() {
        return Err(IamfMp4Error::MalformedBox(
            "iacb",
            "trailing bytes after descriptor OBUs",
        ));
    }
    Ok((descriptors, rest[..consumed].to_vec()))
}

/// Select the track's codec config: the sample entry reference wins when
/// present, otherwise the single codec config in the section.
fn select_codec_config(
    descriptors: &IamfDescriptors,
    entry_codec_config_id: Option<u32>,
) -> Result<CodecConfig, IamfMp4Error> {
    if let Some(id) = entry_codec_config_id {
        return descriptors
            .codec_configs
            .iter()
            .find(|c| c.codec_config_id == id)
            .cloned()
            .ok_or(IamfMp4Error::BadDescriptors(IamfError::UnknownCodecConfig(
                id,
            )));
    }
    if descriptors.codec_configs.len() == 1 {
        return Ok(descriptors.codec_configs[0].clone());
    }
    Err(IamfMp4Error::MalformedBox(
        "iacb",
        "ambiguous codec_config_id",
    ))
}

/// Walk one `trak` box and extract its [`IamfTrackConfig`].
///
/// Expects `mdia/minf/stbl/stsd` to hold exactly one `iamf` sample entry
/// and `mdia/minf` to hold one `iacb` descriptor box.
pub fn parse_trak<R: Read + Seek>(
    r: &mut R,
    trak: &BoxHeader,
    track_number: u32,
) -> Result<IamfTrackConfig, IamfMp4Error> {
    let end = trak.end_offset.unwrap_or(u64::MAX);
    r.seek(SeekFrom::Start(trak.payload_offset))?;
    let mut tkhd_id = 0u32;
    let mut iacb: Option<Vec<u8>> = None;
    let mut entry_codec_config_id: Option<u32> = None;
    let mut mdhd: Option<Vec<u8>> = None;
    let mut elst: Option<Vec<u8>> = None;
    let mut stbl: Option<BoxHeader> = None;

    walk_children(r, end, 1, |r, child, depth| {
        match &child.typ {
            b"tkhd" => {
                let payload = child.read_payload(r)?;
                tkhd_id = parse_tkhd_id(&payload)?;
            }
            b"edts" => {
                let edts_end = child.end_offset.unwrap_or(u64::MAX);
                r.seek(SeekFrom::Start(child.payload_offset))?;
                walk_children(r, edts_end, depth + 1, |r, edts_child, _| {
                    if &edts_child.typ == b"elst" {
                        elst = Some(edts_child.read_payload(r)?);
                    } else {
                        edts_child.skip(r)?;
                    }
                    Ok(true)
                })?;
            }
            b"mdia" => {
                let child_end = child.end_offset.unwrap_or(u64::MAX);
                r.seek(SeekFrom::Start(child.payload_offset))?;
                walk_children(r, child_end, depth + 1, |r, mdia_child, depth| {
                    match &mdia_child.typ {
                        b"mdhd" => {
                            mdhd = Some(mdia_child.read_payload(r)?);
                        }
                        b"minf" => {
                            let minf_end = mdia_child.end_offset.unwrap_or(u64::MAX);
                            r.seek(SeekFrom::Start(mdia_child.payload_offset))?;
                            walk_children(r, minf_end, depth + 1, |r, minf_child, _| {
                                match &minf_child.typ {
                                    b"iacb" => {
                                        iacb = Some(minf_child.read_payload(r)?);
                                    }
                                    b"stbl" => {
                                        stbl = Some(*minf_child);
                                        entry_codec_config_id = parse_stsd_entry(r, minf_child)?;
                                    }
                                    _ => minf_child.skip(r)?,
                                }
                                Ok(true)
                            })?;
                        }
                        _ => mdia_child.skip(r)?,
                    }
                    Ok(true)
                })?;
            }
            _ => child.skip(r)?,
        }
        Ok(true)
    })?;

    let iacb = iacb.ok_or(IamfMp4Error::MissingBox("trak/minf/iacb"))?;
    let (descriptors, obu_section) = parse_iacb(&iacb)?;
    let codec_config = select_codec_config(&descriptors, entry_codec_config_id)?;
    let mdhd = mdhd.ok_or(IamfMp4Error::MissingBox("trak/mdia/mdhd"))?;
    let media = parse_mdhd(&mdhd)?;
    if media.timescale == 0 {
        return Err(IamfMp4Error::MalformedBox("mdhd", "zero timescale"));
    }
    // Negative media times (empty edits) collapse to zero: edit-list
    // shifting is milestone-2 scope.
    let start_media_time = match elst {
        Some(payload) => parse_elst_media_time(&payload)?.unwrap_or(0).max(0),
        None => 0,
    };
    let stbl_header = stbl.ok_or(IamfMp4Error::MissingBox("trak/minf/stbl"))?;
    let sample_table = parse_stbl(r, &stbl_header)?;
    Ok(IamfTrackConfig {
        track_number,
        tkhd_id,
        codec_config,
        descriptors,
        obu_section,
        timescale: media.timescale,
        start_media_time,
        sample_table,
    })
}

/// Parse the track id from a `tkhd` payload (version 0 or 1).
fn parse_tkhd_id(payload: &[u8]) -> Result<u32, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    if version == 1 {
        r.skip(16)?; // creation + modification
    } else {
        r.skip(8)?;
    }
    r.u32_be()
}

/// Find the `stsd` box inside `stbl` and require a single `iamf` sample
/// entry.
///
/// The entry layout is the `iamf` `FourCC`, 8 reserved bytes, then the
/// referenced `codec_config_id` as leb128. Returns that id so the caller
/// can cross-check it against the parsed `iacb` descriptors.
fn parse_stsd_entry<R: Read + Seek>(
    r: &mut R,
    stbl: &BoxHeader,
) -> Result<Option<u32>, IamfMp4Error> {
    let end = stbl.end_offset.unwrap_or(u64::MAX);
    r.seek(SeekFrom::Start(stbl.payload_offset))?;
    let mut found: Option<u32> = None;
    walk_children(r, end, 4, |r, child, _| {
        if &child.typ == b"stsd" {
            let payload = child.read_payload(r)?;
            let (version, _, rest) = read_full_box(&payload)?;
            if version > 0 {
                return Err(IamfMp4Error::Unsupported("stsd version > 0"));
            }
            let mut sr = SliceReader::new(rest);
            let entry_count = sr.u32_be()?;
            if entry_count != 1 {
                return Err(IamfMp4Error::MalformedBox(
                    "stsd",
                    "iamf requires exactly one sample entry",
                ));
            }
            let entry_len = usize::try_from(sr.u32_be()?)
                .ok()
                .ok_or(IamfMp4Error::MalformedBox("stsd", "entry too large"))?;
            let entry_typ = sr.fourcc()?;
            if entry_typ != IAMF_SAMPLE_ENTRY {
                return Err(IamfMp4Error::Unsupported(
                    "non-iamf sample entry in IAMF track",
                ));
            }
            if entry_len < 8 {
                return Err(IamfMp4Error::MalformedBox("iamf", "entry too small"));
            }
            if entry_len == 8 {
                // Reserved-only entry: nothing to cross-check.
                return Ok(true);
            }
            let mut er = SliceReader::new(sr.bytes(entry_len - 8)?);
            er.skip(8)?; // reserved
            let mut id = 0u32;
            for i in 0..5 {
                let b = er.u8()?;
                id |= u32::from(b & 0x7F) << (7 * i);
                if b & 0x80 == 0 {
                    break;
                }
                if i == 4 {
                    return Err(IamfMp4Error::MalformedBox(
                        "iamf",
                        "codec_config_id leb128 overflow",
                    ));
                }
            }
            found = Some(id);
        } else {
            child.skip(r)?;
        }
        Ok(true)
    })?;
    Ok(found)
}
