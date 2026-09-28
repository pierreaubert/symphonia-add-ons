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

/// Parse an `elst` box, returning the media time of the first edit
/// (version 0: u32, `0xFFFF_FFFF` = empty edit) if any edit exists.
pub fn parse_elst_media_time(payload: &[u8]) -> Result<Option<i64>, IamfMp4Error> {
    let (version, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let entry_count = r.u32_be()?;
    if entry_count == 0 {
        return Ok(None);
    }
    if version == 1 {
        r.skip(8)?; // segment_duration
        Ok(Some(r.u64_be()?.cast_signed()))
    } else {
        r.skip(4)?; // segment_duration
        Ok(Some(i64::from(r.u32_be()?.cast_signed())))
    }
}

/// Parse the `stbl` box into an expanded [`SampleTable`].
pub fn parse_stbl<R: Read + Seek>(
    r: &mut R,
    stbl: &BoxHeader,
) -> Result<SampleTable, IamfMp4Error> {
    let end = stbl.end_offset.unwrap_or(u64::MAX);
    r.seek(SeekFrom::Start(stbl.payload_offset))?;

    let mut sample_sizes: Option<Vec<u32>> = None;
    let mut chunk_offsets: Option<Vec<u64>> = None;
    let mut chunk_map: Vec<(u32, u32)> = Vec::new(); // (first_chunk, samples_per_chunk)
    let mut stts: Vec<(u32, u32)> = Vec::new(); // (sample_count, sample_delta)
    let mut has_sdtp = false;
    let mut has_groups = false;

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
            b"sgpd" | b"sbgp" => has_groups = true,
            // Sync, composition-offset, and shadow tables carry no
            // information the IAMF profile needs.
            _ => {}
        }
        child.skip(r)?;
        Ok(true)
    })?;

    if has_sdtp {
        return Err(IamfMp4Error::Unsupported(
            "sdtp sample dependencies in IAMF track",
        ));
    }
    if has_groups {
        return Err(IamfMp4Error::Unsupported(
            "sample groups (sgpd/sbgp) in IAMF track",
        ));
    }
    let sizes = sample_sizes.ok_or(IamfMp4Error::MissingBox("stbl/stsz"))?;
    let offsets = chunk_offsets.ok_or(IamfMp4Error::MissingBox("stbl/stco"))?;
    if chunk_map.is_empty() {
        return Err(IamfMp4Error::MissingBox("stbl/stsc"));
    }
    let runs = expand_runs(&offsets, &chunk_map, &sizes)?;

    let (stts_count, duration) = stts_totals(&stts)?;
    let sample_count = u64::try_from(sizes.len())
        .ok()
        .ok_or(IamfMp4Error::BadSampleTable("sample count overflow"))?;
    let table = SampleTable {
        runs,
        sample_count,
        duration,
        stts_count,
        stts,
    };
    if table.stts_count != table.sample_count {
        return Err(IamfMp4Error::BadSampleTable(
            "stts sample count disagrees with stsz",
        ));
    }
    Ok(table)
}

/// Expand `stsc` + `stsz` + `stco`/`co64` into contiguous sample runs.
fn expand_runs(
    offsets: &[u64],
    chunk_map: &[(u32, u32)],
    sizes: &[u32],
) -> Result<Vec<SampleRun>, IamfMp4Error> {
    let mut runs = Vec::with_capacity(offsets.len());
    let mut sample_idx = 0usize;
    for (chunk_i, &offset) in offsets.iter().enumerate() {
        let chunk_no = u32::try_from(chunk_i)
            .ok()
            .ok_or(IamfMp4Error::BadSampleTable("too many chunks"))?
            + 1;
        let per_chunk = samples_per_chunk(chunk_map, chunk_no).ok_or(
            IamfMp4Error::BadSampleTable("stsc does not cover all chunks"),
        )?;
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

/// Samples-per-chunk for a 1-based chunk number from `stsc` rows.
fn samples_per_chunk(chunk_map: &[(u32, u32)], chunk_no: u32) -> Option<u32> {
    let mut current = None;
    for &(first, per) in chunk_map {
        if first <= chunk_no {
            current = Some(per);
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

fn parse_stsc(payload: &[u8]) -> Result<Vec<(u32, u32)>, IamfMp4Error> {
    let (_, _, rest) = read_full_box(payload)?;
    let mut r = SliceReader::new(rest);
    let entries = widen_count(r.u32_be()?)?;
    let mut out = Vec::with_capacity(entries.min(1024));
    let mut last_first = 0u32;
    for _ in 0..entries {
        let first = r.u32_be()?;
        let per = r.u32_be()?;
        r.skip(4)?; // sample_description_index (must be 1; unchecked)
        if first < last_first || per == 0 {
            return Err(IamfMp4Error::BadSampleTable("invalid stsc row"));
        }
        last_first = first;
        out.push((first, per));
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
