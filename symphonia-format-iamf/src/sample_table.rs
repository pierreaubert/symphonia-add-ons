//! Sample table parsing (`stbl`) and media-header helpers (`mdhd`, `elst`).
//!
//! The IAMF profile uses uniform, unhinted, unencrypted samples, so the
//! demuxer only needs the classic tables: `stts` (kept for duration
//! accounting), `stsc` + `stsz`/`stz2` + `stco`/`co64` expanded into
//! [`SampleRun`]s, and `sdtp`/`sgpd`/`sbgp` rejected when they signal
//! dependencies or encryption.

use std::io::{Read, Seek, SeekFrom};

use crate::boxes::{BoxHeader, SliceReader, read_full_box, walk_children};
use crate::error::IamfMp4Error;

/// One contiguous run of samples: file offsets and sizes in decode order.
#[derive(Debug, Clone)]
pub struct SampleRun {
    /// Byte offset of the first sample in the file.
    pub offset: u64,
    /// Byte size of each sample in the run.
    pub sizes: Vec<u32>,
    /// 0-based index into the track's sample entries (`stsd` order).
    pub entry_idx: usize,
}

/// A fully expanded sample table for one track.
#[derive(Debug, Clone, Default)]
pub struct SampleTable {
    /// Sample runs in decode order.
    pub runs: Vec<SampleRun>,
    /// Total number of samples.
    pub sample_count: u64,
    /// Total media duration in track timescale units (from `stts`).
    pub duration: u64,
    /// Sample count implied by `stts` (must match `sample_count`).
    pub stts_count: u64,
    /// Raw `stts` rows kept for per-packet timestamp expansion.
    pub stts: Vec<(u32, u32)>,
    /// Per-sample roll distances from `sbgp`/`sgpd` (`roll` grouping),
    /// in decode order. Empty when the track carries no roll groups.
    pub sample_roll: Vec<i16>,
    /// Sync sample numbers from `stss` (1-based, ascending). `None`
    /// means every sample is a sync sample.
    pub sync_samples: Option<Vec<u32>>,
    /// Per-sample composition offsets from `ctts` (plus fragment `trun`
    /// offsets), in decode order. Empty when all offsets are zero.
    pub sample_ctts: Vec<i64>,
}

impl SampleTable {
    /// Number of samples across all runs.
    #[must_use]
    pub fn run_sample_count(&self) -> u64 {
        self.runs.iter().map(|r| r.sizes.len() as u64).sum()
    }
}

/// Widen a 32-bit table count for host indexing.
fn widen_count(value: u32) -> Result<usize, IamfMp4Error> {
    usize::try_from(value)
        .ok()
        .ok_or(IamfMp4Error::BadSampleTable("count overflow"))
}

/// Fill an optional table slot, rejecting a repeated box.
fn set_once<T>(slot: &mut Option<T>, value: T, repeated: &'static str) -> Result<(), IamfMp4Error> {
    if slot.is_some() {
        return Err(IamfMp4Error::MalformedBox("stbl", repeated));
    }
    *slot = Some(value);
    Ok(())
}

/// Movie/media header for one track (`mdhd`).
#[derive(Debug, Clone, Copy)]
pub struct MediaHeader {
    /// Track timescale (units per second).
    pub timescale: u32,
    /// Track duration in timescale units.
    pub duration: u64,
}

/// Parse `mdhd` (version 0 or 1).
pub fn parse_mdhd(payload: &[u8]) -> Result<MediaHeader, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    if version == 1 {
        r.skip(16)?; // creation + modification
        Ok(MediaHeader {
            timescale: r.u32_be()?,
            duration: r.u64_be()?,
        })
    } else {
        r.skip(8)?; // creation + modification
        Ok(MediaHeader {
            timescale: r.u32_be()?,
            duration: u64::from(r.u32_be()?),
        })
    }
}

/// One `elst` entry: a presentation segment mapped onto media time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditEntry {
    /// Segment duration in movie-timescale units.
    pub segment_duration: u64,
    /// Media start in media-timescale units (-1 = empty edit).
    pub media_time: i64,
    /// Media rate as combined 16.16 fixed point (`0x0001_0000` = 1.0).
    pub media_rate: u32,
}

/// Unity media rate (16.16 fixed point 1.0).
const MEDIA_RATE_UNITY: u32 = 0x0001_0000;

/// Parse an `elst` box into its edit entries (versions 0 and 1).
///
/// An empty edit list parses to an empty vec (identity timeline).
/// Trailing bytes past the entries are tolerated as future fields.
///
/// # Errors
///
/// Rejects unknown box versions and truncated entries.
pub fn parse_elst(payload: &[u8]) -> Result<Vec<EditEntry>, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    if version > 1 {
        return Err(IamfMp4Error::Unsupported("elst version"));
    }
    let mut r = SliceReader::new(rest);
    let entry_count = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entry_count.min(1024));
    for _ in 0..entry_count {
        if version == 1 {
            out.push(EditEntry {
                segment_duration: r.u64_be()?,
                media_time: r.u64_be()?.cast_signed(),
                media_rate: r.u32_be()?,
            });
        } else {
            out.push(EditEntry {
                segment_duration: u64::from(r.u32_be()?),
                media_time: i64::from(r.u32_be()?.cast_signed()),
                media_rate: r.u32_be()?,
            });
        }
    }
    Ok(out)
}

/// One selected media range plus its presentation placement.
///
/// Media edits select whole samples: the range covers every sample whose
/// media dts falls inside the edit, so an edit starting mid-sample drops
/// that packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditSegment {
    /// First selected sample ordinal (into the merged table).
    pub ord_start: u64,
    /// One-past-last selected sample ordinal.
    pub ord_end: u64,
    /// Presentation timestamp of `ord_start`, in media-timescale units.
    pub pts_base: i64,
    /// Media dts of `ord_start`, in media-timescale units.
    pub media_base: i64,
}

/// Presentation timeline derived from an edit list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditPlan {
    /// Selected segments in presentation order.
    pub segments: Vec<EditSegment>,
    /// Total presentation duration in media-timescale units, including
    /// empty edits.
    pub presentation_duration: u64,
    /// Total selected samples across segments.
    pub selected_samples: u64,
}

impl EditPlan {
    /// Identity plan: every sample selected, presentation == media time.
    #[must_use]
    pub fn identity(table: &SampleTable) -> Self {
        let segments = if table.sample_count == 0 {
            Vec::new()
        } else {
            vec![EditSegment {
                ord_start: 0,
                ord_end: table.sample_count,
                pts_base: 0,
                media_base: 0,
            }]
        };
        Self {
            segments,
            presentation_duration: table.duration,
            selected_samples: table.sample_count,
        }
    }

    /// Timestamp of the first emitted packet (0 when nothing selected).
    #[must_use]
    pub fn first_pts(&self) -> i64 {
        self.segments.first().map_or(0, |s| s.pts_base)
    }
}

/// Build the presentation plan for `edits` against the merged table.
///
/// Rules: empty edits advance presentation time without selecting media;
/// media edits select whole samples and must be ordered and disjoint
/// (repeats and backward jumps cannot stream forward); ranges past the
/// media end clamp; segment durations convert from the movie timescale
/// with round-to-nearest.
///
/// # Errors
///
/// Rejects negative media times other than -1, dwell edits, non-unity
/// media rates, and overlapping or backward media ranges.
pub fn plan_edits(
    edits: &[EditEntry],
    movie_timescale: u32,
    media_timescale: u32,
    table: &SampleTable,
) -> Result<EditPlan, IamfMp4Error> {
    if edits.is_empty() {
        return Ok(EditPlan::identity(table));
    }
    let mut segments = Vec::new();
    let mut presentation = 0u64;
    let mut prev_ord_end = 0u64;
    let mut selected = 0u64;
    for edit in edits {
        let seg_media = convert_duration(edit.segment_duration, movie_timescale, media_timescale)?;
        if edit.media_time < 0 {
            if edit.media_time != -1 {
                return Err(IamfMp4Error::MalformedBox("elst", "negative media time"));
            }
            presentation = presentation
                .checked_add(seg_media)
                .ok_or(IamfMp4Error::MalformedBox("elst", "edit time overflow"))?;
            continue;
        }
        if edit.media_rate == 0 {
            return Err(IamfMp4Error::Unsupported("elst dwell edit"));
        }
        if edit.media_rate != MEDIA_RATE_UNITY {
            return Err(IamfMp4Error::Unsupported("elst media rate"));
        }
        let range_start = u64::try_from(edit.media_time)
            .ok()
            .ok_or(IamfMp4Error::MalformedBox("elst", "edit time overflow"))?;
        let range_end = range_start
            .checked_add(seg_media)
            .ok_or(IamfMp4Error::MalformedBox("elst", "edit time overflow"))?;
        let (ord_start, media_base) = ordinal_at(table, range_start);
        let (ord_end, _) = ordinal_at(table, range_end);
        if ord_start < ord_end {
            if ord_start < prev_ord_end {
                return Err(IamfMp4Error::Unsupported(
                    "elst media ranges must be ordered and disjoint",
                ));
            }
            let pts_base = i64::try_from(presentation)
                .ok()
                .ok_or(IamfMp4Error::MalformedBox("elst", "edit time overflow"))?;
            let media_base = i64::try_from(media_base)
                .ok()
                .ok_or(IamfMp4Error::MalformedBox("elst", "edit time overflow"))?;
            segments.push(EditSegment {
                ord_start,
                ord_end,
                pts_base,
                media_base,
            });
            selected += ord_end - ord_start;
            prev_ord_end = ord_end;
        }
        presentation = presentation
            .checked_add(seg_media)
            .ok_or(IamfMp4Error::MalformedBox("elst", "edit time overflow"))?;
    }
    Ok(EditPlan {
        segments,
        presentation_duration: presentation,
        selected_samples: selected,
    })
}

/// First sample ordinal with dts >= `media_time`, plus its dts.
/// Returns `(sample_count, table.duration)` past the media end.
fn ordinal_at(table: &SampleTable, media_time: u64) -> (u64, u64) {
    let mut ordinal = 0u64;
    let mut dts = 0u64;
    for &(count, delta) in &table.stts {
        if media_time <= dts {
            return (ordinal, dts);
        }
        let row_samples = u64::from(count);
        if delta == 0 {
            // Zero-duration row past the boundary: skip it whole.
            ordinal += row_samples;
            continue;
        }
        // Validated `stts` totals bound these sums: no overflow.
        let step = u64::from(delta);
        let skip = media_time
            .saturating_sub(dts)
            .div_ceil(step)
            .min(row_samples);
        ordinal += skip;
        dts += skip * step;
        if skip < row_samples {
            return (ordinal, dts);
        }
    }
    (ordinal, dts)
}

/// Convert a movie-timescale duration into media-timescale units,
/// rounding to nearest.
fn convert_duration(
    movie_duration: u64,
    movie_timescale: u32,
    media_timescale: u32,
) -> Result<u64, IamfMp4Error> {
    let scaled = u128::from(movie_duration) * u128::from(media_timescale);
    let rounded = (scaled + u128::from(movie_timescale) / 2) / u128::from(movie_timescale);
    u64::try_from(rounded)
        .ok()
        .ok_or(IamfMp4Error::MalformedBox("elst", "edit time overflow"))
}

/// Parse the `mvhd` timescale (version 0 or 1).
///
/// # Errors
///
/// Rejects unknown versions, truncated boxes, and zero timescales.
pub fn parse_mvhd_timescale(payload: &[u8]) -> Result<u32, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    if version == 1 {
        r.skip(16)?; // creation + modification
    } else if version == 0 {
        r.skip(8)?;
    } else {
        return Err(IamfMp4Error::Unsupported("mvhd version"));
    }
    let timescale = r.u32_be()?;
    if timescale == 0 {
        return Err(IamfMp4Error::MalformedBox("mvhd", "zero timescale"));
    }
    Ok(timescale)
}

/// Parse the `stbl` box into an expanded [`SampleTable`].
///
/// An init segment may carry zero chunks and zero samples (no `stsc`
/// rows): the table starts empty and `moof` fragments extend it.
pub fn parse_stbl<R: Read + Seek>(
    r: &mut R,
    stbl: &BoxHeader,
    entry_count: usize,
) -> Result<SampleTable, IamfMp4Error> {
    let end = stbl.end_offset.unwrap_or(u64::MAX);
    r.seek(SeekFrom::Start(stbl.payload_offset))?;

    let mut sample_sizes: Option<Vec<u32>> = None;
    let mut chunk_offsets: Option<Vec<u64>> = None;
    let mut chunk_map: Vec<StscRow> = Vec::new();
    let mut stts: Vec<(u32, u32)> = Vec::new(); // (sample_count, sample_delta)
    let mut has_sdtp = false;
    let mut has_aux_info = false;
    let mut groups = GroupState::default();
    let mut sync_samples: Option<Vec<u32>> = None;
    let mut ctts: Option<Vec<(u32, i64)>> = None;

    walk_children(r, end, 5, |r, child, _| {
        match &child.typ {
            b"stts" => {
                stts = parse_stts(&child.read_payload(r)?)?;
            }
            b"stsc" => {
                chunk_map = parse_stsc(&child.read_payload(r)?)?;
            }
            b"stsz" => {
                sample_sizes = Some(parse_stsz(&child.read_payload(r)?)?);
            }
            b"stz2" => {
                sample_sizes = Some(parse_stz2(&child.read_payload(r)?)?);
            }
            b"stco" => {
                chunk_offsets = Some(
                    parse_stco(&child.read_payload(r)?)?
                        .into_iter()
                        .map(u64::from)
                        .collect(),
                );
            }
            b"co64" => {
                chunk_offsets = Some(parse_co64(&child.read_payload(r)?)?);
            }
            b"sdtp" => has_sdtp = true,
            b"stss" => {
                set_once(
                    &mut sync_samples,
                    parse_stss(&child.read_payload(r)?)?,
                    "multiple sync tables",
                )?;
            }
            b"ctts" => {
                set_once(
                    &mut ctts,
                    parse_ctts(&child.read_payload(r)?)?,
                    "multiple composition offset tables",
                )?;
            }
            b"sbgp" => groups.collect_sbgp("stbl", r, child)?,
            b"sgpd" => groups.collect_sgpd("stbl", r, child)?,
            // Auxiliary info only exists for encrypted samples.
            b"saiz" | b"saio" => has_aux_info = true,
            // Shadow (`stsh`), partial-sync (`stps`), and padding
            // (`padb`) tables carry no information the IAMF profile needs.
            _ => {}
        }
        child.skip(r)?;
        Ok(true)
    })?;

    finish_table(
        has_sdtp,
        has_aux_info,
        groups,
        sample_sizes,
        chunk_offsets,
        &chunk_map,
        stts,
        sync_samples,
        ctts,
        entry_count,
    )
}

/// Assemble the collected `stbl` parts into an expanded [`SampleTable`].
#[allow(clippy::too_many_arguments, reason = "collected table parts")]
fn finish_table(
    has_sdtp: bool,
    has_aux_info: bool,
    groups: GroupState,
    sample_sizes: Option<Vec<u32>>,
    chunk_offsets: Option<Vec<u64>>,
    chunk_map: &[StscRow],
    stts: Vec<(u32, u32)>,
    sync_samples: Option<Vec<u32>>,
    ctts: Option<Vec<(u32, i64)>>,
    entry_count: usize,
) -> Result<SampleTable, IamfMp4Error> {
    if has_sdtp {
        return Err(IamfMp4Error::Unsupported(
            "sdtp sample dependencies in IAMF track",
        ));
    }
    if groups.has_seig {
        return Err(IamfMp4Error::Unsupported(
            "encrypted samples (seig sample groups) in IAMF track",
        ));
    }
    if has_aux_info {
        return Err(IamfMp4Error::Unsupported(
            "encrypted sample auxiliary info (saiz/saio) in IAMF track",
        ));
    }
    if groups.has_groups {
        return Err(IamfMp4Error::Unsupported(
            "non-roll sample groups (sgpd/sbgp) in IAMF track",
        ));
    }
    let sizes = sample_sizes.ok_or(IamfMp4Error::MissingBox("stbl/stsz"))?;
    let offsets = chunk_offsets.ok_or(IamfMp4Error::MissingBox("stbl/stco"))?;
    if chunk_map.is_empty() && (!offsets.is_empty() || !sizes.is_empty()) {
        return Err(IamfMp4Error::MissingBox("stbl/stsc"));
    }
    let runs = expand_runs(&offsets, chunk_map, &sizes, entry_count)?;

    let (stts_count, duration) = stts_totals(&stts)?;
    let sample_count = u64::try_from(sizes.len())
        .ok()
        .ok_or(IamfMp4Error::BadSampleTable("sample count overflow"))?;
    let sample_roll = expand_roll(groups.sbgp, groups.roll_descriptions, &sizes)?;
    let sync_samples = validate_sync(sync_samples, sample_count)?;
    let sample_ctts = expand_ctts(ctts, sample_count)?;
    let table = SampleTable {
        runs,
        sample_count,
        duration,
        stts_count,
        stts,
        sample_roll,
        sync_samples,
        sample_ctts,
    };
    if table.stts_count != table.sample_count {
        return Err(IamfMp4Error::BadSampleTable(
            "stts sample count disagrees with stsz",
        ));
    }
    Ok(table)
}

/// Validate `stss` entries: non-empty, 1-based, in range, ascending.
///
/// Sync-ness never gates emission (playback always starts at sample 0
/// and IAMF codecs decode every frame independently); the validated
/// table is kept for API completeness.
fn validate_sync(
    sync_samples: Option<Vec<u32>>,
    sample_count: u64,
) -> Result<Option<Vec<u32>>, IamfMp4Error> {
    let Some(entries) = sync_samples else {
        return Ok(None);
    };
    if entries.is_empty() {
        return Err(IamfMp4Error::MalformedBox("stss", "no sync samples"));
    }
    let mut prev = 0u32;
    for &sample in &entries {
        if sample <= prev {
            return Err(IamfMp4Error::BadSampleTable("stss entries not ordered"));
        }
        if u64::from(sample) > sample_count {
            return Err(IamfMp4Error::BadSampleTable("stss entry out of range"));
        }
        prev = sample;
    }
    Ok(Some(entries))
}

/// Expand `ctts` rows into per-sample composition offsets. Returns an
/// empty vec when the box is absent or every offset is zero; the row
/// counts must partition all samples exactly.
fn expand_ctts(ctts: Option<Vec<(u32, i64)>>, sample_count: u64) -> Result<Vec<i64>, IamfMp4Error> {
    let Some(rows) = ctts else {
        return Ok(Vec::new());
    };
    let mut offsets = Vec::new();
    for &(count, offset) in &rows {
        let count = widen_count(count)?;
        offsets.extend(std::iter::repeat_n(offset, count));
    }
    if u64::try_from(offsets.len()).ok() != Some(sample_count) {
        return Err(IamfMp4Error::BadSampleTable(
            "ctts sample count disagrees with stsz",
        ));
    }
    if offsets.iter().all(|&o| o == 0) {
        return Ok(Vec::new());
    }
    Ok(offsets)
}

/// Parse `stss`: 1-based sync sample numbers.
fn parse_stss(payload: &[u8]) -> Result<Vec<u32>, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    if version != 0 {
        return Err(IamfMp4Error::Unsupported("stss version"));
    }
    let mut r = SliceReader::new(rest);
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1024));
    for _ in 0..entries {
        out.push(r.u32_be()?);
    }
    Ok(out)
}

/// Parse `ctts`: `(sample_count, composition_offset)` rows. Version 0
/// carries unsigned offsets, version 1 signed.
fn parse_ctts(payload: &[u8]) -> Result<Vec<(u32, i64)>, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    if version > 1 {
        return Err(IamfMp4Error::Unsupported("ctts version"));
    }
    let mut r = SliceReader::new(rest);
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1024));
    for _ in 0..entries {
        let count = r.u32_be()?;
        let offset = if version == 1 {
            i64::from(r.u32_be()?.cast_signed())
        } else {
            i64::from(r.u32_be()?)
        };
        out.push((count, offset));
    }
    Ok(out)
}

/// Expand `sbgp` rows against `sgpd` roll entries into per-sample roll
/// distances. Returns an empty vec when no roll grouping is present; the
/// row counts must partition all samples exactly.
fn expand_roll(
    sbgp: Option<SbgpGrouping>,
    roll_descriptions: Option<Vec<i16>>,
    sizes: &[u32],
) -> Result<Vec<i16>, IamfMp4Error> {
    let Some((_, rows)) = sbgp else {
        if roll_descriptions.is_some() {
            return Err(IamfMp4Error::MalformedBox(
                "stbl",
                "roll descriptions without grouping",
            ));
        }
        return Ok(Vec::new());
    };
    let descriptions = roll_descriptions.ok_or(IamfMp4Error::MissingBox("stbl/sgpd"))?;
    let mut roll = Vec::with_capacity(sizes.len());
    for &(count, index) in &rows {
        // Description indices are 1-based; 0 is never valid.
        let distance = usize::try_from(index)
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| descriptions.get(i).copied())
            .ok_or(IamfMp4Error::BadSampleTable(
                "sample group references unknown description",
            ))?;
        let count = widen_count(count)?;
        roll.extend(std::iter::repeat_n(distance, count));
    }
    if roll.len() != sizes.len() {
        return Err(IamfMp4Error::BadSampleTable(
            "sample grouping does not cover all samples",
        ));
    }
    Ok(roll)
}

/// Parse `sbgp`: grouping type plus `(sample_count, description index)` rows.
/// Version 0 and 1 both parse (the v1 grouping parameter is ignored).
pub(crate) fn parse_sbgp(payload: &[u8]) -> Result<SbgpGrouping, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let grouping = r.fourcc()?;
    if version == 1 {
        r.skip(4)?; // grouping_type_parameter
    } else if version > 1 {
        return Err(IamfMp4Error::Unsupported("sbgp version > 1"));
    }
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1024));
    for _ in 0..entries {
        out.push((r.u32_be()?, r.u32_be()?));
    }
    Ok((grouping, out))
}

/// Parse `sgpd` roll descriptions: one i16 roll distance per entry.
/// Version 1 entries are bare i16s; version 2 must declare the 2-byte
/// entry size up front.
pub(crate) fn parse_sgpd(payload: &[u8]) -> Result<([u8; 4], Vec<i16>), IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let grouping = r.fourcc()?;
    if grouping != *b"roll" {
        return Ok((grouping, Vec::new()));
    }
    if version == 2 {
        let default_length = r.u32_be()?;
        if default_length != 2 {
            return Err(IamfMp4Error::MalformedBox("sgpd", "bad roll entry size"));
        }
    } else if version != 1 {
        return Err(IamfMp4Error::Unsupported("sgpd version"));
    }
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1024));
    for _ in 0..entries {
        out.push(r.u16_be()?.cast_signed());
    }
    Ok((grouping, out))
}

/// Expand `stsc` + `stsz` + `stco`/`co64` into contiguous sample runs.
fn expand_runs(
    offsets: &[u64],
    chunk_map: &[StscRow],
    sizes: &[u32],
    entry_count: usize,
) -> Result<Vec<SampleRun>, IamfMp4Error> {
    let mut runs = Vec::with_capacity(offsets.len());
    let mut sample_idx = 0usize;
    for (chunk_i, &offset) in offsets.iter().enumerate() {
        let chunk_no = u32::try_from(chunk_i)
            .ok()
            .ok_or(IamfMp4Error::BadSampleTable("too many chunks"))?
            + 1;
        let (per_chunk, desc) = chunk_row(chunk_map, chunk_no).ok_or(
            IamfMp4Error::BadSampleTable("stsc does not cover all chunks"),
        )?;
        let entry_idx = usize::try_from(desc)
            .ok()
            .filter(|&d| d >= 1 && d <= entry_count)
            .map(|d| d - 1)
            .ok_or(IamfMp4Error::BadSampleTable(
                "stsc references unknown sample entry",
            ))?;
        let end_idx = sample_idx
            .checked_add(widen_count(per_chunk)?)
            .ok_or(IamfMp4Error::BadSampleTable("chunk overflow"))?;
        if end_idx > sizes.len() {
            return Err(IamfMp4Error::BadSampleTable(
                "stsc references more samples than stsz holds",
            ));
        }
        runs.push(SampleRun {
            offset,
            sizes: sizes[sample_idx..end_idx].to_vec(),
            entry_idx,
        });
        sample_idx = end_idx;
    }
    if sample_idx != sizes.len() {
        return Err(IamfMp4Error::BadSampleTable(
            "stsc leaves trailing samples uncovered",
        ));
    }
    Ok(runs)
}

/// Row covering a 1-based chunk number: samples per chunk and entry.
fn chunk_row(chunk_map: &[StscRow], chunk_no: u32) -> Option<(u32, u32)> {
    let mut current = None;
    for &(first, per, desc) in chunk_map {
        if first <= chunk_no {
            current = Some((per, desc));
        } else {
            break;
        }
    }
    current
}

/// Totals from `stts` rows with overflow checks.
fn stts_totals(stts: &[(u32, u32)]) -> Result<(u64, u64), IamfMp4Error> {
    let mut count = 0u64;
    let mut duration = 0u64;
    for &(n, delta) in stts {
        count = count
            .checked_add(u64::from(n))
            .ok_or(IamfMp4Error::BadSampleTable("stts count overflow"))?;
        duration = duration
            .checked_add(u64::from(n).saturating_mul(u64::from(delta)))
            .ok_or(IamfMp4Error::BadSampleTable("stts duration overflow"))?;
    }
    Ok((count, duration))
}

fn parse_stts(payload: &[u8]) -> Result<Vec<(u32, u32)>, IamfMp4Error> {
    let (_, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1024));
    for _ in 0..entries {
        out.push((r.u32_be()?, r.u32_be()?));
    }
    Ok(out)
}

/// One `stsc` row: first chunk, samples per chunk, 1-based sample entry.
pub type StscRow = (u32, u32, u32);

/// One `sbgp` grouping: type plus `(sample_count, description index)` rows.
pub(crate) type SbgpGrouping = ([u8; 4], Vec<(u32, u32)>);

/// Collected sample-group state: the `roll` grouping plus reject
/// signals for any other grouping. Shared by the `stbl` and `traf`
/// walks.
#[derive(Debug, Default)]
pub(crate) struct GroupState {
    pub(crate) sbgp: Option<SbgpGrouping>,
    pub(crate) roll_descriptions: Option<Vec<i16>>,
    pub(crate) has_groups: bool,
    pub(crate) has_seig: bool,
    sgpd_boxes: u32,
}

impl GroupState {
    /// Collect one `sbgp` box: `roll` groupings are kept, `seig`
    /// signals encryption, anything else signals rejection.
    pub(crate) fn collect_sbgp<R: Read + Seek>(
        &mut self,
        box_name: &'static str,
        r: &mut R,
        child: &BoxHeader,
    ) -> Result<(), IamfMp4Error> {
        if self.sbgp.is_some() {
            return Err(IamfMp4Error::MalformedBox(
                box_name,
                "multiple sample groupings",
            ));
        }
        let (grouping, rows) = parse_sbgp(&child.read_payload(r)?)?;
        if grouping == *b"roll" {
            self.sbgp = Some((grouping, rows));
        } else if grouping == *b"seig" {
            self.has_seig = true;
        } else {
            self.has_groups = true;
        }
        Ok(())
    }

    /// Collect one `sgpd` box, mirroring [`Self::collect_sbgp`].
    pub(crate) fn collect_sgpd<R: Read + Seek>(
        &mut self,
        box_name: &'static str,
        r: &mut R,
        child: &BoxHeader,
    ) -> Result<(), IamfMp4Error> {
        self.sgpd_boxes += 1;
        if self.sgpd_boxes > 1 {
            return Err(IamfMp4Error::MalformedBox(
                box_name,
                "multiple sample group descriptions",
            ));
        }
        let (grouping, entries) = parse_sgpd(&child.read_payload(r)?)?;
        if grouping == *b"roll" {
            self.roll_descriptions = Some(entries);
        } else if grouping == *b"seig" {
            self.has_seig = true;
        } else {
            self.has_groups = true;
        }
        Ok(())
    }
}

fn parse_stsc(payload: &[u8]) -> Result<Vec<StscRow>, IamfMp4Error> {
    let (_, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1024));
    let mut last_first = 0u32;
    for _ in 0..entries {
        let first = r.u32_be()?;
        let per = r.u32_be()?;
        let desc = r.u32_be()?;
        if first < last_first || per == 0 || desc == 0 {
            return Err(IamfMp4Error::BadSampleTable("invalid stsc row"));
        }
        last_first = first;
        out.push((first, per, desc));
    }
    Ok(out)
}

fn parse_stsz(payload: &[u8]) -> Result<Vec<u32>, IamfMp4Error> {
    let (_, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let default_size = r.u32_be()?;
    let count = widen_count(r.u32_be()?)?;
    if default_size != 0 {
        return Ok(vec![default_size; count]);
    }
    let mut out = Vec::with_capacity(count.min(1 << 20));
    for _ in 0..count {
        out.push(r.u32_be()?);
    }
    Ok(out)
}

fn parse_stz2(payload: &[u8]) -> Result<Vec<u32>, IamfMp4Error> {
    let (_, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    r.skip(3)?; // reserved
    let field_size = r.u8()?;
    let count = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(count.min(1 << 20));
    match field_size {
        4 => {
            for _ in 0..count.div_ceil(2) {
                let b = r.u8()?;
                out.push(u32::from(b >> 4));
                out.push(u32::from(b & 0x0F));
            }
            out.truncate(count);
        }
        8 => {
            for _ in 0..count {
                out.push(u32::from(r.u8()?));
            }
        }
        16 => {
            for _ in 0..count {
                out.push(u32::from(r.u16_be()?));
            }
        }
        _ => {
            return Err(IamfMp4Error::MalformedBox("stz2", "unsupported field size"));
        }
    }
    Ok(out)
}

fn parse_stco(payload: &[u8]) -> Result<Vec<u32>, IamfMp4Error> {
    let (_, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1 << 20));
    for _ in 0..entries {
        out.push(r.u32_be()?);
    }
    Ok(out)
}

fn parse_co64(payload: &[u8]) -> Result<Vec<u64>, IamfMp4Error> {
    let (_, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1 << 20));
    for _ in 0..entries {
        out.push(r.u64_be()?);
    }
    Ok(out)
}
