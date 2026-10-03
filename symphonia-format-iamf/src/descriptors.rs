//! IAMF-specific box parsing: `ftyp` brand check, `trak` walk, `iacb`
//! descriptor OBUs, and `iamf` sample entries.
//!
//! The demuxer never interprets codec payloads here; per-entry descriptor
//! OBUs are validated with `sotf-iamf`'s own descriptor parser and the raw
//! audio frames flow through to Symphonia packets untouched.
//!
//! Layout (IAMF §6.2.3–§6.2.4): each `iamf` sample entry is an
//! `AudioSampleEntry` whose child boxes hold exactly one `iacb`
//! (`configurationVersion` byte, leb128 `configOBUs_size`, then the OBU
//! bytes). A track may carry several entries (concatenated sequences);
//! `stsc` rows and fragment headers select among them.

use std::io::{Cursor, Read, Seek, SeekFrom};

use symphonia_iamf_core::error::IamfError;
use symphonia_iamf_core::obu::parse_descriptors;
use symphonia_iamf_core::obu::parser::IamfDescriptors;
use symphonia_iamf_core::types::{CodecConfig, CodecId};

use crate::boxes::{BoxHeader, SliceReader, read_full_box, walk_children};
use crate::error::IamfMp4Error;
use crate::sample_table::{EditEntry, EditPlan, SampleTable, parse_elst, parse_mdhd, parse_stbl};
use crate::{IAMF_BRAND, IAMF_SAMPLE_ENTRY};

/// `iacb` configuration version defined by IAMF §6.2.4. Boxes carrying any
/// other version are ignored (not rejected) per spec.
pub const IACB_VERSION: u8 = 1;

/// `AudioSampleEntry` fixed field size before child boxes:
/// `6 reserved + data_reference_index(2) + 8 reserved + channelcount(2) +
/// samplesize(2) + samplerate(4)` = 24 bytes. `channelcount`/`samplerate`
/// SHALL be 0 and are ignored per spec.
const AUDIO_SAMPLE_ENTRY_FIELDS: usize = 24;

/// One parsed `iamf` sample entry: its descriptor set plus the selected
/// codec config used for track parameters.
#[derive(Debug, Clone)]
pub struct TrackEntry {
    /// All descriptor OBUs parsed from this entry's `iacb` box.
    pub descriptors: IamfDescriptors,
    /// Raw OBU section bytes (sequence-header OBU first), kept verbatim
    /// for [`reassemble_ia_sequence`](crate::reassemble_ia_sequence).
    pub obu_section: Vec<u8>,
    /// Codec configuration selected for this entry (single config, else
    /// the first audio element's reference).
    pub codec_config: CodecConfig,
}

/// Per-track configuration extracted from one `trak` box.
#[derive(Debug, Clone)]
pub struct IamfTrackConfig {
    /// 1-based track number inside `moov` (also the Symphonia track id).
    pub track_number: u32,
    /// Track id from `tkhd` (0 when the header is absent).
    pub tkhd_id: u32,
    /// Codec configuration of the first sample entry (track parameters).
    pub codec_config: CodecConfig,
    /// Parsed sample entries in `stsd` order.
    pub entries: Vec<TrackEntry>,
    /// Track timescale from `mdia/mdhd` (media units per second).
    pub timescale: u32,
    /// Raw edit entries from `edts/elst` (empty when absent).
    pub(crate) edits: Vec<EditEntry>,
    /// Presentation plan built from `edits` against the merged
    /// sample table. `parse_trak` seeds the identity placeholder; the
    /// reader replaces it via `finalize_edits` after merging
    /// `moof` runs.
    pub edit_plan: EditPlan,
    /// Expanded sample table from `mdia/minf/stbl`.
    pub sample_table: SampleTable,
}

impl IamfTrackConfig {
    /// RFC 6381 codecs parameter string for this track (IAMF §6.4).
    ///
    /// `iamf.<primary:03>.<additional:03>.<codec elements>` from the
    /// first entry's sequence-header profiles and codec config; later
    /// entries share the sequence header. AAC reports the audio object
    /// type from its `AudioSpecificConfig` (`mp4a.40.<aot>`).
    #[must_use]
    pub fn codecs_string(&self) -> String {
        let entry = &self.entries[0];
        let tail = match entry.codec_config.codec_id {
            CodecId::Opus => "Opus".to_string(),
            CodecId::AacLc => {
                format!(
                    "mp4a.40.{}",
                    aac_audio_object_type(&entry.codec_config.decoder_config)
                )
            }
            CodecId::Flac => "fLaC".to_string(),
            CodecId::Lpcm => "ipcm".to_string(),
        };
        format!(
            "iamf.{:03}.{:03}.{tail}",
            entry.descriptors.primary_profile, entry.descriptors.additional_profile
        )
    }

    /// Derive the presentation plan from the parsed edits against the
    /// (possibly fragment-extended) sample table.
    pub(crate) fn finalize_edits(&mut self, movie_timescale: u32) -> Result<(), IamfMp4Error> {
        self.edit_plan = crate::sample_table::plan_edits(
            &self.edits,
            movie_timescale,
            self.timescale,
            &self.sample_table,
        )?;
        Ok(())
    }
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

/// Parse an `iacb` payload: version byte, leb128 OBU size, OBU bytes.
///
/// Returns `None` when `configurationVersion != 1`: such boxes SHALL be
/// ignored per spec. Exactly `configOBUs_size` bytes are parsed as
/// descriptor OBUs (sequence-header OBU first); trailing bytes are future
/// fields and are skipped. The section must hold at least one audio
/// element and one mix presentation per §6.2.4.
///
/// # Errors
///
/// Rejects malformed sizes and descriptor sections `sotf-iamf` refuses.
pub fn parse_iacb(payload: &[u8]) -> Result<Option<(IamfDescriptors, Vec<u8>)>, IamfMp4Error> {
    let mut r = SliceReader::new(payload);
    let version = r.u8()?;
    if version != IACB_VERSION {
        return Ok(None);
    }
    let obu_size = r.leb128_u32()?;
    let obu_len = usize::try_from(obu_size)
        .ok()
        .ok_or(IamfMp4Error::BoxTooLarge(u64::from(obu_size)))?;
    let obu_bytes = r.bytes(obu_len).ok().ok_or(IamfMp4Error::MalformedBox(
        "iacb",
        "configOBUs_size exceeds box",
    ))?;
    // Trailing bytes are future fields: intentionally not consumed.
    let (descriptors, _) = parse_descriptors(obu_bytes)?;
    if descriptors.audio_elements.is_empty() {
        return Err(IamfMp4Error::MalformedBox("iacb", "no audio elements"));
    }
    if descriptors.mix_presentations.is_empty() {
        return Err(IamfMp4Error::MalformedBox("iacb", "no mix presentations"));
    }
    Ok(Some((descriptors, obu_bytes.to_vec())))
}

/// Validate roll recovery groups against entry codec configs.
///
/// Opus and `mp4a` samples SHALL carry `roll` groups whose distance equals
/// the entry's `audio_roll_distance`; other codecs only validate when
/// groups are present. Re-run after fragment merges: fragment runs
/// default to each entry's expected distance (traf-level groups, when
/// present, are validated separately against the same expectation).
pub(crate) fn validate_roll(
    table: &SampleTable,
    entries: &[TrackEntry],
) -> Result<(), IamfMp4Error> {
    if table.sample_roll.is_empty() {
        let mut counts = vec![0usize; entries.len()];
        for run in &table.runs {
            counts[run.entry_idx] += run.sizes.len();
        }
        for (entry, &count) in entries.iter().zip(counts.iter()) {
            if count > 0 && matches!(entry.codec_config.codec_id, CodecId::Opus | CodecId::AacLc) {
                return Err(IamfMp4Error::MalformedBox(
                    "stbl",
                    "opus/mp4a samples require roll groups",
                ));
            }
        }
        return Ok(());
    }
    let mut sample_idx = 0usize;
    for run in &table.runs {
        let want = entries[run.entry_idx].codec_config.audio_roll_distance;
        for _ in &run.sizes {
            let got = table.sample_roll.get(sample_idx).copied().unwrap_or(want);
            if got != want {
                return Err(IamfMp4Error::BadSampleTable("roll distance mismatch"));
            }
            sample_idx += 1;
        }
    }
    Ok(())
}

/// Select an entry's codec config: the single config wins, otherwise the
/// first audio element's reference.
fn select_codec_config(descriptors: &IamfDescriptors) -> Result<CodecConfig, IamfMp4Error> {
    if descriptors.codec_configs.len() == 1 {
        return Ok(descriptors.codec_configs[0].clone());
    }
    if let Some(element) = descriptors.audio_elements.first() {
        let id = element.codec_config_id;
        return descriptors
            .codec_configs
            .iter()
            .find(|c| c.codec_config_id == id)
            .cloned()
            .ok_or(IamfMp4Error::BadDescriptors(IamfError::UnknownCodecConfig(
                id,
            )));
    }
    Err(IamfMp4Error::MalformedBox(
        "iacb",
        "ambiguous codec_config_id",
    ))
}

/// Walk one `trak` box and extract its [`IamfTrackConfig`].
///
/// Expects `mdia/minf/stbl/stsd` to hold at least one `iamf` sample entry
/// with an `iacb` child box.
pub fn parse_trak<R: Read + Seek>(
    r: &mut R,
    trak: &BoxHeader,
    track_number: u32,
) -> Result<IamfTrackConfig, IamfMp4Error> {
    let end = trak.end_offset.unwrap_or(u64::MAX);
    r.seek(SeekFrom::Start(trak.payload_offset))?;
    let mut tkhd_id = 0u32;
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
                                if &minf_child.typ == b"stbl" {
                                    stbl = Some(*minf_child);
                                } else {
                                    minf_child.skip(r)?;
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

    let mdhd = mdhd.ok_or(IamfMp4Error::MissingBox("trak/mdia/mdhd"))?;
    let media = parse_mdhd(&mdhd)?;
    if media.timescale == 0 {
        return Err(IamfMp4Error::MalformedBox("mdhd", "zero timescale"));
    }
    let edits = match elst {
        Some(payload) => parse_elst(&payload)?,
        None => Vec::new(),
    };
    let stbl_header = stbl.ok_or(IamfMp4Error::MissingBox("trak/minf/stbl"))?;
    let entries = parse_stsd_entries(r, &stbl_header)?;
    if entries.is_empty() {
        return Err(IamfMp4Error::MissingBox("trak/minf/stbl/stsd/iacb"));
    }
    let sample_table = parse_stbl(r, &stbl_header, entries.len())?;
    validate_roll(&sample_table, &entries)?;
    let codec_config = entries[0].codec_config.clone();
    // Identity placeholder: the reader recomputes the real plan after
    // merging `moof` runs.
    let edit_plan = EditPlan::identity(&sample_table);
    Ok(IamfTrackConfig {
        track_number,
        tkhd_id,
        codec_config,
        entries,
        timescale: media.timescale,
        edits,
        edit_plan,
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

/// Parse every sample entry in `stsd`, returning usable IAMF entries.
///
/// `enca` entries are rejected with the protection scheme reported from
/// `sinf`; any other non-`iamf` type is rejected generically. Entries
/// whose `iacb` carries an unknown version are ignored per spec; at
/// least one usable entry must remain.
fn parse_stsd_entries<R: Read + Seek>(
    r: &mut R,
    stbl: &BoxHeader,
) -> Result<Vec<TrackEntry>, IamfMp4Error> {
    let end = stbl.end_offset.unwrap_or(u64::MAX);
    r.seek(SeekFrom::Start(stbl.payload_offset))?;
    let mut entries = Vec::new();
    walk_children(r, end, 4, |r, child, _| {
        if &child.typ == b"stsd" {
            let payload = child.read_payload(r)?;
            let (version, _, rest) = read_full_box(&payload)?;
            if version > 0 {
                return Err(IamfMp4Error::Unsupported("stsd version > 0"));
            }
            let mut sr = SliceReader::new(rest);
            let entry_count = sr.u32_be()?;
            if entry_count == 0 {
                return Err(IamfMp4Error::MalformedBox("stsd", "no sample entries"));
            }
            for _ in 0..entry_count {
                let entry_len = usize::try_from(sr.u32_be()?)
                    .ok()
                    .ok_or(IamfMp4Error::MalformedBox("stsd", "entry too large"))?;
                let entry_typ = sr.fourcc()?;
                let entry_bytes = sr
                    .bytes(entry_len.saturating_sub(8))
                    .ok()
                    .ok_or(IamfMp4Error::MalformedBox("stsd", "entry overruns box"))?;
                if entry_typ == *b"enca" {
                    // Always rejects: the `sinf` parse only reports the
                    // scheme precisely.
                    return Err(protected_error(entry_bytes));
                }
                if entry_typ != IAMF_SAMPLE_ENTRY {
                    return Err(IamfMp4Error::Unsupported(
                        "non-iamf sample entry in IAMF track",
                    ));
                }
                if let Some(entry) = parse_iamf_entry(entry_bytes)? {
                    entries.push(entry);
                }
            }
        } else {
            child.skip(r)?;
        }
        Ok(true)
    })?;
    Ok(entries)
}

/// Parse one `iamf` entry payload (after the 8-byte box header):
/// `AudioSampleEntry` fields, then child boxes; returns `None` when the
/// entry's `iacb` version is unknown (ignored per spec).
fn parse_iamf_entry(entry: &[u8]) -> Result<Option<TrackEntry>, IamfMp4Error> {
    if entry.len() < AUDIO_SAMPLE_ENTRY_FIELDS {
        return Err(IamfMp4Error::MalformedBox("iamf", "entry too small"));
    }
    // channelcount/samplerate SHALL be 0; parsers SHALL ignore them, so no
    // validation — just skip the fixed fields.
    let children = &entry[AUDIO_SAMPLE_ENTRY_FIELDS..];
    let mut cursor = Cursor::new(children);
    let end = children.len() as u64;
    let mut found: Option<TrackEntry> = None;
    walk_children(&mut cursor, end, 0, |r, child, _| {
        if &child.typ == b"iacb" {
            let payload = child.read_payload(r)?;
            if let Some((descriptors, obu_section)) = parse_iacb(&payload)? {
                let codec_config = select_codec_config(&descriptors)?;
                found = Some(TrackEntry {
                    descriptors,
                    obu_section,
                    codec_config,
                });
            }
        } else {
            // `btrt` and future boxes: tolerated, not interpreted.
            child.skip(r)?;
        }
        Ok(true)
    })?;
    Ok(found)
}

/// Build the precise rejection for an `enca` entry: the scheme and
/// version from `sinf/schm` plus the original format from `sinf/frma`.
fn protected_error(entry: &[u8]) -> IamfMp4Error {
    match parse_sinf(entry) {
        Ok((scheme, version, original)) => IamfMp4Error::Protected {
            scheme,
            version,
            original,
        },
        Err(other) => other,
    }
}

/// Parse an `enca` entry payload into `(scheme, scheme_version,
/// original_format)`: the `AudioSampleEntry` prefix plus a `sinf` child
/// carrying `schm` and `frma` (`schi` data stays opaque).
fn parse_sinf(entry: &[u8]) -> Result<(String, u32, String), IamfMp4Error> {
    if entry.len() < AUDIO_SAMPLE_ENTRY_FIELDS {
        return Err(IamfMp4Error::MalformedBox("enca", "entry too small"));
    }
    let children = &entry[AUDIO_SAMPLE_ENTRY_FIELDS..];
    let mut cursor = Cursor::new(children);
    let end = children.len() as u64;
    let mut sinf: Option<Vec<u8>> = None;
    walk_children(&mut cursor, end, 0, |r, child, _| {
        if &child.typ == b"sinf" {
            sinf = Some(child.read_payload(r)?);
        } else {
            child.skip(r)?;
        }
        Ok(true)
    })?;
    let sinf = sinf.ok_or(IamfMp4Error::MalformedBox("enca", "missing sinf"))?;
    let mut cursor = Cursor::new(&sinf);
    let end = sinf.len() as u64;
    let mut original: Option<String> = None;
    let mut scheme: Option<(String, u32)> = None;
    walk_children(&mut cursor, end, 0, |r, child, _| {
        match &child.typ {
            b"frma" => {
                original = Some(parse_frma(&child.read_payload(r)?)?);
            }
            b"schm" => {
                scheme = Some(parse_schm(&child.read_payload(r)?)?);
            }
            _ => child.skip(r)?,
        }
        Ok(true)
    })?;
    let original = original.ok_or(IamfMp4Error::MissingBox("sinf/frma"))?;
    let (scheme, version) = scheme.ok_or(IamfMp4Error::MissingBox("sinf/schm"))?;
    Ok((scheme, version, original))
}

/// Parse `frma`: the original (pre-encryption) sample-entry format.
fn parse_frma(payload: &[u8]) -> Result<String, IamfMp4Error> {
    if payload.len() < 4 {
        return Err(IamfMp4Error::MalformedBox("frma", "truncated"));
    }
    Ok(String::from_utf8_lossy(&payload[..4]).into_owned())
}

/// Audio object type from an `AudioSpecificConfig`: the first 5 bits,
/// with the 31-escape followed. Missing or truncated configs report
/// AAC-LC (2), implied by the `mp4a` codec id.
fn aac_audio_object_type(asc: &[u8]) -> u32 {
    let Some(&first) = asc.first() else {
        return 2;
    };
    let aot = u32::from(first >> 3);
    if aot != 31 {
        return aot;
    }
    let Some(&second) = asc.get(1) else {
        return 2;
    };
    32 + u32::from((second >> 2) & 0x3F)
}

/// Parse `schm` into `(scheme, scheme_version)`. The optional scheme
/// URI and the `schi` box stay opaque to the demuxer.
fn parse_schm(payload: &[u8]) -> Result<(String, u32), IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    if version != 0 {
        return Err(IamfMp4Error::Unsupported("schm version"));
    }
    let mut r = SliceReader::new(rest);
    let scheme = r.fourcc()?;
    let scheme_version = r.u32_be()?;
    Ok((
        String::from_utf8_lossy(&scheme).into_owned(),
        scheme_version,
    ))
}
