//! Symphonia [`FormatReader`] for IAMF in ISO-BMFF.
//!
//! One Symphonia track per `trak`; each packet carries one raw codec frame
//! (Opus, AAC, FLAC, or LPCM — the demuxer never decodes). Timestamps come
//! from the `stts` table rebased through the `elst` edit list; only
//! rewind-to-start seeking is supported.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_FLAC, CODEC_ID_OPUS, CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S24BE,
    CODEC_ID_PCM_S32BE,
};
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{Error as SymphoniaError, Result as SymphoniaResult, SeekErrorKind};
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::{
    FormatId, FormatInfo, FormatOptions, FormatReader, MediaInfo, SeekMode, SeekTo, SeekedTo,
    Track, TrackFlags,
};
use symphonia_core::io::{MediaSourceStream, ReadBytes, ScopedStream};
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::packet::Packet;
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Time, TimeBase, Timestamp};
use symphonia_iamf_core::types::{CodecConfig, CodecId};

use crate::boxes::{BoxHeader, walk_children};
use crate::descriptors::{IamfTrackConfig, check_ftyp, parse_trak, validate_roll};
use crate::error::{IamfMp4Error, seek_err, to_symphonia_error};
use crate::fragments::{TrackDefaults, append_fragments, parse_mvex};
use crate::sample_table::parse_mvhd_timescale;

/// Format descriptor: IAMF audio carried in ISO-BMFF.
pub const FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"iamf")),
    short_name: "iamf",
    long_name: "IAMF immersive audio in ISO-BMFF",
};

/// Largest single sample buffered (8 MiB — far above any IAMF frame).
pub const MAX_SAMPLE_BYTES: u64 = 8 * 1024 * 1024;

impl std::fmt::Debug for IamfFormatReader<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IamfFormatReader")
            .field("tracks", &self.tracks)
            .field("cursors", &self.cursors)
            .field("cursor_index", &self.cursor_index)
            .field("media_info", &self.media_info)
            .finish_non_exhaustive()
    }
}

/// Decode cursor for one track's sample table.
#[derive(Debug)]
struct PacketCursor {
    symphonia_id: u32,
    config: IamfTrackConfig,
    run_idx: usize,
    sample_in_run: usize,
    next_offset: u64,
    next_media_dts: i64,
    ordinal: u64,
    seg_idx: usize,
    stts_row: usize,
    stts_left: u32,
    packet_index: u64,
}

impl PacketCursor {
    fn new(symphonia_id: u32, config: IamfTrackConfig) -> Self {
        let next_offset = config.sample_table.runs.first().map_or(0, |r| r.offset);
        // `advance_table` refills and skips exhausted rows, so seeding the
        // first row's count (possibly zero) is enough.
        let (stts_row, stts_left) = match config.sample_table.stts.first() {
            Some(&(count, _)) => (0, count),
            None => (0, 0),
        };
        Self {
            symphonia_id,
            config,
            run_idx: 0,
            sample_in_run: 0,
            next_offset,
            next_media_dts: 0,
            ordinal: 0,
            seg_idx: 0,
            stts_row,
            stts_left,
            packet_index: 0,
        }
    }

    /// Reset to the first sample (rewind).
    fn rewind(&mut self) {
        *self = Self::new(self.symphonia_id, self.config.clone());
    }

    /// Current sample size, advancing run cursors past exhaustion.
    fn sample_size(&mut self) -> Option<u32> {
        loop {
            let run = self.config.sample_table.runs.get(self.run_idx)?;
            if let Some(&size) = run.sizes.get(self.sample_in_run) {
                return Some(size);
            }
            self.run_idx += 1;
            self.sample_in_run = 0;
            self.next_offset = self.config.sample_table.runs.get(self.run_idx)?.offset;
        }
    }

    /// Consume the next selected sample: returns (offset, size, pts, dur).
    ///
    /// Table samples outside the edit plan's segments are skipped;
    /// emitted timestamps are rebased onto the presentation timeline.
    fn take_sample(&mut self) -> Option<(u64, u32, i64, u64)> {
        loop {
            let seg = *self.config.edit_plan.segments.get(self.seg_idx)?;
            while self.ordinal < seg.ord_start {
                self.advance_table()?;
            }
            if self.ordinal >= seg.ord_end {
                self.seg_idx += 1;
                continue;
            }
            let ordinal = self.ordinal;
            let (offset, size, media_dts, dur) = self.advance_table()?;
            let ctts = usize::try_from(ordinal)
                .ok()
                .and_then(|i| self.config.sample_table.sample_ctts.get(i).copied())
                .unwrap_or(0);
            let pts = seg.pts_base + (media_dts - seg.media_base) + ctts;
            self.packet_index += 1;
            return Some((offset, size, pts, dur));
        }
    }

    /// Consume the current table sample: returns (offset, size, media
    /// dts, dur).
    fn advance_table(&mut self) -> Option<(u64, u32, i64, u64)> {
        let size = self.sample_size()?;
        let offset = self.next_offset;
        let dts = self.next_media_dts;
        // Current stts delta, advancing past exhausted/zero rows.
        let dur = loop {
            let &(count, delta) = self.config.sample_table.stts.get(self.stts_row)?;
            if self.stts_left == 0 {
                if count == 0 {
                    self.stts_row += 1;
                    continue;
                }
                self.stts_left = count;
            }
            break u64::from(delta);
        };
        self.next_offset += u64::from(size);
        self.next_media_dts += dur.cast_signed();
        self.ordinal += 1;
        self.sample_in_run += 1;
        self.stts_left = self.stts_left.saturating_sub(1);
        if self.stts_left == 0 {
            self.stts_row += 1;
            // Skip zero-count rows eagerly so the next take lands on time.
            while let Some(&(count, _)) = self.config.sample_table.stts.get(self.stts_row) {
                if count > 0 {
                    break;
                }
                self.stts_row += 1;
            }
        }
        Some((offset, size, dts, dur))
    }
}

/// Symphonia format reader for IAMF ISO-BMFF files.
pub struct IamfFormatReader<'s> {
    reader: MediaSourceStream<'s>,
    tracks: Vec<Track>,
    cursors: Vec<PacketCursor>,
    cursor_index: usize,
    media_info: MediaInfo,
    metadata: MetadataLog,
}

impl<'s> IamfFormatReader<'s> {
    /// Parse all top-level boxes and build per-track demux state.
    ///
    /// `moof` fragments (with `mvex`/`trex` defaults from `moov`) extend
    /// each track's sample table in file order before packets flow.
    ///
    /// # Errors
    ///
    /// Rejects non-IAMF files, tracks without `iacb` or sample tables,
    /// malformed fragments, unrepresentable edit lists, and samples
    /// pointing outside the file.
    pub fn try_new(
        mut reader: MediaSourceStream<'s>,
        _opts: FormatOptions,
    ) -> SymphoniaResult<Self> {
        let file_len = reader
            .seek(SeekFrom::End(0))
            .map_err(SymphoniaError::from)?;
        reader
            .seek(SeekFrom::Start(0))
            .map_err(SymphoniaError::from)?;

        let mut ftyp: Option<Vec<u8>> = None;
        let mut moov: Option<BoxHeader> = None;
        let mut moofs: Vec<BoxHeader> = Vec::new();

        walk_children(&mut reader, file_len, 0, |r, child, _| {
            match &child.typ {
                b"ftyp" => {
                    ftyp = Some(child.read_payload(r)?);
                }
                b"moov" => moov = Some(*child),
                b"moof" => moofs.push(*child),
                _ => child.skip(r)?,
            }
            Ok(true)
        })
        .map_err(to_symphonia_error)?;

        let ftyp = ftyp.ok_or_else(|| to_symphonia_error(IamfMp4Error::NotIamf))?;
        check_ftyp(&ftyp).map_err(to_symphonia_error)?;
        let moov = moov.ok_or_else(|| to_symphonia_error(IamfMp4Error::MissingBox("moov")))?;

        // Walk moov for the movie timescale, tracks, and fragment
        // defaults.
        let moov_end = moov.end_offset.unwrap_or(file_len);
        reader
            .seek(SeekFrom::Start(moov.payload_offset))
            .map_err(SymphoniaError::from)?;
        let mut configs = Vec::new();
        let mut movie_timescale: Option<u32> = None;
        let mut track_defaults: HashMap<u32, TrackDefaults> = HashMap::new();
        let mut track_number = 0u32;
        walk_children(&mut reader, moov_end, 1, |r, child, _| {
            match &child.typ {
                b"mvhd" => {
                    movie_timescale = Some(parse_mvhd_timescale(&child.read_payload(r)?)?);
                }
                b"trak" => {
                    track_number += 1;
                    let config = parse_trak(r, child, track_number)?;
                    configs.push(config);
                }
                b"mvex" => {
                    track_defaults.extend(parse_mvex(r, child)?);
                }
                _ => child.skip(r)?,
            }
            Ok(true)
        })
        .map_err(to_symphonia_error)?;

        if configs.is_empty() {
            return Err(to_symphonia_error(IamfMp4Error::NoAudioTrack));
        }

        if !moofs.is_empty() {
            append_fragments(&mut reader, &moofs, &track_defaults, &mut configs, file_len)
                .map_err(to_symphonia_error)?;
            // Fragment runs carry no groups of their own; re-validate
            // roll against the merged tables.
            for config in &configs {
                validate_roll(&config.sample_table, &config.entries).map_err(to_symphonia_error)?;
            }
        }

        // Derive each track's presentation plan from its edit list. Only
        // tracks with edits need the movie timescale for conversion.
        for config in &mut configs {
            let movie_ts = if config.edits.is_empty() {
                config.timescale
            } else {
                movie_timescale
                    .ok_or_else(|| to_symphonia_error(IamfMp4Error::MissingBox("moov/mvhd")))?
            };
            config
                .finalize_edits(movie_ts)
                .map_err(to_symphonia_error)?;
        }

        // Validate sample bounds against the file length.
        for config in &configs {
            validate_sample_bounds(config, file_len).map_err(to_symphonia_error)?;
        }

        let mut tracks = Vec::with_capacity(configs.len());
        let mut cursors = Vec::with_capacity(configs.len());
        for config in configs {
            let sym_id = config.track_number;
            tracks.push(build_track(&config, sym_id, tracks.is_empty())?);
            cursors.push(PacketCursor::new(sym_id, config));
        }

        let media_info = MediaInfo::from_tracks(&tracks);
        Ok(Self {
            reader,
            tracks,
            cursors,
            cursor_index: 0,
            media_info,
            metadata: MetadataLog::default(),
        })
    }

    /// Per-track IAMF configuration (codec config, roll distance, ...).
    #[must_use]
    pub fn track_config(&self, track_id: u32) -> Option<&IamfTrackConfig> {
        self.cursors
            .iter()
            .find(|c| c.symphonia_id == track_id)
            .map(|c| &c.config)
    }
}

/// Reject sample tables that point outside the file.
fn validate_sample_bounds(config: &IamfTrackConfig, file_len: u64) -> Result<(), IamfMp4Error> {
    for run in &config.sample_table.runs {
        let mut end = run.offset;
        for &size in &run.sizes {
            end = end
                .checked_add(u64::from(size))
                .ok_or(IamfMp4Error::BadSampleTable("sample offset overflow"))?;
        }
        if end > file_len {
            return Err(IamfMp4Error::BadSampleTable("sample extends past EOF"));
        }
    }
    Ok(())
}

/// Build one Symphonia track from a parsed IAMF track.
fn build_track(config: &IamfTrackConfig, sym_id: u32, is_default: bool) -> SymphoniaResult<Track> {
    let codec = &config.codec_config;
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(symphonia_codec(codec)?)
        .with_sample_rate(codec.sample_rate)
        .with_max_frames_per_packet(u64::from(codec.num_samples_per_frame))
        .with_frames_per_block(u64::from(codec.num_samples_per_frame));
    if codec.bit_depth != 0 {
        params.with_bits_per_sample(u32::from(codec.bit_depth));
    }
    match codec.codec_id {
        CodecId::AacLc | CodecId::Flac if !codec.decoder_config.is_empty() => {
            params.with_extra_data(codec.decoder_config.clone().into_boxed_slice());
        }
        _ => {}
    }
    // Channel count stays unknown at the container level (it lives in
    // the audio element substream layout), so it is left unset.
    let time_base = TimeBase::try_from_recip(config.timescale)
        .ok_or(SymphoniaError::DecodeError("iamf: invalid track timescale"))?;
    let num_frames =
        u64::from(codec.num_samples_per_frame).saturating_mul(config.edit_plan.selected_samples);

    let mut track = Track::new(sym_id);
    track
        .with_codec_params(CodecParameters::Audio(params))
        .with_time_base(time_base)
        .with_duration(Duration::new(config.edit_plan.presentation_duration))
        .with_num_frames(num_frames)
        .with_start_ts(Timestamp::new(config.edit_plan.first_pts()));
    if is_default {
        track.with_flags(TrackFlags::DEFAULT);
    }
    Ok(track)
}

/// Map an IAMF codec id onto a Symphonia well-known audio codec id.
fn symphonia_codec(codec: &CodecConfig) -> SymphoniaResult<AudioCodecId> {
    match codec.codec_id {
        CodecId::Opus => Ok(CODEC_ID_OPUS),
        CodecId::AacLc => Ok(CODEC_ID_AAC),
        CodecId::Flac => Ok(CODEC_ID_FLAC),
        CodecId::Lpcm => match codec.bit_depth {
            16 => Ok(CODEC_ID_PCM_S16BE),
            24 => Ok(CODEC_ID_PCM_S24BE),
            32 => Ok(CODEC_ID_PCM_S32BE),
            other => Err(SymphoniaError::Unsupported(Box::leak(
                format!("iamf: lpcm bit depth {other}").into_boxed_str(),
            ))),
        },
    }
}

impl Scoreable for IamfFormatReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> SymphoniaResult<Score> {
        // Positioned at the `ftyp` marker: major(4) + minor(4), then the
        // compatible-brand list. Scan the first brands for `iamf`.
        let mut buf = [0u8; 40];
        let mut claimed = false;
        if src.read_buf_exact(&mut buf).is_ok() {
            for brand in buf.as_chunks::<4>().0 {
                if brand == b"iamf" {
                    claimed = true;
                    break;
                }
            }
        }
        Ok(if claimed {
            Score::Supported(100)
        } else {
            Score::Unsupported
        })
    }
}

impl ProbeableFormat<'_> for IamfFormatReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> SymphoniaResult<Box<dyn FormatReader + '_>> {
        Ok(Box::new(IamfFormatReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            FORMAT_INFO,
            &["mp4", "m4a"],
            &["audio/mp4", "audio/x-iamf"],
            &[b"ftyp"]
        )]
    }
}

impl FormatReader for IamfFormatReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    /// Only rewind-to-start is supported, mirroring the SACD reader:
    /// non-zero targets return `OutOfRange`.
    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> SymphoniaResult<SeekedTo> {
        let (track_id, required_ts) = match to {
            SeekTo::Time { time, track_id } => {
                if time != Time::ZERO {
                    return Err(seek_err(SeekErrorKind::OutOfRange));
                }
                (
                    track_id.unwrap_or_else(|| self.tracks.first().map_or(0, |t| t.id)),
                    Timestamp::ZERO,
                )
            }
            SeekTo::Timestamp { ts, track_id } => {
                if ts != Timestamp::ZERO {
                    return Err(seek_err(SeekErrorKind::OutOfRange));
                }
                (track_id, ts)
            }
        };
        if !self.tracks.iter().any(|t| t.id == track_id) {
            return Err(seek_err(SeekErrorKind::InvalidTrack));
        }
        self.cursor_index = 0;
        for cursor in &mut self.cursors {
            cursor.rewind();
        }
        Ok(SeekedTo {
            track_id,
            required_ts,
            actual_ts: Timestamp::ZERO,
        })
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> SymphoniaResult<Option<Packet>> {
        loop {
            let Some(cursor) = self.cursors.get_mut(self.cursor_index) else {
                return Ok(None);
            };
            let Some((offset, size, ts, dur)) = cursor.take_sample() else {
                self.cursor_index += 1;
                continue;
            };
            if u64::from(size) > MAX_SAMPLE_BYTES {
                return Err(to_symphonia_error(IamfMp4Error::BoxTooLarge(u64::from(
                    size,
                ))));
            }
            self.reader
                .seek(SeekFrom::Start(offset))
                .map_err(SymphoniaError::from)?;
            let len = usize::try_from(size)
                .ok()
                .ok_or_else(|| to_symphonia_error(IamfMp4Error::BoxTooLarge(u64::from(size))))?;
            let mut data = vec![0u8; len];
            self.reader
                .read_exact(&mut data)
                .map_err(SymphoniaError::from)?;
            return Ok(Some(Packet::new(
                cursor.symphonia_id,
                Timestamp::new(ts),
                Duration::new(dur),
                data.into_boxed_slice(),
            )));
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}
