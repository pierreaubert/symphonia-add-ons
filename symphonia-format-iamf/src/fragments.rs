//! Fragmented MP4 support: `mvex`/`trex` defaults plus `moof` runs.
//!
//! ISO-BMFF §8.8: the movie-extends box carries per-track `trex` defaults
//! and each movie fragment contributes track runs (`tfhd` + `trun`) whose
//! samples extend the track's [`SampleTable`](crate::SampleTable) in file
//! order. Fragment runs merge into the table at open time, so packet reads
//! never know whether a sample came from `stbl` or a `moof`.
//!
//! Resolution order for per-sample values is `trun`, then `tfhd`, then
//! `trex`; a missing link in that chain rejects the fragment. The `tfdt`
//! decode time must continue the track timeline exactly — gaps are not
//! representable in the merged table and are rejected.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use crate::boxes::{BoxHeader, SliceReader, read_full_box, walk_children};
use crate::descriptors::IamfTrackConfig;
use crate::error::IamfMp4Error;
use crate::sample_table::{GroupState, SampleRun, SampleTable, SbgpGrouping};

/// `tfhd` flag: `base_data_offset` is present.
const TFHD_BASE_OFFSET: u32 = 0x0000_0001;
/// `tfhd` flag: `sample_description_index` is present.
const TFHD_DESC_INDEX: u32 = 0x0000_0002;
/// `tfhd` flag: `default_sample_duration` is present.
const TFHD_DEFAULT_DURATION: u32 = 0x0000_0008;
/// `tfhd` flag: `default_sample_size` is present.
const TFHD_DEFAULT_SIZE: u32 = 0x0000_0010;
/// `tfhd` flag: `default_sample_flags` is present.
const TFHD_DEFAULT_FLAGS: u32 = 0x0000_0020;
/// `tfhd` flag: the fragment carries no samples for this track.
const TFHD_DURATION_IS_EMPTY: u32 = 0x0001_0000;
/// `tfhd` flag: an absent `base_data_offset` means the `moof` start.
const TFHD_DEFAULT_BASE_IS_MOOF: u32 = 0x0002_0000;

/// `trun` flag: signed `data_offset` (relative to the base) is present.
const TRUN_DATA_OFFSET: u32 = 0x0000_0001;
/// `trun` flag: `first_sample_flags` is present.
const TRUN_FIRST_SAMPLE_FLAGS: u32 = 0x0000_0004;
/// `trun` flag: per-sample durations are present.
const TRUN_SAMPLE_DURATION: u32 = 0x0000_0100;
/// `trun` flag: per-sample sizes are present.
const TRUN_SAMPLE_SIZE: u32 = 0x0000_0200;
/// `trun` flag: per-sample flags are present.
const TRUN_SAMPLE_FLAGS: u32 = 0x0000_0400;
/// `trun` flag: per-sample composition offsets are present.
const TRUN_SAMPLE_CTS: u32 = 0x0000_0800;

/// Per-track fragment defaults from one `trex` box.
#[derive(Debug, Clone, Copy, Default)]
pub struct TrackDefaults {
    /// 1-based default sample description index (0 = unset).
    pub description_index: u32,
    /// Default sample duration in media timescale units (0 = unset).
    pub duration: u32,
    /// Default sample size in bytes (0 = unset).
    pub size: u32,
    /// Default sample flags (sync strictness is milestone-2 scope,
    /// so nothing reads this yet).
    #[allow(dead_code, reason = "parsed trex field kept for M2-5")]
    pub flags: u32,
}

/// Parse `mvex`, collecting `trex` defaults keyed by track id.
///
/// # Errors
///
/// Rejects truncated `trex` boxes; `mehd` and future boxes are skipped.
pub fn parse_mvex<R: Read + Seek>(
    r: &mut R,
    mvex: &BoxHeader,
) -> Result<HashMap<u32, TrackDefaults>, IamfMp4Error> {
    let end = mvex.end_offset.unwrap_or(u64::MAX);
    r.seek(SeekFrom::Start(mvex.payload_offset))?;
    let mut out = HashMap::new();
    walk_children(r, end, 2, |r, child, _| {
        if &child.typ == b"trex" {
            let payload = child.read_payload(r)?;
            let (version, _, rest) = read_full_box(&payload)?;
            if version != 0 {
                return Err(IamfMp4Error::Unsupported("trex version"));
            }
            // `read_full_box` consumed version + flags; track_ID is next.
            let mut sr = SliceReader::new(rest);
            let track_id = sr.u32_be()?;
            out.insert(
                track_id,
                TrackDefaults {
                    description_index: sr.u32_be()?,
                    duration: sr.u32_be()?,
                    size: sr.u32_be()?,
                    flags: sr.u32_be()?,
                },
            );
        } else {
            // `mehd` carries only a duration hint; future boxes likewise.
            child.skip(r)?;
        }
        Ok(true)
    })?;
    Ok(out)
}

/// Widen a 32-bit table count for host indexing.
fn widen_count(value: u32) -> Result<usize, IamfMp4Error> {
    usize::try_from(value)
        .ok()
        .ok_or(IamfMp4Error::BadSampleTable("count overflow"))
}

/// Extend every track's sample table with the samples carried by `moofs`,
/// in file order.
///
/// Callers re-run roll validation and sample-bounds checks on the merged
/// tables afterwards.
///
/// # Errors
///
/// Rejects fragments that reference unknown tracks or sample entries,
/// leave durations or sizes unspecified, break decode-time continuity,
/// or compute out-of-range sample offsets.
pub fn append_fragments<R: Read + Seek>(
    r: &mut R,
    moofs: &[BoxHeader],
    defaults: &HashMap<u32, TrackDefaults>,
    configs: &mut [IamfTrackConfig],
    file_len: u64,
) -> Result<(), IamfMp4Error> {
    for moof in moofs {
        let end = moof.end_offset.unwrap_or(file_len);
        r.seek(SeekFrom::Start(moof.payload_offset))?;
        walk_children(r, end, 1, |r, child, depth| {
            if &child.typ == b"traf" {
                append_traf(r, child, depth, moof.start, defaults, configs, file_len)?;
            } else {
                // `mfhd` sequence numbers are informational; `pssh` is
                // license metadata, tolerated because real encryption
                // always also signals via `enca`/`seig`/`saiz`.
                child.skip(r)?;
            }
            Ok(true)
        })?;
    }
    // Collapse all-zero offsets so `sample_ctts` stays empty exactly
    // when every offset is zero (mirroring the `stbl` rule).
    for config in configs {
        if config.sample_table.sample_ctts.iter().all(|&o| o == 0) {
            config.sample_table.sample_ctts.clear();
        }
    }
    Ok(())
}

/// Parsed `tfhd` fields; every default is `None` when its flag is absent.
struct Tfhd {
    track_id: u32,
    base_offset: Option<u64>,
    description_index: Option<u32>,
    default_duration: Option<u32>,
    default_size: Option<u32>,
    default_base_is_moof: bool,
    duration_is_empty: bool,
}

/// Parse one `tfhd` payload.
fn parse_tfhd(payload: &[u8]) -> Result<Tfhd, IamfMp4Error> {
    let (version, flags, rest) = read_full_box(payload)?;
    if version != 0 {
        return Err(IamfMp4Error::Unsupported("tfhd version"));
    }
    let mut sr = SliceReader::new(rest);
    let mut out = Tfhd {
        track_id: sr.u32_be()?,
        base_offset: None,
        description_index: None,
        default_duration: None,
        default_size: None,
        default_base_is_moof: flags & TFHD_DEFAULT_BASE_IS_MOOF != 0,
        duration_is_empty: flags & TFHD_DURATION_IS_EMPTY != 0,
    };
    if flags & TFHD_BASE_OFFSET != 0 {
        out.base_offset = Some(sr.u64_be()?);
    }
    if flags & TFHD_DESC_INDEX != 0 {
        out.description_index = Some(sr.u32_be()?);
    }
    if flags & TFHD_DEFAULT_DURATION != 0 {
        out.default_duration = Some(sr.u32_be()?);
    }
    if flags & TFHD_DEFAULT_SIZE != 0 {
        out.default_size = Some(sr.u32_be()?);
    }
    if flags & TFHD_DEFAULT_FLAGS != 0 {
        sr.skip(4)?; // sync strictness is milestone-2 scope
    }
    Ok(out)
}

/// Parse one `tfdt` payload into a media-timescale decode time.
fn parse_tfdt(payload: &[u8]) -> Result<u64, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    let mut sr = SliceReader::new(rest);
    if version == 1 {
        Ok(sr.u64_be()?)
    } else if version == 0 {
        Ok(u64::from(sr.u32_be()?))
    } else {
        Err(IamfMp4Error::Unsupported("tfdt version"))
    }
}

/// One parsed `trun`: sample data start plus per-sample
/// sizes/durations, with composition offsets when the flag is set.
struct FragmentRun {
    offset: u64,
    sizes: Vec<u32>,
    durations: Vec<u32>,
    ctts: Vec<i64>,
}

/// Parse one `trun` payload. `implicit` carries the running sample end
/// across `trun`s for runs that omit `data_offset`.
fn parse_trun(
    payload: &[u8],
    base: u64,
    implicit: &mut u64,
    default_duration: Option<u32>,
    default_size: Option<u32>,
) -> Result<FragmentRun, IamfMp4Error> {
    let (version, flags, rest) = read_full_box(payload)?;
    if version > 1 {
        return Err(IamfMp4Error::Unsupported("trun version"));
    }
    let mut sr = SliceReader::new(rest);
    let count = widen_count(sr.u32_be()?)?;
    let mut start = *implicit;
    if flags & TRUN_DATA_OFFSET != 0 {
        let rel = sr.u32_be()?.cast_signed();
        start = base
            .checked_add_signed(i64::from(rel))
            .ok_or(IamfMp4Error::BadSampleTable(
                "trun data offset out of range",
            ))?;
    }
    if flags & TRUN_FIRST_SAMPLE_FLAGS != 0 {
        // Sync flags carry no information the rewind-only demuxer needs:
        // playback always starts at sample 0.
        sr.skip(4)?;
    }
    let mut sizes = Vec::with_capacity(count.min(1024));
    let mut durations = Vec::with_capacity(count.min(1024));
    let mut ctts = Vec::new();
    for _ in 0..count {
        if flags & TRUN_SAMPLE_DURATION != 0 {
            durations.push(sr.u32_be()?);
        } else {
            durations.push(
                default_duration.ok_or(IamfMp4Error::MalformedBox("trun", "no sample duration"))?,
            );
        }
        if flags & TRUN_SAMPLE_SIZE != 0 {
            sizes.push(sr.u32_be()?);
        } else {
            sizes.push(default_size.ok_or(IamfMp4Error::MalformedBox("trun", "no sample size"))?);
        }
        if flags & TRUN_SAMPLE_FLAGS != 0 {
            sr.skip(4)?; // sync flags: tolerated, see above
        }
        if flags & TRUN_SAMPLE_CTS != 0 {
            // Version 1 offsets are signed, version 0 unsigned.
            ctts.push(if version == 1 {
                i64::from(sr.u32_be()?.cast_signed())
            } else {
                i64::from(sr.u32_be()?)
            });
        }
    }
    let run_bytes = sizes
        .iter()
        .try_fold(0u64, |acc, &s| acc.checked_add(u64::from(s)))
        .ok_or(IamfMp4Error::BadSampleTable("fragment run overflow"))?;
    *implicit = start
        .checked_add(run_bytes)
        .ok_or(IamfMp4Error::BadSampleTable("fragment run overflow"))?;
    Ok(FragmentRun {
        offset: start,
        sizes,
        durations,
        ctts,
    })
}

/// Merge one fragment run into the table: a new [`SampleRun`], run-length
/// merged `stts` rows, roll distances defaulting to the entry's expected
/// value, and composition offsets (zero-filled when the `trun` carries
/// none but the table already has offsets).
fn push_fragment_run(
    table: &mut SampleTable,
    run: &FragmentRun,
    entry_idx: usize,
    want_roll: i16,
) -> Result<(), IamfMp4Error> {
    for &delta in &run.durations {
        let extend = match table.stts.last() {
            Some(&(count, row_delta)) => row_delta == delta && count < u32::MAX,
            None => false,
        };
        if extend {
            if let Some(last) = table.stts.last_mut() {
                last.0 += 1;
            }
        } else {
            table.stts.push((1, delta));
        }
    }
    let added = u64::try_from(run.sizes.len())
        .ok()
        .ok_or(IamfMp4Error::BadSampleTable("fragment count overflow"))?;
    table.sample_count = table
        .sample_count
        .checked_add(added)
        .ok_or(IamfMp4Error::BadSampleTable("fragment count overflow"))?;
    table.stts_count = table
        .stts_count
        .checked_add(added)
        .ok_or(IamfMp4Error::BadSampleTable("fragment count overflow"))?;
    let mut duration = table.duration;
    for &delta in &run.durations {
        duration = duration
            .checked_add(u64::from(delta))
            .ok_or(IamfMp4Error::BadSampleTable("fragment duration overflow"))?;
    }
    table.duration = duration;
    table.runs.push(SampleRun {
        offset: run.offset,
        sizes: run.sizes.clone(),
        entry_idx,
    });
    if !table.sample_roll.is_empty() {
        table
            .sample_roll
            .extend(std::iter::repeat_n(want_roll, run.sizes.len()));
    }
    // `sample_ctts` stays all-or-nothing: backfill zeros when either
    // side carries offsets.
    if !run.ctts.is_empty() || !table.sample_ctts.is_empty() {
        if table.sample_ctts.is_empty() {
            let prior = usize::try_from(table.sample_count - added)
                .ok()
                .ok_or(IamfMp4Error::BadSampleTable("fragment count overflow"))?;
            table.sample_ctts.resize(prior, 0);
        }
        if run.ctts.is_empty() {
            table
                .sample_ctts
                .extend(std::iter::repeat_n(0, run.sizes.len()));
        } else {
            table.sample_ctts.extend(run.ctts.iter().copied());
        }
    }
    Ok(())
}

/// Collected `traf` children: headers plus dependency/group signals.
struct TrafParts {
    tfhd: Option<Tfhd>,
    tfdt: Option<u64>,
    truns: Vec<BoxHeader>,
    has_sdtp: bool,
    has_aux_info: bool,
    groups: GroupState,
}

/// Walk one `traf` box, collecting its children.
fn collect_traf<R: Read + Seek>(
    r: &mut R,
    traf: &BoxHeader,
    depth: u8,
    file_len: u64,
) -> Result<TrafParts, IamfMp4Error> {
    let end = traf.end_offset.unwrap_or(file_len);
    r.seek(SeekFrom::Start(traf.payload_offset))?;
    let mut parts = TrafParts {
        tfhd: None,
        tfdt: None,
        truns: Vec::new(),
        has_sdtp: false,
        has_aux_info: false,
        groups: GroupState::default(),
    };
    walk_children(r, end, depth + 1, |r, child, _| {
        match &child.typ {
            b"tfhd" => {
                if parts.tfhd.is_some() {
                    return Err(IamfMp4Error::MalformedBox("traf", "multiple tfhd"));
                }
                parts.tfhd = Some(parse_tfhd(&child.read_payload(r)?)?);
            }
            b"tfdt" => {
                if parts.tfdt.is_some() {
                    return Err(IamfMp4Error::MalformedBox("traf", "multiple tfdt"));
                }
                parts.tfdt = Some(parse_tfdt(&child.read_payload(r)?)?);
            }
            b"trun" => parts.truns.push(*child),
            b"sdtp" => parts.has_sdtp = true,
            b"sbgp" => parts.groups.collect_sbgp("traf", r, child)?,
            b"sgpd" => parts.groups.collect_sgpd("traf", r, child)?,
            // Auxiliary info only exists for encrypted samples.
            b"saiz" | b"saio" => parts.has_aux_info = true,
            _ => child.skip(r)?,
        }
        Ok(true)
    })?;
    Ok(parts)
}

/// Merge one `traf` into its track's sample table.
#[allow(clippy::too_many_arguments, reason = "fragment parse context")]
fn append_traf<R: Read + Seek>(
    r: &mut R,
    traf: &BoxHeader,
    depth: u8,
    moof_start: u64,
    defaults: &HashMap<u32, TrackDefaults>,
    configs: &mut [IamfTrackConfig],
    file_len: u64,
) -> Result<(), IamfMp4Error> {
    let parts = collect_traf(r, traf, depth, file_len)?;
    let tfhd = parts.tfhd.ok_or(IamfMp4Error::MissingBox("traf/tfhd"))?;
    if tfhd.duration_is_empty {
        return Ok(());
    }
    if parts.has_sdtp {
        return Err(IamfMp4Error::Unsupported(
            "sdtp sample dependencies in fragment",
        ));
    }
    if parts.groups.has_seig {
        return Err(IamfMp4Error::Unsupported(
            "encrypted samples (seig sample groups) in fragment",
        ));
    }
    if parts.has_aux_info {
        return Err(IamfMp4Error::Unsupported(
            "encrypted sample auxiliary info (saiz/saio) in fragment",
        ));
    }
    if parts.groups.has_groups {
        return Err(IamfMp4Error::Unsupported(
            "non-roll sample groups (sgpd/sbgp) in fragment",
        ));
    }
    let config = configs
        .iter_mut()
        .find(|c| c.tkhd_id == tfhd.track_id)
        .ok_or(IamfMp4Error::MalformedBox(
            "traf",
            "tfhd references unknown track",
        ))?;
    let trex = defaults.get(&tfhd.track_id).copied().unwrap_or_default();
    let desc = tfhd
        .description_index
        .or((trex.description_index != 0).then_some(trex.description_index))
        .ok_or(IamfMp4Error::MalformedBox(
            "tfhd",
            "no sample description index",
        ))?;
    let entry_idx = usize::try_from(desc)
        .ok()
        .and_then(|d| d.checked_sub(1))
        .filter(|&d| d < config.entries.len())
        .ok_or(IamfMp4Error::BadSampleTable(
            "fragment references unknown sample entry",
        ))?;
    let default_duration = tfhd
        .default_duration
        .or((trex.duration != 0).then_some(trex.duration));
    let default_size = tfhd.default_size.or((trex.size != 0).then_some(trex.size));
    let base = match tfhd.base_offset {
        Some(base) => base,
        None if tfhd.default_base_is_moof => moof_start,
        None => {
            return Err(IamfMp4Error::MalformedBox("tfhd", "no base data offset"));
        }
    };
    if parts
        .tfdt
        .is_some_and(|decode_time| decode_time != config.sample_table.duration)
    {
        return Err(IamfMp4Error::MalformedBox(
            "tfdt",
            "fragment decode time discontinuity",
        ));
    }
    let want_roll = config.entries[entry_idx].codec_config.audio_roll_distance;
    let mut implicit = base;
    let mut traf_samples = 0u64;
    for trun_header in &parts.truns {
        let payload = trun_header.read_payload(r)?;
        let run = parse_trun(
            &payload,
            base,
            &mut implicit,
            default_duration,
            default_size,
        )?;
        let added = u64::try_from(run.sizes.len())
            .ok()
            .ok_or(IamfMp4Error::BadSampleTable("fragment count overflow"))?;
        traf_samples = traf_samples
            .checked_add(added)
            .ok_or(IamfMp4Error::BadSampleTable("fragment count overflow"))?;
        push_fragment_run(&mut config.sample_table, &run, entry_idx, want_roll)?;
    }
    validate_traf_roll(
        parts.groups.sbgp,
        parts.groups.roll_descriptions,
        traf_samples,
        want_roll,
    )?;
    Ok(())
}

/// Validate traf-level `roll` groups against the fragment's samples.
///
/// Row counts must partition the traf's samples exactly and every
/// distance must equal the entry's expected roll distance (mirroring
/// the `stbl` rules).
fn validate_traf_roll(
    sbgp: Option<SbgpGrouping>,
    roll_descriptions: Option<Vec<i16>>,
    traf_samples: u64,
    want_roll: i16,
) -> Result<(), IamfMp4Error> {
    let Some((_, rows)) = sbgp else {
        if roll_descriptions.is_some() {
            return Err(IamfMp4Error::MalformedBox(
                "traf",
                "roll descriptions without grouping",
            ));
        }
        return Ok(());
    };
    let descriptions = roll_descriptions.ok_or(IamfMp4Error::MissingBox("traf/sgpd"))?;
    let mut covered = 0u64;
    for &(count, index) in &rows {
        let distance = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| descriptions.get(i).copied())
            .ok_or(IamfMp4Error::BadSampleTable(
                "sample group references unknown description",
            ))?;
        if distance != want_roll {
            return Err(IamfMp4Error::BadSampleTable("roll distance mismatch"));
        }
        covered = covered
            .checked_add(u64::from(count))
            .ok_or(IamfMp4Error::BadSampleTable("fragment count overflow"))?;
    }
    if covered != traf_samples {
        return Err(IamfMp4Error::BadSampleTable(
            "sample grouping does not cover all samples",
        ));
    }
    Ok(())
}
