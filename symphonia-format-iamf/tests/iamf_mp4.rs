//! Integration tests for the IAMF ISO-BMFF demuxer.
//!
//! Fixtures are valid BMFF files assembled in memory: hand-encoded `iacb`
//! descriptor OBUs (validated against `sotf-iamf` itself), a standard
//! sample table, and opaque frame payloads. The corpus test wraps the real
//! `sotf-iamf` `.iamf` fixtures and checks demuxed packets byte-for-byte.

use std::io::Cursor;

use sotf_iamf::obu::parse_descriptors;
use sotf_iamf::obu::parser::parse_temporal_unit_with_kinds;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_FLAC, CODEC_ID_OPUS, CODEC_ID_PCM_S16BE,
};
use symphonia_core::errors::{Error as SymphoniaError, SeekErrorKind};
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::{Time, Timestamp};

use symphonia_format_iamf::{
    IamfFormatReader, reassemble_ia_sequence, register_all, register_decoders,
    track_descriptor_obus,
};

// ---------------------------------------------------------------------------
// BMFF writers
// ---------------------------------------------------------------------------

fn bx(typ: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    let size = u32::try_from(8 + payload.len()).expect("fixture box fits in u32");
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(&typ);
    out.extend_from_slice(payload);
    out
}

fn bx_large(typ: [u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + payload.len());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&typ);
    let size = u64::try_from(16 + payload.len()).expect("fixture box fits in u64");
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn full(typ: [u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let flag_bytes = |shift: u32| u8::try_from((flags >> shift) & 0xFF).expect("24-bit flags");
    let mut inner = vec![version, flag_bytes(16), flag_bytes(8), flag_bytes(0)];
    inner.extend_from_slice(payload);
    bx(typ, &inner)
}

fn leb(mut v: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut b = u8::try_from(v & 0x7F).expect("7 bits fit in u8");
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
    out
}

/// Frame one OBU: `type << 3`, no trimming/extension flags.
fn obu(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![ty << 3];
    out.extend(leb(
        u32::try_from(payload.len()).expect("fixture OBU fits in u32")
    ));
    out.extend_from_slice(payload);
    out
}

fn mix_gain_config(parameter_id: u32, default_db_q78: i16) -> Vec<u8> {
    let mut out = leb(parameter_id);
    out.extend(leb(48000)); // parameter_rate
    out.push(0x80); // param_definition_mode = 1 (no durations)
    out.extend_from_slice(&default_db_q78.to_be_bytes());
    out
}

fn sub_mix(element_gain_q78: i16) -> Vec<u8> {
    let mut sm = leb(1); // num_audio_elements
    sm.extend(leb(0)); // audio_element_id
    sm.push(0x00); // headphones rendering mode
    sm.extend(leb(0)); // rendering extension size
    sm.extend(mix_gain_config(10, element_gain_q78));
    sm.extend(mix_gain_config(11, 0));
    sm.extend(leb(1)); // num_layouts
    sm.push(0x80); // type 2 (loudspeakers) + sound system 0 (stereo)
    sm.extend_from_slice(&[0x00, 0xE9, 0x00, 0xFF, 0x00]); // loudness
    sm
}

/// Descriptor OBU section: sequence header + one Opus codec config +
/// one single-layer channel element + one stereo mix presentation.
fn opus_obu_section(codec_config_id: u32) -> Vec<u8> {
    let mut stream = Vec::new();
    stream.extend(obu(31, &[b'i', b'a', b'm', b'f', 0, 0]));
    let mut cc = leb(codec_config_id);
    cc.extend_from_slice(b"Opus");
    cc.extend(leb(960)); // num_samples_per_frame
    cc.extend_from_slice(&0i16.to_be_bytes()); // audio_roll_distance
    // Opus decoder config: version, channels, pre_skip, sample_rate,
    // output_gain, mapping_family.
    cc.extend_from_slice(&[1, 2, 0x38, 0x01, 0x00, 0x00, 0xBB, 0x80, 0, 0, 0]);
    stream.extend(obu(0, &cc));
    let mut ae = leb(0); // audio_element_id
    ae.push(0x00); // element_type = channel
    ae.extend(leb(codec_config_id));
    ae.extend(leb(1)); // num_substreams
    ae.extend(leb(0)); // substream id 0
    ae.extend(leb(0)); // num_parameters
    ae.push(0x20); // num_layers = 1
    ae.extend_from_slice(&[0x10, 0x01, 0x01]); // stereo layer, 1 substream
    stream.extend(obu(1, &ae));
    let mut mp = leb(0); // mix_presentation_id
    mp.extend(leb(0)); // count_label
    mp.extend(leb(1)); // num_sub_mixes
    mp.extend(sub_mix(0));
    stream.extend(obu(2, &mp));
    stream
}

// ---------------------------------------------------------------------------
// File assembler
// ---------------------------------------------------------------------------

/// Options for the single-track fixture builder.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools, reason = "test-only fixture knobs")]
struct FileOpts {
    timescale: u32,
    deltas: Vec<u32>, // stts deltas, one per sample
    use_co64: bool,
    use_stz2: bool,
    elst_media_time: Option<i64>,
    iacb_version: u8,
    entry_cc_id: u32,
    stsd_entries: u32,
    with_sdtp: bool,
    ftyp_brands: Vec<[u8; 4]>,
    mdat_large: bool,
    stco_offset: Option<u64>, // override chunk offset (for truncation tests)
}

impl Default for FileOpts {
    fn default() -> Self {
        Self {
            timescale: 48000,
            deltas: Vec::new(),
            use_co64: false,
            use_stz2: false,
            elst_media_time: None,
            iacb_version: 0,
            entry_cc_id: 0,
            stsd_entries: 1,
            with_sdtp: false,
            ftyp_brands: vec![*b"isom", *b"iamf"],
            mdat_large: false,
            stco_offset: None,
        }
    }
}

/// Assemble a complete single-track file.
fn build_file(frames: &[Vec<u8>], obu_section: &[u8], opts: &FileOpts) -> Vec<u8> {
    let deltas: Vec<u32> = if opts.deltas.is_empty() {
        vec![960; frames.len()]
    } else {
        opts.deltas.clone()
    };
    assert_eq!(deltas.len(), frames.len());

    let trak = trak_box(&deltas, frames, obu_section, opts);
    let duration: u32 = deltas.iter().sum();
    let movie_header = header_box_payload(opts.timescale, duration);
    let mut movie_bytes = Vec::new();
    movie_bytes.extend(full(*b"mvhd", 0, 0, &movie_header));
    movie_bytes.extend(trak);
    let moov = bx(*b"moov", &movie_bytes);

    // ftyp.
    let mut file_type = b"isom".to_vec();
    file_type.extend(0u32.to_be_bytes());
    for brand in &opts.ftyp_brands {
        file_type.extend_from_slice(brand);
    }
    let ftyp = bx(*b"ftyp", &file_type);

    // mdat.
    let mut media_data = Vec::new();
    for f in frames {
        media_data.extend_from_slice(f);
    }
    let mdat = if opts.mdat_large {
        bx_large(*b"mdat", &media_data)
    } else {
        bx(*b"mdat", &media_data)
    };

    // Layout: patch chunk offset.
    let mdat_header = if opts.mdat_large { 16 } else { 8 };
    let data_offset =
        u64::try_from(ftyp.len() + moov.len() + mdat_header).expect("fixture fits in u64");
    let chunk_offset = opts.stco_offset.unwrap_or(data_offset);
    let offset_size = if opts.use_co64 { 8 } else { 4 };
    let mut file = Vec::new();
    file.extend(ftyp);
    // moov contains exactly one chunk box; patch its 4/8-byte offset.
    file.extend(patch_chunk_offset(&moov, chunk_offset, offset_size));
    file.extend(mdat);
    file
}

/// Version-0 `mvhd`/`mdhd`-style payload: creation + modification +
/// timescale + duration.
fn header_box_payload(timescale: u32, duration: u32) -> Vec<u8> {
    let mut p = 0u32.to_be_bytes().to_vec(); // creation
    p.extend(0u32.to_be_bytes()); // modification
    p.extend(timescale.to_be_bytes());
    p.extend(duration.to_be_bytes());
    p
}

/// One `trak` box: `tkhd`, optional `edts`, and `mdia`.
fn trak_box(deltas: &[u32], frames: &[Vec<u8>], obu_section: &[u8], opts: &FileOpts) -> Vec<u8> {
    let duration: u32 = deltas.iter().sum();
    // tkhd v0: creation + modification + track_ID + reserved + duration.
    let mut track_header = 0u32.to_be_bytes().to_vec(); // creation
    track_header.extend(0u32.to_be_bytes()); // modification
    track_header.extend(1u32.to_be_bytes()); // track id
    track_header.extend(0u32.to_be_bytes()); // reserved
    track_header.extend(duration.to_be_bytes());
    let tkhd = full(*b"tkhd", 0, 3, &track_header);

    let mut track_bytes = Vec::new();
    track_bytes.extend(tkhd);
    if let Some(media_time) = opts.elst_media_time {
        let mut edit_list = 1u32.to_be_bytes().to_vec(); // entry_count
        edit_list.extend(1024u32.to_be_bytes()); // segment_duration
        let media_u32 = u32::try_from(media_time).expect("fixture media time fits in u32");
        edit_list.extend(media_u32.to_be_bytes()); // media_time
        edit_list.extend(0x0001_0000u32.to_be_bytes()); // media_rate
        track_bytes.extend(bx(*b"edts", &full(*b"elst", 0, 0, &edit_list)));
    }
    track_bytes.extend(mdia_box(deltas, frames, obu_section, opts));
    bx(*b"trak", &track_bytes)
}

/// One `mdia` box: `mdhd`, `hdlr`, and `minf`.
fn mdia_box(deltas: &[u32], frames: &[Vec<u8>], obu_section: &[u8], opts: &FileOpts) -> Vec<u8> {
    let duration: u32 = deltas.iter().sum();
    let mut media_header = header_box_payload(opts.timescale, duration);
    media_header.extend([0x55, 0xC4, 0, 0]); // language + quality
    let mdhd = full(*b"mdhd", 0, 0, &media_header);
    let mut handler_bytes = 0u32.to_be_bytes().to_vec(); // pre_defined
    handler_bytes.extend_from_slice(b"soun");
    handler_bytes.extend([0u8; 12]); // reserved
    handler_bytes.extend(b"SoundHandler\0");
    let hdlr = full(*b"hdlr", 0, 0, &handler_bytes);
    let mut media_bytes = Vec::new();
    media_bytes.extend(mdhd);
    media_bytes.extend(hdlr);
    media_bytes.extend(minf_box(deltas, frames, obu_section, opts));
    bx(*b"mdia", &media_bytes)
}

/// One `minf` box: `smhd`, `dinf`, `stbl`, `iacb`, plus a stray `free`.
fn minf_box(deltas: &[u32], frames: &[Vec<u8>], obu_section: &[u8], opts: &FileOpts) -> Vec<u8> {
    let smhd = full(*b"smhd", 0, 0, &[0, 0, 0, 0]);
    let url_box = full(*b"url ", 0, 1, &[]);
    let mut data_ref = 0u32.to_be_bytes().to_vec();
    data_ref.extend(1u32.to_be_bytes());
    data_ref.extend(url_box);
    let dinf = bx(*b"dinf", &full(*b"dref", 0, 0, &data_ref));
    // iacb: FullBox + OBU section. The box ends at the last descriptor
    // OBU; the referenced codec config comes from the sample entry.
    let iacb = full(*b"iacb", opts.iacb_version, 0, obu_section);
    let mut movie_media = Vec::new();
    movie_media.extend(smhd);
    movie_media.extend(dinf);
    movie_media.extend(stbl_box(deltas, frames, opts));
    movie_media.extend(iacb);
    // A stray free box exercises the unknown-box skip path.
    movie_media.extend(bx(*b"free", &[0xDE, 0xAD]));
    bx(*b"minf", &movie_media)
}

/// One `stbl` box for a single chunk holding every sample.
fn stbl_box(deltas: &[u32], frames: &[Vec<u8>], opts: &FileOpts) -> Vec<u8> {
    let frame_count = u32::try_from(frames.len()).expect("fixture fits in u32");
    // stsd: entry_count + iamf entries (8 reserved + leb cc id).
    let mut desc_entries = opts.stsd_entries.to_be_bytes().to_vec();
    for _ in 0..opts.stsd_entries {
        let mut entry = vec![0u8; 8];
        entry.extend(leb(opts.entry_cc_id));
        desc_entries.extend(bx(*b"iamf", &entry));
    }
    let entry_table = full(*b"stsd", 0, 0, &desc_entries);

    // stts: single row.
    let mut time_rows = 1u32.to_be_bytes().to_vec();
    time_rows.extend(frame_count.to_be_bytes());
    time_rows.extend(deltas.first().copied().unwrap_or(960).to_be_bytes());
    let timing_table = full(*b"stts", 0, 0, &time_rows);

    // stsc: one row (first_chunk=1, samples_per_chunk=N).
    let mut chunk_rows = 1u32.to_be_bytes().to_vec();
    chunk_rows.extend(1u32.to_be_bytes());
    chunk_rows.extend(frame_count.to_be_bytes());
    chunk_rows.extend(1u32.to_be_bytes());
    let chunk_table = full(*b"stsc", 0, 0, &chunk_rows);

    // stsz / stz2.
    let sample_sizes = if opts.use_stz2 {
        let mut inner = vec![0, 0, 0, 8]; // reserved + field_size 8
        inner.extend(frame_count.to_be_bytes());
        for f in frames {
            inner.push(u8::try_from(f.len()).expect("fixture frame fits in u8"));
        }
        full(*b"stz2", 0, 0, &inner)
    } else {
        let mut inner = 0u32.to_be_bytes().to_vec(); // sample_size 0
        inner.extend(frame_count.to_be_bytes());
        for f in frames {
            inner.extend(
                u32::try_from(f.len())
                    .expect("fixture frame fits in u32")
                    .to_be_bytes(),
            );
        }
        full(*b"stsz", 0, 0, &inner)
    };

    // stco / co64 with placeholder, patched after layout.
    let offset_box = if opts.use_co64 {
        full(*b"co64", 0, 0, &[0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0])
    } else {
        full(*b"stco", 0, 0, &[0, 0, 0, 1, 0, 0, 0, 0])
    };

    let mut table_bytes = Vec::new();
    table_bytes.extend(entry_table);
    table_bytes.extend(timing_table);
    table_bytes.extend(chunk_table);
    table_bytes.extend(sample_sizes);
    if opts.with_sdtp {
        let sdtp_payload = vec![0u8; frames.len()];
        table_bytes.extend(full(*b"sdtp", 0, 0, &sdtp_payload));
    }
    table_bytes.extend(offset_box);
    bx(*b"stbl", &table_bytes)
}

/// Rewrite the single chunk offset inside `moov`.
fn patch_chunk_offset(moov: &[u8], chunk_offset: u64, offset_size: usize) -> Vec<u8> {
    let mut out = moov.to_vec();
    // Find the stco/co64 payload: search for the box by walking sizes.
    let pos = find_box(&out, 8, *b"trak");
    let pos = find_box(&out, pos, *b"mdia");
    let pos = find_box(&out, pos, *b"minf");
    let pos = find_box(&out, pos, *b"stbl");
    let (is_co64, pos) = match find_box_opt(&out, pos, *b"co64") {
        Some(p) => (true, p),
        None => (false, find_box(&out, pos, *b"stco")),
    };
    assert_eq!(is_co64, offset_size == 8);
    // Payload: version/flags(4) + entry_count(4) + offsets.
    let field = pos + 8;
    if is_co64 {
        out[field..field + 8].copy_from_slice(&chunk_offset.to_be_bytes());
    } else {
        let offset_u32 = u32::try_from(chunk_offset).expect("fixture fits in u32");
        out[field..field + 4].copy_from_slice(&offset_u32.to_be_bytes());
    }
    out
}

/// Payload offset of the first `needle` child when scanning from `from`.
fn find_box(buf: &[u8], from: usize, needle: [u8; 4]) -> usize {
    find_box_opt(buf, from, needle).expect("box must exist")
}

fn find_box_opt(buf: &[u8], mut from: usize, needle: [u8; 4]) -> Option<usize> {
    // `from` points at the parent payload start.
    loop {
        if from + 8 > buf.len() {
            return None;
        }
        let size_bytes: [u8; 4] = buf[from..from + 4]
            .try_into()
            .expect("bounds checked above");
        let size = usize::try_from(u32::from_be_bytes(size_bytes)).expect("box size fits");
        if size < 8 || from + size > buf.len() {
            return None;
        }
        if buf[from + 4..from + 8] == needle {
            return Some(from + 8);
        }
        from += size;
    }
}

// ---------------------------------------------------------------------------
// Reader plumbing
// ---------------------------------------------------------------------------

fn open_reader(file: Vec<u8>) -> IamfFormatReader<'static> {
    let cursor = Cursor::new(file);
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());
    IamfFormatReader::try_new(mss, FormatOptions::default()).expect("parse fixture")
}

fn probe_file(file: Vec<u8>) -> Box<dyn FormatReader> {
    let mut probe = Probe::new();
    register_all(&mut probe);
    let mut hint = Hint::new();
    hint.with_extension("mp4");
    let cursor = Cursor::new(file);
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());
    probe
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .expect("probe fixture")
}

fn opus_frames() -> Vec<Vec<u8>> {
    vec![vec![0x11; 64], vec![0x22; 128], vec![0x33; 96]]
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[test]
fn demux_opus_track_packets() {
    let frames = opus_frames();
    let file = build_file(&frames, &opus_obu_section(0), &FileOpts::default());
    let mut reader = open_reader(file);

    assert_eq!(reader.tracks().len(), 1);
    let track = &reader.tracks()[0];
    assert_eq!(track.id, 1);
    match track.codec_params.as_ref().expect("codec params") {
        CodecParameters::Audio(p) => {
            assert_eq!(p.codec, CODEC_ID_OPUS);
            assert_eq!(p.sample_rate, Some(48000));
        }
        other => panic!("expected audio params, got {other:?}"),
    }

    for (i, expected) in frames.iter().enumerate() {
        let packet = reader.next_packet().expect("packet").expect("not eof");
        assert_eq!(packet.track_id, 1);
        assert_eq!(
            packet.pts,
            Timestamp::new(i64::try_from(i).expect("small index") * 960)
        );
        assert_eq!(packet.dur, symphonia_core::units::Duration::new(960));
        assert_eq!(packet.data.as_ref(), expected.as_slice());
    }
    assert!(reader.next_packet().expect("eof").is_none());
}

#[test]
fn probe_registers_and_scores() {
    let file = build_file(&opus_frames(), &opus_obu_section(0), &FileOpts::default());
    register_decoders(&mut symphonia_core::codecs::registry::CodecRegistry::new());
    let reader = probe_file(file);
    assert_eq!(reader.tracks().len(), 1);
    assert_eq!(reader.format_info().short_name, "iamf");
}

#[test]
fn seek_rewinds_to_start() {
    let frames = opus_frames();
    let file = build_file(&frames, &opus_obu_section(0), &FileOpts::default());
    let mut reader = open_reader(file);
    let first = reader.next_packet().unwrap().unwrap();
    assert_eq!(first.data.as_ref(), frames[0].as_slice());
    // Non-zero targets are out of range.
    let err = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::new(960),
                track_id: 1,
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        SymphoniaError::SeekError(SeekErrorKind::OutOfRange)
    ));
    let err = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time: Time::from_nanos(1_000_000_000),
                track_id: None,
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        SymphoniaError::SeekError(SeekErrorKind::OutOfRange)
    ));
    // Unknown track is invalid even at zero.
    let err = reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::ZERO,
                track_id: 99,
            },
        )
        .unwrap_err();
    assert!(matches!(
        err,
        SymphoniaError::SeekError(SeekErrorKind::InvalidTrack)
    ));
    // Rewind replays from the first packet.
    reader
        .seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: Timestamp::ZERO,
                track_id: 1,
            },
        )
        .expect("rewind");
    let replay = reader.next_packet().unwrap().unwrap();
    assert_eq!(replay.data.as_ref(), frames[0].as_slice());
    assert_eq!(replay.pts, Timestamp::ZERO);
}

#[test]
fn elst_media_time_offsets_timestamps() {
    let frames = opus_frames();
    let opts = FileOpts {
        elst_media_time: Some(480),
        ..FileOpts::default()
    };
    let file = build_file(&frames, &opus_obu_section(0), &opts);
    let mut reader = open_reader(file);
    assert_eq!(reader.tracks()[0].start_ts, Timestamp::new(480));
    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.pts, Timestamp::new(480));
    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.pts, Timestamp::new(1440));
}

#[test]
fn co64_and_stz2_variant_demuxes() {
    let frames = opus_frames();
    let opts = FileOpts {
        use_co64: true,
        use_stz2: true,
        ..FileOpts::default()
    };
    let file = build_file(&frames, &opus_obu_section(0), &opts);
    let mut reader = open_reader(file);
    let mut count = 0;
    while reader.next_packet().unwrap().is_some() {
        count += 1;
    }
    assert_eq!(count, 3);
}

#[test]
fn largesize_mdat_demuxes() {
    let frames = opus_frames();
    let opts = FileOpts {
        mdat_large: true,
        ..FileOpts::default()
    };
    let file = build_file(&frames, &opus_obu_section(0), &opts);
    let mut reader = open_reader(file);
    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.data.as_ref(), frames[0].as_slice());
}

#[test]
fn track_config_exposes_roll_distance() {
    let file = build_file(&opus_frames(), &opus_obu_section(0), &FileOpts::default());
    let reader = open_reader(file);
    let config = reader.track_config(1).expect("track config");
    assert_eq!(config.codec_config.audio_roll_distance, 0);
    assert_eq!(config.codec_config.num_samples_per_frame, 960);
    assert!(reader.track_config(99).is_none());
}

// ---------------------------------------------------------------------------
// Codec mapping
// ---------------------------------------------------------------------------

#[test]
fn codec_ids_map_to_well_known() {
    // (fourcc tag, decoder_config bytes, expected codec, extra_data?)
    let cases: &[(
        [u8; 4],
        Vec<u8>,
        symphonia_core::codecs::audio::AudioCodecId,
        bool,
    )] = &[
        (
            *b"Opus",
            vec![1, 2, 0x38, 0x01, 0, 0, 0xBB, 0x80, 0, 0, 0],
            CODEC_ID_OPUS,
            false,
        ),
        (*b"mp4a", vec![0x12, 0x10], CODEC_ID_AAC, true),
        (*b"fLaC", vec![0x10, 0, 0x22, 0, 0], CODEC_ID_FLAC, true),
        (
            *b"ipcm",
            vec![0, 16, 0, 0, 0xBB, 0x80],
            CODEC_ID_PCM_S16BE,
            false,
        ),
    ];
    for case in cases {
        let (tag, decoder_config, expected, want_extra) = case;
        let section = custom_cc_section(*tag, decoder_config, 0);
        let file = build_file(&opus_frames(), &section, &FileOpts::default());
        let reader = open_reader(file);
        match reader.tracks()[0].codec_params.as_ref().expect("params") {
            CodecParameters::Audio(p) => {
                assert_eq!(
                    p.codec,
                    *expected,
                    "tag {:?}",
                    String::from_utf8_lossy(&tag[..])
                );
                assert_eq!(p.extra_data.is_some(), *want_extra);
            }
            other => panic!("expected audio params, got {other:?}"),
        }
    }
}

/// Descriptor section with a custom codec-config body (AAC/FLAC/LPCM shapes).
fn custom_cc_section(tag: [u8; 4], decoder_config: &[u8], cc_id: u32) -> Vec<u8> {
    let mut stream = Vec::new();
    stream.extend(obu(31, &[b'i', b'a', b'm', b'f', 0, 0]));
    let mut cc = leb(cc_id);
    cc.extend_from_slice(&tag);
    cc.extend(leb(1024));
    cc.extend_from_slice(&0i16.to_be_bytes());
    cc.extend_from_slice(decoder_config);
    stream.extend(obu(0, &cc));
    stream
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

#[test]
fn reassemble_single_track_roundtrips() {
    let section = opus_obu_section(0);
    let file = build_file(&opus_frames(), &section, &FileOpts::default());
    let reader = open_reader(file);
    let config = reader.track_config(1).expect("config").clone();
    assert_eq!(track_descriptor_obus(&config), section.as_slice());
    let rebuilt = reassemble_ia_sequence(std::slice::from_ref(&config)).expect("reassemble");
    assert_eq!(rebuilt, section);
    let (desc, _) = parse_descriptors(&rebuilt).expect("rebuilt parses");
    assert_eq!(desc.codec_configs.len(), 1);
    assert_eq!(desc.audio_elements.len(), 1);
    assert_eq!(desc.mix_presentations.len(), 1);
}

#[test]
fn reassemble_two_tracks_dedupes_sequence_header() {
    // Track 2 carries the same sequence header plus a second codec config.
    let section1 = opus_obu_section(0);
    let (header, header_size) =
        sotf_iamf::obu::parser::parse_obu_header(&section1).expect("seqhdr");
    let seq_total = header_size + header.payload_size;
    let mut section2 = section1[..seq_total].to_vec();
    let mut cc = leb(7);
    cc.extend_from_slice(b"Opus");
    cc.extend(leb(960));
    cc.extend_from_slice(&0i16.to_be_bytes());
    cc.extend_from_slice(&[1, 2, 0x38, 0x01, 0, 0, 0xBB, 0x80, 0, 0, 0]);
    section2.extend(obu(0, &cc));

    // Parse both files and reassemble across readers (each file holds one
    // track; the bridge only needs the track configs). Track 2 selects
    // its second codec config (id 7) through its sample entry.
    let file_a = build_file(&opus_frames(), &section1, &FileOpts::default());
    let opts_b = FileOpts {
        entry_cc_id: 7,
        ..FileOpts::default()
    };
    let file_b = build_file(&opus_frames(), &section2, &opts_b);
    let ra = open_reader(file_a.clone());
    let rb = open_reader(file_b);
    let ta = ra.track_config(1).expect("a").clone();
    let tb = rb.track_config(1).expect("b").clone();
    assert_eq!(tb.codec_config.codec_config_id, 7);
    let rebuilt = reassemble_ia_sequence(&[ta, tb]).expect("reassemble");
    // One sequence header + track1's 3 OBUs + track2's extra codec config.
    let (desc, _) = parse_descriptors(&rebuilt).expect("rebuilt parses");
    assert_eq!(desc.codec_configs.len(), 2);
    assert_eq!(desc.audio_elements.len(), 1);
    assert_eq!(desc.mix_presentations.len(), 1);
    // Mismatched sequence headers are rejected: flip a profile byte (still
    // valid OBUs, but the raw bytes differ across tracks).
    let mut bad = section2.clone();
    bad[seq_total - 1] ^= 0xFF;
    let file_bad = build_file(&opus_frames(), &bad, &opts_b);
    let rb2 = open_reader(file_bad);
    let tb2 = rb2.track_config(1).expect("b2").clone();
    let ta2 = open_reader(file_a).track_config(1).expect("a2").clone();
    reassemble_ia_sequence(&[ta2, tb2]).unwrap_err();
}

// ---------------------------------------------------------------------------
// Malformed inputs
// ---------------------------------------------------------------------------

fn open_expect_error(file: Vec<u8>) -> SymphoniaError {
    let cursor = Cursor::new(file);
    let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());
    IamfFormatReader::try_new(mss, FormatOptions::default()).expect_err("must fail")
}

#[test]
fn rejects_non_iamf_ftyp() {
    let opts = FileOpts {
        ftyp_brands: vec![*b"isom"],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &opus_obu_section(0), &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn rejects_fragmented_moof() {
    let mut file = build_file(&opus_frames(), &opus_obu_section(0), &FileOpts::default());
    // Append a minimal moof after mdat.
    file.extend(bx(*b"moof", &bx(*b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1])));
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn rejects_sample_past_eof() {
    let opts = FileOpts {
        stco_offset: Some(0x00FF_FFFF),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &opus_obu_section(0), &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_sdtp_dependencies() {
    let opts = FileOpts {
        with_sdtp: true,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &opus_obu_section(0), &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn rejects_iacb_version_2() {
    let opts = FileOpts {
        iacb_version: 2,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &opus_obu_section(0), &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn rejects_two_sample_entries() {
    let opts = FileOpts {
        stsd_entries: 2,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &opus_obu_section(0), &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_garbage() {
    assert!(matches!(
        open_expect_error(vec![0u8; 64]),
        SymphoniaError::Unsupported(_) | SymphoniaError::DecodeError(_)
    ));
    assert!(matches!(
        open_expect_error(b"ftypXXXX not a box at all........".to_vec()),
        SymphoniaError::Unsupported(_) | SymphoniaError::DecodeError(_)
    ));
}

// ---------------------------------------------------------------------------
// Corpus end-to-end
// ---------------------------------------------------------------------------

/// Wrap each real `sotf-iamf` corpus stream in BMFF and demux it.
///
/// The descriptor prefix becomes the `iacb` section verbatim, so this
/// exercises real-world descriptor sections (5.1 layers, demixing
/// parameters, the 3OA scene element) through `parse_trak` and the
/// bridge. Files whose temporal units split cleanly with the public
/// parser (FLAC, PCM) additionally get byte-exact packet comparisons;
/// the Opus references interleave redundant descriptor copies that the
/// current TU splitter rejects (upstream only descriptor-parses them
/// too), so they run with zero samples.
#[test]
fn corpus_streams_demux() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../sotf-daw/crates/sotf-iamf/tests/data");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .expect("sotf-iamf corpus present")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "iamf"))
        .collect();
    names.sort();
    assert!(!names.is_empty(), "expected .iamf corpus files in {dir:?}");

    // Files with fully splittable temporal units get packet comparisons.
    let byte_exact = [
        "noise_1024samp_stereo_flac.iamf",
        "tones_256samp_5p1_pcm.iamf",
    ];
    for path in names {
        let raw = std::fs::read(&path).expect("read corpus");
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        roundtrip_corpus(&name, &raw, byte_exact.contains(&name.as_str()));
    }
}

fn roundtrip_corpus(name: &str, raw: &[u8], byte_exact: bool) {
    let (desc, temporal_offset) = parse_descriptors(raw).expect("corpus parses");
    assert!(!desc.codec_configs.is_empty(), "{name}: no codec configs");

    // Descriptor prefix becomes the iacb OBU section verbatim.
    let obu_section = &raw[..temporal_offset];
    let entry_cc_id = desc.codec_configs[0].codec_config_id;
    let delta = desc.codec_configs[0].num_samples_per_frame.max(1);

    // Split temporal units into per-frame samples in file order.
    let kinds = desc.parameter_kinds();
    let recon = desc.recon_layouts();
    let mut samples = Vec::new();
    let mut pos = temporal_offset;
    let mut split_ok = true;
    while pos < raw.len() {
        match parse_temporal_unit_with_kinds(&raw[pos..], &kinds, &recon) {
            Ok((_, 0)) => {
                eprintln!("{name}: zero-length temporal unit at {pos}");
                split_ok = false;
                break;
            }
            Ok((unit, consumed)) => {
                for frame in &unit.audio_frames {
                    samples.push(frame.payload.clone());
                }
                pos += consumed;
            }
            Err(e) => {
                eprintln!("{name}: TU split stops at {pos}: {e}");
                split_ok = false;
                break;
            }
        }
    }
    if byte_exact {
        assert!(split_ok, "{name}: expected clean TU split");
        assert!(!samples.is_empty(), "{name}: no audio frames");
    } else {
        // Opus references: demux the descriptor section with no samples.
        samples.clear();
    }

    // A zero-sample track still validates: one empty chunk.
    let frames: Vec<Vec<u8>> = if samples.is_empty() {
        vec![vec![]]
    } else {
        samples.clone()
    };
    let opts = FileOpts {
        deltas: vec![delta; frames.len()],
        entry_cc_id,
        ..FileOpts::default()
    };
    let file = build_file(&frames, obu_section, &opts);
    let mut reader = open_reader(file);

    // Track parameters must reflect the real codec config.
    let codec = reader.track_config(1).expect("config").codec_config.clone();
    assert_eq!(codec.codec_config_id, entry_cc_id, "{name}");
    assert_eq!(
        codec.sample_rate, desc.codec_configs[0].sample_rate,
        "{name}"
    );

    for (i, expected) in frames.iter().enumerate() {
        let packet = reader
            .next_packet()
            .unwrap_or_else(|e| panic!("{name} packet {i}: {e}"))
            .unwrap_or_else(|| panic!("{name} eof at packet {i}"));
        assert_eq!(
            packet.data.as_ref(),
            expected.as_slice(),
            "{name} packet {i}"
        );
        assert_eq!(
            packet.pts,
            Timestamp::new(i64::try_from(i).expect("small index") * i64::from(delta)),
            "{name} ts {i}"
        );
    }
    assert!(reader.next_packet().expect("eof").is_none());

    // The bridge must reproduce a descriptor section sotf-iamf accepts.
    let config = reader.track_config(1).expect("config").clone();
    let rebuilt = reassemble_ia_sequence(std::slice::from_ref(&config)).expect("reassemble");
    let (redesc, _) = parse_descriptors(&rebuilt).expect("rebuilt parses");
    assert_eq!(
        redesc.codec_configs.len(),
        desc.codec_configs.len(),
        "{name}"
    );
    assert_eq!(
        redesc.audio_elements.len(),
        desc.audio_elements.len(),
        "{name}"
    );
    assert_eq!(
        redesc.mix_presentations.len(),
        desc.mix_presentations.len(),
        "{name}"
    );
}
