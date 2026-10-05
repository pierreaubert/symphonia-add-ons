//! Integration tests for the IAMF ISO-BMFF demuxer.
//!
//! Fixtures are valid BMFF files assembled in memory: hand-encoded `iacb`
//! descriptor OBUs (validated against `sotf-iamf` itself), a standard
//! sample table, and opaque frame payloads. The corpus test wraps the real
//! `sotf-iamf` `.iamf` fixtures and checks demuxed packets byte-for-byte.

use std::io::Cursor;

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_FLAC, CODEC_ID_OPUS, CODEC_ID_PCM_S16BE,
};
use symphonia_core::errors::{Error as SymphoniaError, SeekErrorKind};
use symphonia_core::formats::probe::{Hint, Probe};
use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia_core::meta::MetadataOptions;
use symphonia_core::units::{Duration, Time, Timestamp};
use symphonia_iamf_core::obu::parse_descriptors;
use symphonia_iamf_core::obu::parser::parse_temporal_unit_with_kinds;

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
    opus_obu_section_with_roll(codec_config_id, 0)
}

/// [`opus_obu_section`] with an explicit `audio_roll_distance`.
fn opus_obu_section_with_roll(codec_config_id: u32, roll: i16) -> Vec<u8> {
    opus_obu_section_full(codec_config_id, roll, 0, 0)
}

/// [`opus_obu_section`] with explicit sequence-header profiles.
fn opus_obu_section_full(
    codec_config_id: u32,
    roll: i16,
    primary_profile: u8,
    additional_profile: u8,
) -> Vec<u8> {
    let mut stream = Vec::new();
    stream.extend(obu(
        31,
        &[b'i', b'a', b'm', b'f', primary_profile, additional_profile],
    ));
    let mut cc = leb(codec_config_id);
    cc.extend_from_slice(b"Opus");
    cc.extend(leb(960)); // num_samples_per_frame
    cc.extend_from_slice(&roll.to_be_bytes()); // audio_roll_distance
    // Opus decoder config: version, channels, pre_skip, sample_rate,
    // output_gain, mapping_family.
    cc.extend_from_slice(&[1, 2, 0x38, 0x01, 0x00, 0x00, 0xBB, 0x80, 0, 0, 0]);
    stream.extend(obu(0, &cc));
    stream.extend(obu(1, &channel_element_obu(codec_config_id)));
    stream.extend(obu(2, &mix_presentation_obu()));
    stream
}

/// One single-layer stereo channel element OBU body.
fn channel_element_obu(codec_config_id: u32) -> Vec<u8> {
    let mut ae = leb(0); // audio_element_id
    ae.push(0x00); // element_type = channel
    ae.extend(leb(codec_config_id));
    ae.extend(leb(1)); // num_substreams
    ae.extend(leb(0)); // substream id 0
    ae.extend(leb(0)); // num_parameters
    ae.push(0x20); // num_layers = 1
    ae.extend_from_slice(&[0x10, 0x01, 0x01]); // stereo layer, 1 substream
    ae
}

/// One stereo mix presentation OBU body.
fn mix_presentation_obu() -> Vec<u8> {
    let mut mp = leb(0); // mix_presentation_id
    mp.extend(leb(0)); // count_label
    mp.extend(leb(1)); // num_sub_mixes
    mp.extend(sub_mix(0));
    mp
}

// ---------------------------------------------------------------------------
// File assembler
// ---------------------------------------------------------------------------

/// An `enca` sample entry for the fixture builder.
#[derive(Clone)]
struct EncaSpec {
    /// Omit the `sinf` box entirely.
    omit_sinf: bool,
    /// `(scheme, version)` for `schm` (`None` omits the box).
    scheme: Option<([u8; 4], u32)>,
    /// Original format for `frma` (`None` omits the box).
    original: Option<[u8; 4]>,
    /// `schm` box version.
    schm_version: u8,
}

/// One `elst` edit for the fixture builder.
#[derive(Clone, Copy)]
struct FixtureEdit {
    segment_duration: u64,
    media_time: i64,
    media_rate: u32,
}

/// Options for the single-track fixture builder.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools, reason = "test-only fixture knobs")]
struct FileOpts {
    timescale: u32,
    deltas: Vec<u32>, // stts deltas, one per sample
    use_co64: bool,
    use_stz2: bool,
    /// Full `elst` edit list; empty omits the `edts` box.
    edits: Vec<FixtureEdit>,
    /// `elst` box version (0 or 1).
    elst_version: u8,
    /// Emit an `elst` box with zero entries (identity timeline).
    emit_empty_elst: bool,
    /// `mvhd` timescale override (defaults to the track timescale).
    mvhd_timescale: Option<u32>,
    /// Sync sample numbers for an emitted `stss` box (`None` omits it).
    stss: Option<Vec<u32>>,
    /// `stss` box version (0 only, per spec).
    stss_version: u8,
    /// `(sample_count, offset)` rows for an emitted `ctts` box.
    ctts: Option<Vec<(u32, i64)>>,
    /// `ctts` box version (0 unsigned, 1 signed).
    ctts_version: u8,
    iacb_version: u8,
    /// Future-field bytes appended after the OBU section inside `iacb`.
    iacb_trailing: Vec<u8>,
    /// Sample-entry index per chunk segment; frames split evenly across
    /// segments. Empty means one segment referencing entry 1.
    segments: Vec<u32>,
    /// Roll distance for an emitted `sbgp`/`sgpd` (`roll`) grouping.
    /// `None` emits no sample groups. Defaults to `Some(0)` so fixtures
    /// are spec-compliant for Opus/AAC.
    with_roll: Option<i16>,
    /// Emit an `sbgp` box with this grouping instead of `roll`
    /// (non-roll groupings are rejected).
    sbgp_grouping: Option<[u8; 4]>,
    /// Emit an `sgpd` box with `(grouping, body)` (version 1).
    sgpd_grouping: Option<([u8; 4], Vec<u8>)>,
    with_sdtp: bool,
    /// Emit `saiz`/`saio` auxiliary-info boxes (rejected).
    with_saiz_saio: bool,
    /// `enca` sample entries appended after the `iamf` entries.
    enca_entries: Vec<EncaSpec>,
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
            edits: Vec::new(),
            elst_version: 0,
            emit_empty_elst: false,
            mvhd_timescale: None,
            stss: None,
            stss_version: 0,
            ctts: None,
            ctts_version: 0,
            iacb_version: 1,
            iacb_trailing: Vec::new(),
            segments: Vec::new(),
            with_roll: Some(0),
            sbgp_grouping: None,
            sgpd_grouping: None,
            with_sdtp: false,
            with_saiz_saio: false,
            enca_entries: Vec::new(),
            ftyp_brands: vec![*b"isom", *b"iamf"],
            mdat_large: false,
            stco_offset: None,
        }
    }
}

/// Assemble a complete single-track file.
fn build_file(frames: &[Vec<u8>], sections: &[Vec<u8>], opts: &FileOpts) -> Vec<u8> {
    let deltas: Vec<u32> = if opts.deltas.is_empty() {
        vec![960; frames.len()]
    } else {
        opts.deltas.clone()
    };
    assert_eq!(deltas.len(), frames.len());
    assert!(!sections.is_empty(), "need at least one sample entry");
    // Segment boundaries: even split across the requested entries.
    let seg_descs: Vec<u32> = if opts.segments.is_empty() {
        vec![1]
    } else {
        opts.segments.clone()
    };
    assert_eq!(
        frames.len() % seg_descs.len(),
        0,
        "frames must split evenly"
    );
    for &d in &seg_descs {
        assert!(
            (d as usize) <= sections.len(),
            "segment references missing entry"
        );
    }

    let trak = trak_box(&deltas, frames, sections, opts);
    let duration: u32 = deltas.iter().sum();
    let movie_header = header_box_payload(opts.mvhd_timescale.unwrap_or(opts.timescale), duration);
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

    // Layout: patch one chunk offset per segment.
    let mdat_header = if opts.mdat_large { 16 } else { 8 };
    let mut data_offset =
        u64::try_from(ftyp.len() + moov.len() + mdat_header).expect("fixture fits in u64");
    let seg_len = frames.len() / seg_descs.len();
    let mut chunk_offsets = Vec::with_capacity(seg_descs.len());
    for seg in 0..seg_descs.len() {
        let base = opts.stco_offset.unwrap_or(data_offset);
        chunk_offsets.push(base);
        let seg_bytes: usize = frames[seg * seg_len..(seg + 1) * seg_len]
            .iter()
            .map(Vec::len)
            .sum();
        data_offset += u64::try_from(seg_bytes).expect("fixture fits in u64");
    }
    let offset_size = if opts.use_co64 { 8 } else { 4 };
    let mut file = Vec::new();
    file.extend(ftyp);
    file.extend(patch_chunk_offsets(&moov, &chunk_offsets, offset_size));
    file.extend(mdat);
    file
}

/// One `iamf` sample entry: `AudioSampleEntry` fields plus the `iacb`
/// child box (version byte, leb128 OBU size, OBUs, future bytes).
fn iamf_entry(obu_section: &[u8], opts: &FileOpts) -> Vec<u8> {
    let mut fields = vec![0u8; 6]; // reserved
    fields.extend(1u16.to_be_bytes()); // data_reference_index
    fields.extend([0u8; 8]); // reserved
    fields.extend(0u16.to_be_bytes()); // channelcount (SHALL be 0)
    fields.extend(0u16.to_be_bytes()); // samplesize
    fields.extend(0u32.to_be_bytes()); // samplerate (SHALL be 0)
    let mut iacb_payload = vec![opts.iacb_version];
    iacb_payload.extend(leb(
        u32::try_from(obu_section.len()).expect("fixture fits in u32")
    ));
    iacb_payload.extend_from_slice(obu_section);
    iacb_payload.extend_from_slice(&opts.iacb_trailing);
    fields.extend(bx(*b"iacb", &iacb_payload));
    bx(*b"iamf", &fields)
}

/// One `enca` sample entry: `AudioSampleEntry` fields plus `sinf`.
fn enca_entry(spec: &EncaSpec) -> Vec<u8> {
    let mut fields = vec![0u8; 6]; // reserved
    fields.extend(1u16.to_be_bytes()); // data_reference_index
    fields.extend([0u8; 8]); // reserved
    fields.extend(0u16.to_be_bytes()); // channelcount
    fields.extend(0u16.to_be_bytes()); // samplesize
    fields.extend(0u32.to_be_bytes()); // samplerate
    if !spec.omit_sinf {
        let mut sinf = Vec::new();
        if let Some(original) = spec.original {
            sinf.extend(bx(*b"frma", &original));
        }
        if let Some((scheme, version)) = spec.scheme {
            let mut schm = scheme.to_vec();
            schm.extend(version.to_be_bytes());
            sinf.extend(full(*b"schm", spec.schm_version, 0, &schm));
        }
        // An opaque `schi` box exercises the skip path.
        sinf.extend(bx(*b"schi", &[0xAA; 4]));
        fields.extend(bx(*b"sinf", &sinf));
    }
    bx(*b"enca", &fields)
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
fn trak_box(deltas: &[u32], frames: &[Vec<u8>], sections: &[Vec<u8>], opts: &FileOpts) -> Vec<u8> {
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
    if !opts.edits.is_empty() || opts.emit_empty_elst {
        let mut edit_list = u32::try_from(opts.edits.len())
            .expect("fixture fits in u32")
            .to_be_bytes()
            .to_vec();
        for edit in &opts.edits {
            if opts.elst_version == 1 {
                edit_list.extend(edit.segment_duration.to_be_bytes());
                edit_list.extend(edit.media_time.to_be_bytes());
                edit_list.extend(edit.media_rate.to_be_bytes());
            } else {
                let seg = u32::try_from(edit.segment_duration).expect("v0 seg fits in u32");
                edit_list.extend(seg.to_be_bytes());
                let media = if edit.media_time < 0 {
                    0xFFFF_FFFFu32 // empty edit
                } else {
                    u32::try_from(edit.media_time).expect("v0 media fits in u32")
                };
                edit_list.extend(media.to_be_bytes());
                edit_list.extend(edit.media_rate.to_be_bytes());
            }
        }
        track_bytes.extend(bx(
            *b"edts",
            &full(*b"elst", opts.elst_version, 0, &edit_list),
        ));
    }
    track_bytes.extend(mdia_box(deltas, frames, sections, opts));
    bx(*b"trak", &track_bytes)
}

/// One `mdia` box: `mdhd`, `hdlr`, and `minf`.
fn mdia_box(deltas: &[u32], frames: &[Vec<u8>], sections: &[Vec<u8>], opts: &FileOpts) -> Vec<u8> {
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
    media_bytes.extend(minf_box(deltas, frames, sections, opts));
    bx(*b"mdia", &media_bytes)
}

/// One `minf` box: `smhd`, `dinf`, `stbl`, `iacb`, plus a stray `free`.
fn minf_box(deltas: &[u32], frames: &[Vec<u8>], sections: &[Vec<u8>], opts: &FileOpts) -> Vec<u8> {
    let smhd = full(*b"smhd", 0, 0, &[0, 0, 0, 0]);
    let url_box = full(*b"url ", 0, 1, &[]);
    let mut data_ref = 0u32.to_be_bytes().to_vec();
    data_ref.extend(1u32.to_be_bytes());
    data_ref.extend(url_box);
    let dinf = bx(*b"dinf", &full(*b"dref", 0, 0, &data_ref));
    let mut movie_media = Vec::new();
    movie_media.extend(smhd);
    movie_media.extend(dinf);
    movie_media.extend(stbl_box(deltas, frames, sections, opts));
    // A stray free box exercises the unknown-box skip path.
    movie_media.extend(bx(*b"free", &[0xDE, 0xAD]));
    bx(*b"minf", &movie_media)
}

/// One `stbl` box: entries from sections, one chunk per segment.
fn stbl_box(deltas: &[u32], frames: &[Vec<u8>], sections: &[Vec<u8>], opts: &FileOpts) -> Vec<u8> {
    let frame_count = u32::try_from(frames.len()).expect("fixture fits in u32");
    // stsd: one `iamf` entry per section, then any `enca` entries.
    let mut desc_entries = u32::try_from(sections.len() + opts.enca_entries.len())
        .expect("fixture fits in u32")
        .to_be_bytes()
        .to_vec();
    for section in sections {
        desc_entries.extend(iamf_entry(section, opts));
    }
    for spec in &opts.enca_entries {
        desc_entries.extend(enca_entry(spec));
    }
    let entry_table = full(*b"stsd", 0, 0, &desc_entries);

    // stts: single row.
    let mut time_rows = 1u32.to_be_bytes().to_vec();
    time_rows.extend(frame_count.to_be_bytes());
    time_rows.extend(deltas.first().copied().unwrap_or(960).to_be_bytes());
    let timing_table = full(*b"stts", 0, 0, &time_rows);

    // stsc: one row per segment (first_chunk, samples_per_chunk, entry).
    let seg_descs: Vec<u32> = if opts.segments.is_empty() {
        vec![1]
    } else {
        opts.segments.clone()
    };
    let seg_len = frames.len() / seg_descs.len();
    let mut chunk_rows = u32::try_from(seg_descs.len())
        .expect("fixture fits in u32")
        .to_be_bytes()
        .to_vec();
    for (seg, &desc) in seg_descs.iter().enumerate() {
        let first = u32::try_from(seg + 1).expect("fixture fits in u32");
        let per = u32::try_from(seg_len).expect("fixture fits in u32");
        chunk_rows.extend(first.to_be_bytes());
        chunk_rows.extend(per.to_be_bytes());
        chunk_rows.extend(desc.to_be_bytes());
    }
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

    // stco / co64 with placeholders (one offset per segment), patched
    // after layout.
    let seg_total = u32::try_from(seg_descs.len()).expect("fixture fits in u32");
    let offset_box = if opts.use_co64 {
        let mut inner = seg_total.to_be_bytes().to_vec();
        inner.extend(vec![0u8; 8 * seg_descs.len()]);
        full(*b"co64", 0, 0, &inner)
    } else {
        let mut inner = seg_total.to_be_bytes().to_vec();
        inner.extend(vec![0u8; 4 * seg_descs.len()]);
        full(*b"stco", 0, 0, &inner)
    };

    let mut table_bytes = Vec::new();
    table_bytes.extend(entry_table);
    table_bytes.extend(timing_table);
    table_bytes.extend(chunk_table);
    table_bytes.extend(sample_sizes);
    group_boxes(opts, frame_count, &mut table_bytes);
    if opts.with_sdtp {
        let sdtp_payload = vec![0u8; frames.len()];
        table_bytes.extend(full(*b"sdtp", 0, 0, &sdtp_payload));
    }
    if opts.with_saiz_saio {
        table_bytes.extend(full(*b"saiz", 0, 0, &[]));
        table_bytes.extend(full(*b"saio", 0, 0, &[]));
    }
    if let Some(stss) = stss_box(opts) {
        table_bytes.extend(stss);
    }
    if let Some(ctts) = ctts_box(opts) {
        table_bytes.extend(ctts);
    }
    table_bytes.extend(offset_box);
    bx(*b"stbl", &table_bytes)
}

/// `sbgp`/`sgpd` boxes from the fixture options.
fn group_boxes(opts: &FileOpts, frame_count: u32, table_bytes: &mut Vec<u8>) {
    if let Some(grouping) = opts.sbgp_grouping {
        let mut sbgp_inner = grouping.to_vec();
        sbgp_inner.extend(1u32.to_be_bytes()); // one row
        sbgp_inner.extend(frame_count.to_be_bytes());
        sbgp_inner.extend(1u32.to_be_bytes()); // description index
        table_bytes.extend(full(*b"sbgp", 0, 0, &sbgp_inner));
    } else if let Some(roll) = opts.with_roll {
        let mut sbgp_inner = b"roll".to_vec();
        sbgp_inner.extend(1u32.to_be_bytes()); // one row
        sbgp_inner.extend(frame_count.to_be_bytes());
        sbgp_inner.extend(1u32.to_be_bytes()); // description index
        table_bytes.extend(full(*b"sbgp", 0, 0, &sbgp_inner));
        let mut sgpd_inner = b"roll".to_vec();
        sgpd_inner.extend(1u32.to_be_bytes()); // one entry
        sgpd_inner.extend(roll.to_be_bytes());
        table_bytes.extend(full(*b"sgpd", 1, 0, &sgpd_inner));
    }
    if let Some((grouping, body)) = &opts.sgpd_grouping {
        let mut sgpd_inner = grouping.to_vec();
        sgpd_inner.extend_from_slice(body);
        table_bytes.extend(full(*b"sgpd", 1, 0, &sgpd_inner));
    }
}

/// An `stss` box from the fixture options (`None` omits it).
fn stss_box(opts: &FileOpts) -> Option<Vec<u8>> {
    let entries = opts.stss.as_ref()?;
    let mut inner = u32::try_from(entries.len())
        .expect("fixture fits in u32")
        .to_be_bytes()
        .to_vec();
    for entry in entries {
        inner.extend(entry.to_be_bytes());
    }
    Some(full(*b"stss", opts.stss_version, 0, &inner))
}

/// A `ctts` box from the fixture options (`None` omits it).
fn ctts_box(opts: &FileOpts) -> Option<Vec<u8>> {
    let rows = opts.ctts.as_ref()?;
    let mut inner = u32::try_from(rows.len())
        .expect("fixture fits in u32")
        .to_be_bytes()
        .to_vec();
    for (count, offset) in rows {
        inner.extend(count.to_be_bytes());
        if opts.ctts_version == 1 {
            let value = i32::try_from(*offset).expect("v1 ctts fits in i32");
            inner.extend(value.to_be_bytes());
        } else {
            let value = u32::try_from(*offset).expect("v0 ctts non-negative");
            inner.extend(value.to_be_bytes());
        }
    }
    Some(full(*b"ctts", opts.ctts_version, 0, &inner))
}

/// Rewrite every chunk offset inside `moov`, in segment order.
fn patch_chunk_offsets(moov: &[u8], chunk_offsets: &[u64], offset_size: usize) -> Vec<u8> {
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
    let mut field = pos + 8;
    for &chunk_offset in chunk_offsets {
        if is_co64 {
            out[field..field + 8].copy_from_slice(&chunk_offset.to_be_bytes());
            field += 8;
        } else {
            let offset_u32 = u32::try_from(chunk_offset).expect("fixture fits in u32");
            out[field..field + 4].copy_from_slice(&offset_u32.to_be_bytes());
            field += 4;
        }
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
    let file = build_file(&frames, &[opus_obu_section(0)], &FileOpts::default());
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
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &FileOpts::default());
    register_decoders(&mut symphonia_core::codecs::registry::CodecRegistry::new());
    let reader = probe_file(file);
    assert_eq!(reader.tracks().len(), 1);
    assert_eq!(reader.format_info().short_name, "iamf");
}

#[test]
fn seek_rewinds_to_start() {
    let frames = opus_frames();
    let file = build_file(&frames, &[opus_obu_section(0)], &FileOpts::default());
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

/// Unity-rate media edit for the fixture builder.
fn media_edit(segment_duration: u64, media_time: i64) -> FixtureEdit {
    FixtureEdit {
        segment_duration,
        media_time,
        media_rate: 0x0001_0000,
    }
}

/// Empty (gap) edit for the fixture builder.
fn empty_edit(segment_duration: u64) -> FixtureEdit {
    FixtureEdit {
        segment_duration,
        media_time: -1,
        media_rate: 0x0001_0000,
    }
}

fn four_frames() -> Vec<Vec<u8>> {
    vec![
        vec![0x11; 64],
        vec![0x22; 128],
        vec![0x33; 96],
        vec![0x44; 48],
    ]
}

#[test]
fn elst_single_edit_selects_media_range() {
    // Media time selects samples; presentation restarts at zero. Four
    // frames at dts 0/960/1920/2880 with media [960, 2880) keep the
    // middle two.
    let frames = four_frames();
    let opts = FileOpts {
        edits: vec![media_edit(1920, 960)],
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].data.as_ref(), frames[1].as_slice());
    assert_eq!(packets[1].data.as_ref(), frames[2].as_slice());
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(960));
    assert_eq!(packets[0].dur, Duration::new(960));
    let track = &reader.tracks()[0];
    assert_eq!(track.start_ts, Timestamp::ZERO);
    assert_eq!(track.duration, Some(Duration::new(1920)));
    assert_eq!(track.num_frames, Some(1920));
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.edit_plan.segments.len(), 1);
    let seg = &config.edit_plan.segments[0];
    assert_eq!((seg.ord_start, seg.ord_end), (1, 3));
    assert_eq!((seg.pts_base, seg.media_base), (0, 960));
}

#[test]
fn elst_leading_empty_edit_delays_start() {
    let frames = opus_frames();
    let opts = FileOpts {
        edits: vec![empty_edit(1000), media_edit(2880, 0)],
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(
            packet.pts,
            Timestamp::new(1000 + i64::try_from(i).expect("small index") * 960)
        );
        assert_eq!(packet.data.as_ref(), frames[i].as_slice());
    }
    let track = &reader.tracks()[0];
    assert_eq!(track.start_ts, Timestamp::new(1000));
    assert_eq!(track.duration, Some(Duration::new(3880)));
    assert_eq!(track.num_frames, Some(2880));
}

#[test]
fn elst_mid_sample_start_drops_whole_packet() {
    // Media time 1000 lands inside the dts-960 packet: sample
    // granularity drops it whole, keeping dts 1920/2880.
    let frames = four_frames();
    let opts = FileOpts {
        edits: vec![media_edit(2880, 1000)],
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].data.as_ref(), frames[2].as_slice());
    assert_eq!(packets[1].data.as_ref(), frames[3].as_slice());
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(960));
}

#[test]
fn elst_multiple_media_edits_concatenate() {
    // Two disjoint ranges concatenate on the presentation timeline,
    // skipping the middle samples.
    let frames = four_frames();
    let opts = FileOpts {
        edits: vec![media_edit(960, 0), media_edit(960, 1920)],
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].data.as_ref(), frames[0].as_slice());
    assert_eq!(packets[1].data.as_ref(), frames[2].as_slice());
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(960));
    let track = &reader.tracks()[0];
    assert_eq!(track.duration, Some(Duration::new(1920)));
    assert_eq!(track.num_frames, Some(1920));
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.edit_plan.segments.len(), 2);
    assert_eq!(config.edit_plan.selected_samples, 2);
}

#[test]
fn elst_movie_timescale_conversion() {
    // Empty-edit durations live in the movie timescale (1000 here):
    // 500 units delay presentation by 24000 media units.
    let frames = opus_frames();
    let opts = FileOpts {
        edits: vec![empty_edit(500), media_edit(60, 0)],
        mvhd_timescale: Some(1000),
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(
            packet.pts,
            Timestamp::new(24000 + i64::try_from(i).expect("small index") * 960)
        );
    }
    let track = &reader.tracks()[0];
    assert_eq!(track.start_ts, Timestamp::new(24000));
    assert_eq!(track.duration, Some(Duration::new(26880)));
}

#[test]
fn elst_v1_parses() {
    let frames = four_frames();
    let opts = FileOpts {
        edits: vec![media_edit(1920, 960)],
        elst_version: 1,
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(960));
    assert_eq!(packets[0].data.as_ref(), frames[1].as_slice());
}

#[test]
fn elst_trailing_empty_extends_duration() {
    let frames = opus_frames();
    let opts = FileOpts {
        edits: vec![media_edit(2880, 0), empty_edit(960)],
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[2].pts, Timestamp::new(1920));
    let track = &reader.tracks()[0];
    assert_eq!(track.duration, Some(Duration::new(3840)));
    assert_eq!(track.num_frames, Some(2880));
}

#[test]
fn elst_clamps_past_eof() {
    // A segment running past the media end keeps every sample.
    let frames = opus_frames();
    let opts = FileOpts {
        edits: vec![media_edit(99999, 0)],
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    let track = &reader.tracks()[0];
    assert_eq!(track.duration, Some(Duration::new(99999)));
}

#[test]
fn elst_zero_entry_identity() {
    let frames = opus_frames();
    let opts = FileOpts {
        emit_empty_elst: true,
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[2].pts, Timestamp::new(1920));
}

#[test]
fn elst_rounds_timescale_conversion() {
    // 44100/1000: empty(5) = 220.5 -> 221 and empty(3) = 132.3 -> 132,
    // distinguishing round-to-nearest from both floor (352) and ceil (354).
    let frames = opus_frames();
    let opts = FileOpts {
        timescale: 44100,
        edits: vec![empty_edit(5), empty_edit(3), media_edit(65, 0)],
        mvhd_timescale: Some(1000),
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].pts, Timestamp::new(353));
    let track = &reader.tracks()[0];
    // Media segment 65 -> 2866.5 -> 2867.
    assert_eq!(track.duration, Some(Duration::new(353 + 2867)));
}

#[test]
fn elst_selects_fragment_samples() {
    // The plan applies after the fragment merge: this edit keeps only
    // the two `moof` samples. `tfdt` stays on the media timeline.
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let init_opts = FileOpts {
        edits: vec![media_edit(1920, 1920)],
        ..FileOpts::default()
    };
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &init_opts,
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[frag_spec(&frag)],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].data.as_ref(), frag[0].as_slice());
    assert_eq!(packets[1].data.as_ref(), frag[1].as_slice());
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(960));
}

/// One `traf` of explicit-size samples at the segment `mdat` start.
fn frag_spec(frag: &[Vec<u8>]) -> FragSpec {
    FragSpec {
        track_id: 1,
        desc: None,
        default_duration: None,
        default_size: None,
        base: FragBase::Patched,
        tfdt: Some(1920),
        empty: false,
        traf_sdtp: false,
        traf_sbgp: None,
        traf_sgpd_roll: None,
        traf_sgpd_raw: None,
        traf_saiz: false,
        truns: vec![FragTrun {
            data_offset: FragOffset::Explicit(0),
            version: 0,
            first_sample_flags: false,
            cts: Vec::new(),
            samples: frag
                .iter()
                .map(|data| FragSample {
                    size: Some(u32::try_from(data.len()).expect("fixture fits in u32")),
                    duration: None,
                    data: data.clone(),
                })
                .collect(),
        }],
    }
}

#[test]
fn rejects_elst_bad_version() {
    let opts = FileOpts {
        edits: vec![media_edit(2880, 0)],
        elst_version: 2,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn rejects_elst_non_unity_rate() {
    let edit = FixtureEdit {
        segment_duration: 2880,
        media_time: 0,
        media_rate: 0x0002_0000,
    };
    let opts = FileOpts {
        edits: vec![edit],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("elst media rate")
    ));
}

#[test]
fn rejects_elst_dwell() {
    let edit = FixtureEdit {
        segment_duration: 2880,
        media_time: 0,
        media_rate: 0,
    };
    let opts = FileOpts {
        edits: vec![edit],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("elst dwell edit")
    ));
}

#[test]
fn rejects_elst_repeated_range() {
    // Second range [960, 2880) overlaps the first [0, 1920).
    let opts = FileOpts {
        edits: vec![media_edit(1920, 0), media_edit(1920, 960)],
        ..FileOpts::default()
    };
    let file = build_file(&four_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("elst media ranges must be ordered and disjoint")
    ));
}

#[test]
fn rejects_elst_backward_jump() {
    let opts = FileOpts {
        edits: vec![media_edit(960, 1920), media_edit(960, 0)],
        ..FileOpts::default()
    };
    let file = build_file(&four_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("elst media ranges must be ordered and disjoint")
    ));
}

#[test]
fn rejects_elst_negative_media_time() {
    let edit = FixtureEdit {
        segment_duration: 960,
        media_time: -5,
        media_rate: 0x0001_0000,
    };
    let opts = FileOpts {
        edits: vec![edit],
        elst_version: 1,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn co64_and_stz2_variant_demuxes() {
    let frames = opus_frames();
    let opts = FileOpts {
        use_co64: true,
        use_stz2: true,
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
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
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.data.as_ref(), frames[0].as_slice());
}

#[test]
fn track_config_exposes_roll_distance() {
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &FileOpts::default());
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
        let file = build_file(
            &opus_frames(),
            std::slice::from_ref(&section),
            &FileOpts::default(),
        );
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
    stream.extend(obu(1, &channel_element_obu(cc_id)));
    stream.extend(obu(2, &mix_presentation_obu()));
    stream
}

// ---------------------------------------------------------------------------
// Bridge
// ---------------------------------------------------------------------------

#[test]
fn reassemble_single_track_roundtrips() {
    let section = opus_obu_section(0);
    let file = build_file(
        &opus_frames(),
        std::slice::from_ref(&section),
        &FileOpts::default(),
    );
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
        symphonia_iamf_core::obu::parser::parse_obu_header(&section1).expect("seqhdr");
    let seq_total = header_size + header.payload_size;
    let mut section2 = section1[..seq_total].to_vec();
    let mut cc = leb(7);
    cc.extend_from_slice(b"Opus");
    cc.extend(leb(960));
    cc.extend_from_slice(&0i16.to_be_bytes());
    cc.extend_from_slice(&[1, 2, 0x38, 0x01, 0, 0, 0xBB, 0x80, 0, 0, 0]);
    section2.extend(obu(0, &cc));
    section2.extend(obu(1, &channel_element_obu(7)));
    section2.extend(obu(2, &mix_presentation_obu()));

    // Parse both files and reassemble across readers (each file holds one
    // track; the bridge only needs the track configs).
    let file_a = build_file(
        &opus_frames(),
        std::slice::from_ref(&section1),
        &FileOpts::default(),
    );
    let file_b = build_file(
        &opus_frames(),
        std::slice::from_ref(&section2),
        &FileOpts::default(),
    );
    let ra = open_reader(file_a.clone());
    let rb = open_reader(file_b);
    let ta = ra.track_config(1).expect("a").clone();
    let tb = rb.track_config(1).expect("b").clone();
    assert_eq!(tb.codec_config.codec_config_id, 7);
    let rebuilt = reassemble_ia_sequence(&[ta, tb]).expect("reassemble");
    // One sequence header + both tracks' full descriptor sets.
    let (desc, _) = parse_descriptors(&rebuilt).expect("rebuilt parses");
    assert_eq!(desc.codec_configs.len(), 2);
    assert_eq!(desc.audio_elements.len(), 2);
    assert_eq!(desc.mix_presentations.len(), 2);
    // Mismatched sequence headers are rejected: flip a profile byte (still
    // valid OBUs, but the raw bytes differ across tracks).
    let mut bad = section2.clone();
    bad[seq_total - 1] ^= 0xFF;
    let file_bad = build_file(&opus_frames(), &[bad], &FileOpts::default());
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
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn bare_moof_without_traf_is_ignored() {
    // A `moof` carrying no track fragments no longer rejects the file;
    // packets come from the `stbl` table alone.
    let mut file = build_file(&opus_frames(), &[opus_obu_section(0)], &FileOpts::default());
    // Append a minimal moof after mdat.
    file.extend(bx(*b"moof", &bx(*b"mfhd", &[0, 0, 0, 0, 0, 0, 0, 1])));
    let mut reader = open_reader(file);
    let mut count = 0;
    while reader.next_packet().expect("packet").is_some() {
        count += 1;
    }
    assert_eq!(count, 3);
}

#[test]
fn rejects_sample_past_eof() {
    let opts = FileOpts {
        stco_offset: Some(0x00FF_FFFF),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn roll_groups_absent_rejected_for_opus() {
    // Opus samples SHALL carry `roll` groups.
    let opts = FileOpts {
        with_roll: None,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn roll_groups_mismatch_rejected() {
    // Group distance 5 disagrees with the codec config's roll of 0.
    let opts = FileOpts {
        with_roll: Some(5),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn roll_groups_absent_allowed_for_flac() {
    // Non-Opus/AAC codecs tolerate missing roll groups.
    let section = custom_cc_section(*b"fLaC", &[0x10, 0, 0x22, 0, 0], 0);
    let opts = FileOpts {
        with_roll: None,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[section], &opts);
    let mut reader = open_reader(file);
    assert!(reader.next_packet().unwrap().is_some());
}

#[test]
fn rejects_rap_grouping() {
    let opts = FileOpts {
        sbgp_grouping: Some(*b"rap "),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn rejects_sdtp_dependencies() {
    let opts = FileOpts {
        with_sdtp: true,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn ignores_unknown_iacb_version() {
    // `configurationVersion != 1` boxes SHALL be ignored, leaving no
    // usable entry behind.
    let opts = FileOpts {
        iacb_version: 2,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn iacb_trailing_bytes_ignored() {
    // Future fields past `configOBUs_size` must not disturb parsing.
    let opts = FileOpts {
        iacb_trailing: vec![0xAB, 0xCD, 0xEF],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packet = reader.next_packet().unwrap().unwrap();
    assert_eq!(packet.data.as_ref(), opus_frames()[0].as_slice());
}

#[test]
fn two_entries_switch_mid_track() {
    // Two sample entries, two chunks: the second chunk's `stsc` row
    // selects entry 2 (codec config 7).
    let section1 = opus_obu_section(0);
    let section2 = custom_opus_section(7);
    let frames = opus_frames();
    assert_eq!(frames.len() % 2, 1); // 3 frames: 1 + 2 split below
    let opts = FileOpts {
        segments: vec![1, 2],
        ..FileOpts::default()
    };
    // Even split needs even frame counts: use 4 frames.
    let frames = vec![
        vec![0x11; 64],
        vec![0x22; 128],
        vec![0x33; 96],
        vec![0x44; 48],
    ];
    let file = build_file(&frames, &[section1, section2], &opts);
    let reader = open_reader(file);
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.entries.len(), 2);
    assert_eq!(config.entries[1].codec_config.codec_config_id, 7);
    assert_eq!(config.sample_table.runs.len(), 2);
    assert_eq!(config.sample_table.runs[0].entry_idx, 0);
    assert_eq!(config.sample_table.runs[1].entry_idx, 1);
    let rebuilt = reassemble_ia_sequence(std::slice::from_ref(config)).expect("reassemble");
    let (desc, _) = parse_descriptors(&rebuilt).expect("rebuilt parses");
    assert_eq!(desc.codec_configs.len(), 2);
}

#[test]
fn rejects_unknown_stsc_entry() {
    // Segment 2 references entry 3, but only 2 entries exist. The
    // builder guards this, so corrupt the row after assembly: second
    // row's description index sits 28 bytes into the `stsc` payload.
    let frames = vec![vec![0x11; 64], vec![0x22; 128]];
    let opts = FileOpts {
        segments: vec![1, 2],
        ..FileOpts::default()
    };
    let mut file = build_file(
        &frames,
        &[opus_obu_section(0), custom_opus_section(7)],
        &opts,
    );
    let ftyp_len = u32::from_be_bytes(file[0..4].try_into().expect("ftyp size")) as usize;
    let pos = find_box(&file, ftyp_len + 8, *b"trak");
    let pos = find_box(&file, pos, *b"mdia");
    let pos = find_box(&file, pos, *b"minf");
    let pos = find_box(&file, pos, *b"stbl");
    let pos = find_box(&file, pos, *b"stsc");
    file[pos + 28..pos + 32].copy_from_slice(&3u32.to_be_bytes());
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

/// Opus descriptor section with a custom codec-config id (full shape).
fn custom_opus_section(cc_id: u32) -> Vec<u8> {
    let section1 = opus_obu_section(0);
    let (header, header_size) =
        symphonia_iamf_core::obu::parser::parse_obu_header(&section1).expect("seqhdr");
    let seq_total = header_size + header.payload_size;
    let mut section = section1[..seq_total].to_vec();
    let mut cc = leb(cc_id);
    cc.extend_from_slice(b"Opus");
    cc.extend(leb(960));
    cc.extend_from_slice(&0i16.to_be_bytes());
    cc.extend_from_slice(&[1, 2, 0x38, 0x01, 0, 0, 0xBB, 0x80, 0, 0, 0]);
    section.extend(obu(0, &cc));
    section.extend(obu(1, &channel_element_obu(cc_id)));
    section.extend(obu(2, &mix_presentation_obu()));
    section
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
// Fragmented MP4 (M2-3)
// ---------------------------------------------------------------------------

/// `trex` defaults for one track.
struct FragTrex {
    track_id: u32,
    desc: u32,
    duration: u32,
    size: u32,
}

/// One fragment sample: payload plus optional per-sample `trun` fields.
struct FragSample {
    data: Vec<u8>,
    duration: Option<u32>,
    size: Option<u32>,
}

/// How a `trun` locates its first sample.
enum FragOffset {
    /// Explicit `data_offset` relative to the `tfhd` base.
    Explicit(i32),
    /// Derived after layout: segment `mdat` payload minus `moof` start.
    FromMoofStart,
    /// Omitted flag: samples continue past the previous run.
    Implicit,
}

/// One `trun` in a fragment.
struct FragTrun {
    data_offset: FragOffset,
    /// `trun` box version (0, or 1 for signed composition offsets).
    version: u8,
    /// Emit (tolerated) `first_sample_flags`.
    first_sample_flags: bool,
    /// Per-sample composition offsets; empty omits the flag.
    cts: Vec<i64>,
    samples: Vec<FragSample>,
}

/// How `tfhd` locates sample data.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FragBase {
    /// Explicit `base_data_offset`, patched to the segment `mdat` payload.
    Patched,
    /// `default-base-is-moof`: the base is the `moof` start.
    Moof,
    /// Neither form (malformed).
    Missing,
}

/// One track fragment: a `traf` plus its segment `mdat` payload.
/// `(grouping, rows)` for a traf-level `sbgp` box.
type TrafSbgp = ([u8; 4], Vec<(u32, u32)>);

struct FragSpec {
    track_id: u32,
    desc: Option<u32>,
    default_duration: Option<u32>,
    default_size: Option<u32>,
    base: FragBase,
    tfdt: Option<u64>,
    empty: bool,
    truns: Vec<FragTrun>,
    /// Emit an `sdtp` box in the `traf` (rejected).
    traf_sdtp: bool,
    /// `(grouping, rows)` for a traf-level `sbgp` box.
    traf_sbgp: Option<TrafSbgp>,
    /// Roll distances for a traf-level `sgpd` (version 1) box.
    traf_sgpd_roll: Option<Vec<i16>>,
    /// `(grouping, body)` for a raw traf-level `sgpd` box.
    traf_sgpd_raw: Option<([u8; 4], Vec<u8>)>,
    /// Emit `saiz`/`saio` boxes in the `traf` (rejected).
    traf_saiz: bool,
}

fn trex_box(spec: &FragTrex) -> Vec<u8> {
    let mut payload = spec.track_id.to_be_bytes().to_vec();
    payload.extend(spec.desc.to_be_bytes());
    payload.extend(spec.duration.to_be_bytes());
    payload.extend(spec.size.to_be_bytes());
    payload.extend(0u32.to_be_bytes()); // default flags
    full(*b"trex", 0, 0, &payload)
}

/// One `tfhd` box; returns the bytes plus the payload-relative offset of
/// the `base_data_offset` field when it needs patching.
fn tfhd_box(spec: &FragSpec) -> (Vec<u8>, Option<usize>) {
    let mut flags = 0u32;
    let mut payload = spec.track_id.to_be_bytes().to_vec();
    let mut base_pos = None;
    match spec.base {
        FragBase::Patched => {
            flags |= 0x0000_0001;
            base_pos = Some(payload.len());
            payload.extend(0u64.to_be_bytes());
        }
        FragBase::Moof => flags |= 0x0000_020000,
        FragBase::Missing => {}
    }
    if let Some(desc) = spec.desc {
        flags |= 0x0000_0002;
        payload.extend(desc.to_be_bytes());
    }
    if let Some(duration) = spec.default_duration {
        flags |= 0x0000_0008;
        payload.extend(duration.to_be_bytes());
    }
    if let Some(size) = spec.default_size {
        flags |= 0x0000_0010;
        payload.extend(size.to_be_bytes());
    }
    if spec.empty {
        flags |= 0x0000_010000;
    }
    (full(*b"tfhd", 0, flags, &payload), base_pos)
}

/// One `trun` box; returns the bytes plus payload-relative offsets of
/// `FromMoofStart` fields needing patching.
fn trun_box(spec: &FragTrun) -> (Vec<u8>, Vec<usize>) {
    let mut flags = 0u32;
    if !matches!(spec.data_offset, FragOffset::Implicit) {
        flags |= 0x0000_0001;
    }
    if spec.first_sample_flags {
        flags |= 0x0000_0004;
    }
    let with_durations = spec.samples.iter().any(|s| s.duration.is_some());
    let with_sizes = spec.samples.iter().any(|s| s.size.is_some());
    if with_durations {
        flags |= 0x0000_0100;
    }
    if with_sizes {
        flags |= 0x0000_0200;
    }
    let with_cts = !spec.cts.is_empty();
    if with_cts {
        flags |= 0x0000_0800;
        assert_eq!(
            spec.cts.len(),
            spec.samples.len(),
            "one composition offset per sample"
        );
    }
    let mut payload = u32::try_from(spec.samples.len())
        .expect("fixture fits in u32")
        .to_be_bytes()
        .to_vec();
    let mut rels = Vec::new();
    match spec.data_offset {
        FragOffset::Explicit(value) => payload.extend(value.to_be_bytes()),
        FragOffset::FromMoofStart => {
            rels.push(payload.len());
            payload.extend(0i32.to_be_bytes());
        }
        FragOffset::Implicit => {}
    }
    if spec.first_sample_flags {
        payload.extend(0u32.to_be_bytes());
    }
    for (i, sample) in spec.samples.iter().enumerate() {
        if with_durations {
            payload.extend(sample.duration.unwrap_or(0).to_be_bytes());
        }
        if with_sizes {
            payload.extend(sample.size.unwrap_or(0).to_be_bytes());
        }
        if with_cts {
            // Four-byte entries: signed for version 1, unsigned for 0.
            if spec.version == 1 {
                let value = i32::try_from(spec.cts[i]).expect("v1 cts fits in i32");
                payload.extend(value.to_be_bytes());
            } else {
                let value = u32::try_from(spec.cts[i]).expect("v0 cts non-negative");
                payload.extend(value.to_be_bytes());
            }
        }
    }
    (full(*b"trun", spec.version, flags, &payload), rels)
}

/// Built `traf`: the boxed bytes, the segment `mdat` payload, and
/// traf-relative patch points.
struct TrafBuild {
    bytes: Vec<u8>,
    payload: Vec<u8>,
    base: Option<usize>,
    rels: Vec<usize>,
}

fn traf_build(spec: &FragSpec) -> TrafBuild {
    let mut children = Vec::new();
    let (tfhd, base_in_tfhd) = tfhd_box(spec);
    // +8 box header, +4 `FullBox` header: `tfhd_box` positions are
    // relative to the `FullBox` payload.
    let base = base_in_tfhd.map(|p| children.len() + 12 + p);
    children.extend(tfhd);
    if let Some(decode_time) = spec.tfdt {
        // Version 0 carries a 32-bit decode time.
        let time = u32::try_from(decode_time).expect("fixture fits in u32");
        children.extend(full(*b"tfdt", 0, 0, &time.to_be_bytes()));
    }
    let mut rels = Vec::new();
    let mut payload = Vec::new();
    for trun in &spec.truns {
        let (bytes, trun_rels) = trun_box(trun);
        for rel in trun_rels {
            // +8 box header, +4 `FullBox` header, like above.
            rels.push(children.len() + 12 + rel);
        }
        children.extend(bytes);
        for sample in &trun.samples {
            payload.extend_from_slice(&sample.data);
        }
    }
    if spec.traf_sdtp {
        let total: usize = spec.truns.iter().map(|t| t.samples.len()).sum();
        children.extend(full(*b"sdtp", 0, 0, &vec![0u8; total]));
    }
    if let Some((grouping, rows)) = &spec.traf_sbgp {
        let mut inner = grouping.to_vec();
        inner.extend(
            u32::try_from(rows.len())
                .expect("fixture fits in u32")
                .to_be_bytes(),
        );
        for (count, index) in rows {
            inner.extend(count.to_be_bytes());
            inner.extend(index.to_be_bytes());
        }
        children.extend(full(*b"sbgp", 0, 0, &inner));
    }
    if let Some(distances) = &spec.traf_sgpd_roll {
        let mut inner = b"roll".to_vec();
        inner.extend(
            u32::try_from(distances.len())
                .expect("fixture fits in u32")
                .to_be_bytes(),
        );
        for distance in distances {
            inner.extend(distance.to_be_bytes());
        }
        children.extend(full(*b"sgpd", 1, 0, &inner));
    }
    if let Some((grouping, body)) = &spec.traf_sgpd_raw {
        let mut inner = grouping.to_vec();
        inner.extend_from_slice(body);
        children.extend(full(*b"sgpd", 1, 0, &inner));
    }
    if spec.traf_saiz {
        children.extend(full(*b"saiz", 0, 0, &[]));
        children.extend(full(*b"saio", 0, 0, &[]));
    }
    // +8 for the `traf` box header.
    let base = base.map(|p| p + 8);
    let rels = rels.into_iter().map(|p| p + 8).collect();
    TrafBuild {
        bytes: bx(*b"traf", &children),
        payload,
        base,
        rels,
    }
}

/// Empty `stbl`: entries plus zero-size tables (init-segment style). No
/// sample groups are emitted: Opus/AAC fragment-only tracks need
/// traf-level `roll` groups — use FLAC or a non-empty init segment
/// for those codecs here.
fn stbl_empty(sections: &[Vec<u8>], opts: &FileOpts) -> Vec<u8> {
    let mut desc_entries = u32::try_from(sections.len())
        .expect("fixture fits in u32")
        .to_be_bytes()
        .to_vec();
    for section in sections {
        desc_entries.extend(iamf_entry(section, opts));
    }
    let entry_table = full(*b"stsd", 0, 0, &desc_entries);
    let timing_table = full(*b"stts", 0, 0, &0u32.to_be_bytes());
    let chunk_table = full(*b"stsc", 0, 0, &0u32.to_be_bytes());
    let mut sizes = 0u32.to_be_bytes().to_vec();
    sizes.extend(0u32.to_be_bytes());
    let sample_sizes = full(*b"stsz", 0, 0, &sizes);
    let offset_box = full(*b"stco", 0, 0, &0u32.to_be_bytes());
    let mut table_bytes = Vec::new();
    table_bytes.extend(entry_table);
    table_bytes.extend(timing_table);
    table_bytes.extend(chunk_table);
    table_bytes.extend(sample_sizes);
    table_bytes.extend(offset_box);
    bx(*b"stbl", &table_bytes)
}

fn minf_box_with_stbl(stbl: Vec<u8>) -> Vec<u8> {
    let smhd = full(*b"smhd", 0, 0, &[0, 0, 0, 0]);
    let mut media = Vec::new();
    media.extend(smhd);
    media.extend(stbl);
    bx(*b"minf", &media)
}

fn mdia_box_with_stbl(timescale: u32, duration: u32, stbl: Vec<u8>) -> Vec<u8> {
    let mut media_header = header_box_payload(timescale, duration);
    media_header.extend([0x55, 0xC4, 0, 0]); // language + quality
    let mdhd = full(*b"mdhd", 0, 0, &media_header);
    let mut media_bytes = Vec::new();
    media_bytes.extend(mdhd);
    media_bytes.extend(minf_box_with_stbl(stbl));
    bx(*b"mdia", &media_bytes)
}

fn trak_box_with_stbl(track_id: u32, duration: u32, timescale: u32, stbl: Vec<u8>) -> Vec<u8> {
    let mut track_header = 0u32.to_be_bytes().to_vec(); // creation
    track_header.extend(0u32.to_be_bytes()); // modification
    track_header.extend(track_id.to_be_bytes());
    track_header.extend(0u32.to_be_bytes()); // reserved
    track_header.extend(duration.to_be_bytes());
    let tkhd = full(*b"tkhd", 0, 3, &track_header);
    let mut track_bytes = Vec::new();
    track_bytes.extend(tkhd);
    track_bytes.extend(mdia_box_with_stbl(timescale, duration, stbl));
    bx(*b"trak", &track_bytes)
}

/// File piece: a `moof` with patch points, or a raw `mdat`.
enum FragPiece {
    Moof(Vec<u8>, Vec<(usize, bool, usize)>),
    Mdat(Vec<u8>),
}

/// Assemble `ftyp + moov(mvhd, trak, mvex?) + init mdat? + (moof + mdat)*`.
///
/// The init segment holds `init_frames` (empty for a bare init segment);
/// `trex: None` omits `mvex` entirely. Patch points resolve after layout:
/// each `(moof-relative offset, is-u64, mdat piece index)` targets a
/// segment `mdat` payload.
fn build_fragmented_file(
    init_frames: &[Vec<u8>],
    sections: &[Vec<u8>],
    init_opts: &FileOpts,
    trex: Option<&FragTrex>,
    frags: &[FragSpec],
) -> Vec<u8> {
    let deltas = vec![960u32; init_frames.len()];
    let stbl_duration: u32 = deltas.iter().sum();
    let track_box = if init_frames.is_empty() {
        trak_box_with_stbl(1, 0, init_opts.timescale, stbl_empty(sections, init_opts))
    } else {
        trak_box(&deltas, init_frames, sections, init_opts)
    };
    let mut movie_payload = Vec::new();
    movie_payload.extend(full(
        *b"mvhd",
        0,
        0,
        &header_box_payload(48_000, stbl_duration),
    ));
    movie_payload.extend(track_box);
    if let Some(defaults) = trex {
        let mut mvex_payload = trex_box(defaults);
        mvex_payload.extend(full(*b"mehd", 0, 0, &stbl_duration.to_be_bytes()));
        movie_payload.extend(bx(*b"mvex", &mvex_payload));
    }
    let movie_box = bx(*b"moov", &movie_payload);

    let mut ftyp_payload = b"isom".to_vec();
    ftyp_payload.extend(0u32.to_be_bytes());
    ftyp_payload.extend_from_slice(b"iamf");
    let ftyp = bx(*b"ftyp", &ftyp_payload);

    let mut pieces = Vec::new();
    if !init_frames.is_empty() {
        let mut payload = Vec::new();
        for frame in init_frames {
            payload.extend_from_slice(frame);
        }
        pieces.push(FragPiece::Mdat(bx(*b"mdat", &payload)));
    }
    for (i, spec) in frags.iter().enumerate() {
        let traf = traf_build(spec);
        let seq = 1u32
            .checked_add(u32::try_from(i).expect("fixture fits in u32"))
            .expect("fixture fits in u32");
        let mfhd = full(*b"mfhd", 0, 0, &seq.to_be_bytes());
        // `traf`-relative patch points already include the `traf`
        // header: add the `moof` header plus `mfhd`.
        let traf_pos = 8 + mfhd.len();
        let mut moof_payload = mfhd;
        moof_payload.extend(traf.bytes);
        let moof = bx(*b"moof", &moof_payload);
        let mdat_index = pieces.len() + 1;
        let mut patches = Vec::new();
        if let Some(base) = traf.base {
            patches.push((traf_pos + base, true, mdat_index));
        }
        for rel in traf.rels {
            patches.push((traf_pos + rel, false, mdat_index));
        }
        pieces.push(FragPiece::Moof(moof, patches));
        pieces.push(FragPiece::Mdat(bx(*b"mdat", &traf.payload)));
    }

    // The init `stbl` carries a single chunk: point it at the init `mdat`.
    let init_payload_abs = u64::try_from(ftyp.len() + movie_box.len() + 8).expect("fixture fits");
    let movie_box = if init_frames.is_empty() {
        movie_box
    } else {
        patch_chunk_offsets(&movie_box, &[init_payload_abs], 4)
    };
    let mut file = Vec::new();
    file.extend(ftyp);
    file.extend(movie_box);

    // Layout pass: record every piece start and `mdat` payload offset.
    let mut piece_starts = Vec::with_capacity(pieces.len());
    let mut mdat_payloads: Vec<Option<u64>> = Vec::with_capacity(pieces.len());
    for piece in &pieces {
        piece_starts.push(u64::try_from(file.len()).expect("fixture fits"));
        match piece {
            FragPiece::Mdat(bytes) => {
                mdat_payloads.push(Some(u64::try_from(file.len() + 8).expect("fixture fits")));
                file.extend_from_slice(bytes);
            }
            FragPiece::Moof(bytes, _) => {
                mdat_payloads.push(None);
                file.extend_from_slice(bytes);
            }
        }
    }
    // Patch pass: resolve base/data offsets against the layout.
    for (i, piece) in pieces.iter().enumerate() {
        let FragPiece::Moof(_, patches) = piece else {
            continue;
        };
        let moof_start = piece_starts[i];
        for &(rel, is_u64, mdat_index) in patches {
            let at = usize::try_from(moof_start + u64::try_from(rel).expect("small offset"))
                .expect("fixture fits");
            let target = mdat_payloads[mdat_index].expect("patch targets mdat");
            if is_u64 {
                file[at..at + 8].copy_from_slice(&target.to_be_bytes());
            } else {
                let relative = i32::try_from(target.checked_sub(moof_start).expect("mdat follows"))
                    .expect("fixture fits in i32");
                file[at..at + 4].copy_from_slice(&relative.to_be_bytes());
            }
        }
    }
    file
}

/// Drain all packets from a reader.
fn drain_packets(reader: &mut IamfFormatReader<'_>) -> Vec<symphonia_core::packet::Packet> {
    let mut out = Vec::new();
    while let Some(packet) = reader.next_packet().expect("packet") {
        out.push(packet);
    }
    out
}

fn flac_section() -> Vec<u8> {
    custom_cc_section(*b"fLaC", &[0x10, 0, 0x22, 0, 0], 0)
}

#[test]
fn fragments_extend_track() {
    // Two `stbl` samples plus one fragment of two: durations fall back
    // to the `trex` default, sizes are per-sample, `tfdt` continues the
    // timeline exactly.
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[FragSpec {
            track_id: 1,
            desc: None,
            default_duration: None,
            default_size: None,
            base: FragBase::Patched,
            tfdt: Some(1920),
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![
                    FragSample {
                        data: frag[0].clone(),
                        duration: None,
                        size: Some(96),
                    },
                    FragSample {
                        data: frag[1].clone(),
                        duration: None,
                        size: Some(48),
                    },
                ],
            }],
        }],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    let expected = [init, frag].concat();
    assert_eq!(packets.len(), 4);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(packet.track_id, 1);
        assert_eq!(
            packet.pts,
            Timestamp::new(i64::try_from(i).expect("small index") * 960)
        );
        assert_eq!(packet.dur, Duration::new(960));
        assert_eq!(packet.data.as_ref(), expected[i].as_slice());
    }
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.sample_count, 4);
    assert_eq!(config.sample_table.duration, 3840);
    assert_eq!(config.sample_table.stts, vec![(4, 960)]);
    assert_eq!(config.sample_table.runs.len(), 2);
    assert_eq!(config.sample_table.runs[1].entry_idx, 0);
    assert_eq!(config.sample_table.sample_roll, vec![0, 0, 0, 0]);
}

#[test]
fn fragment_selects_second_entry() {
    // `tfhd` description index 2 routes the fragment run (and its roll
    // default) to the second sample entry.
    let sections = vec![opus_obu_section(0), opus_obu_section_with_roll(7, 5)];
    let file = build_fragmented_file(
        &[vec![0x11; 64]],
        &sections,
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[FragSpec {
            track_id: 1,
            desc: Some(2),
            default_duration: None,
            default_size: None,
            base: FragBase::Patched,
            tfdt: Some(960),
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![FragSample {
                    data: vec![0xAA; 80],
                    duration: None,
                    size: Some(80),
                }],
            }],
        }],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[1].data.as_ref(), &[0xAA; 80]);
    assert_eq!(packets[1].pts, Timestamp::new(960));
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.runs[1].entry_idx, 1);
    assert_eq!(config.sample_table.sample_roll, vec![0, 5]);
}

#[test]
fn fragment_tfhd_defaults_without_trex() {
    // No `mvex`: `tfhd` default duration/size cover bare `trun` entries.
    // Mixed deltas also split the merged `stts` into two rows.
    let file = build_fragmented_file(
        &[vec![0x11; 64]],
        &[opus_obu_section(0)],
        &FileOpts::default(),
        None,
        &[FragSpec {
            track_id: 1,
            desc: Some(1),
            default_duration: Some(480),
            default_size: Some(32),
            base: FragBase::Patched,
            tfdt: None,
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![
                    FragSample {
                        data: vec![0x21; 32],
                        duration: None,
                        size: None,
                    },
                    FragSample {
                        data: vec![0x22; 32],
                        duration: None,
                        size: None,
                    },
                    FragSample {
                        data: vec![0x23; 32],
                        duration: None,
                        size: None,
                    },
                ],
            }],
        }],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 4);
    assert_eq!(packets[0].dur, Duration::new(960));
    for packet in packets.iter().skip(1) {
        assert_eq!(packet.dur, Duration::new(480));
    }
    assert_eq!(packets[3].pts, Timestamp::new(960 + 480 * 2));
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.stts, vec![(1, 960), (3, 480)]);
}

#[test]
fn fragment_two_moofs_implicit_and_moof_relative() {
    // FLAC needs no roll groups, so an empty init segment works: the
    // first `moof` chains two `trun`s through implicit continuation (the
    // second also carries tolerated flags plus a zero composition
    // offset), and the second `moof` uses `default-base-is-moof` with a
    // derived `data_offset`.
    let file = build_fragmented_file(
        &[],
        &[flac_section()],
        &FileOpts {
            with_roll: None,
            ..FileOpts::default()
        },
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[
            FragSpec {
                track_id: 1,
                desc: None,
                default_duration: None,
                default_size: None,
                base: FragBase::Patched,
                tfdt: None,
                empty: false,
                traf_sdtp: false,
                traf_sbgp: None,
                traf_sgpd_roll: None,
                traf_sgpd_raw: None,
                traf_saiz: false,
                truns: vec![
                    FragTrun {
                        data_offset: FragOffset::Implicit,
                        version: 0,
                        first_sample_flags: false,
                        cts: Vec::new(),
                        samples: vec![FragSample {
                            data: vec![0x11; 40],
                            duration: None,
                            size: Some(40),
                        }],
                    },
                    FragTrun {
                        data_offset: FragOffset::Implicit,
                        version: 0,
                        first_sample_flags: true,
                        cts: vec![0],
                        samples: vec![FragSample {
                            data: vec![0x22; 44],
                            duration: None,
                            size: Some(44),
                        }],
                    },
                ],
            },
            FragSpec {
                track_id: 1,
                desc: None,
                default_duration: None,
                default_size: None,
                base: FragBase::Moof,
                tfdt: Some(1920),
                empty: false,
                traf_sdtp: false,
                traf_sbgp: None,
                traf_sgpd_roll: None,
                traf_sgpd_raw: None,
                traf_saiz: false,
                truns: vec![FragTrun {
                    data_offset: FragOffset::FromMoofStart,
                    version: 0,
                    first_sample_flags: false,
                    cts: Vec::new(),
                    samples: vec![FragSample {
                        data: vec![0x33; 36],
                        duration: None,
                        size: Some(36),
                    }],
                }],
            },
        ],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].data.as_ref(), &[0x11; 40]);
    assert_eq!(packets[1].data.as_ref(), &[0x22; 44]);
    assert_eq!(packets[2].data.as_ref(), &[0x33; 36]);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(
            packet.pts,
            Timestamp::new(i64::try_from(i).expect("small index") * 960)
        );
    }
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.runs.len(), 3);
    assert_eq!(config.sample_table.sample_roll, [] as [i16; 0]);
}

#[test]
fn fragment_empty_traf_accepted() {
    // `duration-is-empty` fragments contribute no samples.
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let empty_traf = FragSpec {
        track_id: 1,
        desc: Some(1),
        default_duration: None,
        default_size: None,
        base: FragBase::Patched,
        tfdt: None,
        empty: true,
        traf_sdtp: false,
        traf_sbgp: None,
        traf_sgpd_roll: None,
        traf_sgpd_raw: None,
        traf_saiz: false,
        truns: Vec::new(),
    };
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[empty_traf],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].data.as_ref(), init[0].as_slice());
    assert_eq!(packets[1].data.as_ref(), init[1].as_slice());
}

#[test]
fn fragments_only_empty_init() {
    // Bare init segment: every sample arrives fragmented, without
    // `mvex` or `tfdt`.
    let file = build_fragmented_file(
        &[],
        &[flac_section()],
        &FileOpts {
            with_roll: None,
            ..FileOpts::default()
        },
        None,
        &[FragSpec {
            track_id: 1,
            desc: Some(1),
            default_duration: Some(960),
            default_size: None,
            base: FragBase::Patched,
            tfdt: None,
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![
                    FragSample {
                        data: vec![0x51; 60],
                        duration: None,
                        size: Some(60),
                    },
                    FragSample {
                        data: vec![0x52; 64],
                        duration: None,
                        size: Some(64),
                    },
                ],
            }],
        }],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(960));
    assert_eq!(packets[0].data.as_ref(), &[0x51; 60]);
    assert_eq!(packets[1].data.as_ref(), &[0x52; 64]);
}

#[test]
fn rejects_fragment_unknown_track() {
    let file = build_fragmented_file(
        &[vec![0x11; 64]],
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[FragSpec {
            track_id: 9,
            desc: Some(1),
            default_duration: Some(960),
            default_size: Some(16),
            base: FragBase::Patched,
            tfdt: None,
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![FragSample {
                    data: vec![0x11; 16],
                    duration: None,
                    size: None,
                }],
            }],
        }],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_fragment_bad_description_index() {
    let file = build_fragmented_file(
        &[vec![0x11; 64]],
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[FragSpec {
            track_id: 1,
            desc: Some(3),
            default_duration: None,
            default_size: None,
            base: FragBase::Patched,
            tfdt: None,
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![FragSample {
                    data: vec![0x11; 16],
                    duration: None,
                    size: Some(16),
                }],
            }],
        }],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_fragment_sample_past_eof() {
    let file = build_fragmented_file(
        &[vec![0x11; 64]],
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[FragSpec {
            track_id: 1,
            desc: None,
            default_duration: None,
            default_size: None,
            base: FragBase::Patched,
            tfdt: None,
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(i32::MAX),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![FragSample {
                    data: vec![0x11; 16],
                    duration: None,
                    size: Some(16),
                }],
            }],
        }],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_tfdt_discontinuity() {
    // Two init samples span 1920 units; a `tfdt` of 0 rewinds time.
    let file = build_fragmented_file(
        &[vec![0x11; 64], vec![0x22; 64]],
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[FragSpec {
            track_id: 1,
            desc: None,
            default_duration: None,
            default_size: None,
            base: FragBase::Patched,
            tfdt: Some(0),
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![FragSample {
                    data: vec![0x33; 16],
                    duration: None,
                    size: Some(16),
                }],
            }],
        }],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_trun_without_duration_default() {
    // Neither `trex` (all zeros) nor `tfhd` supplies a duration, and the
    // `trun` carries none per-sample.
    let file = build_fragmented_file(
        &[vec![0x11; 64]],
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 0,
            size: 0,
        }),
        &[FragSpec {
            track_id: 1,
            desc: None,
            default_duration: None,
            default_size: Some(16),
            base: FragBase::Patched,
            tfdt: None,
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Explicit(0),
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![FragSample {
                    data: vec![0x33; 16],
                    duration: None,
                    size: None,
                }],
            }],
        }],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_tfhd_without_base() {
    let file = build_fragmented_file(
        &[vec![0x11; 64]],
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&FragTrex {
            track_id: 1,
            desc: 1,
            duration: 960,
            size: 0,
        }),
        &[FragSpec {
            track_id: 1,
            desc: None,
            default_duration: None,
            default_size: None,
            base: FragBase::Missing,
            tfdt: None,
            empty: false,
            traf_sdtp: false,
            traf_sbgp: None,
            traf_sgpd_roll: None,
            traf_sgpd_raw: None,
            traf_saiz: false,
            truns: vec![FragTrun {
                data_offset: FragOffset::Implicit,
                version: 0,
                first_sample_flags: false,
                cts: Vec::new(),
                samples: vec![FragSample {
                    data: vec![0x33; 16],
                    duration: None,
                    size: Some(16),
                }],
            }],
        }],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

// ---------------------------------------------------------------------------
// Sync and composition offsets (M2-5)
// ---------------------------------------------------------------------------

#[test]
fn stss_valid_subset_accepted() {
    // Sync-ness never gates emission: all packets flow, and the
    // validated table is exposed on the track config.
    let opts = FileOpts {
        stss: Some(vec![1, 3]),
        ..FileOpts::default()
    };
    let file = build_file(&four_frames(), &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 4);
    assert_eq!(packets[3].pts, Timestamp::new(2880));
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.sync_samples, Some(vec![1, 3]));
}

#[test]
fn rejects_stss_unsorted() {
    let opts = FileOpts {
        stss: Some(vec![3, 1]),
        ..FileOpts::default()
    };
    let file = build_file(&four_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_stss_out_of_range() {
    let opts = FileOpts {
        stss: Some(vec![1, 5]),
        ..FileOpts::default()
    };
    let file = build_file(&four_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_stss_zero() {
    let opts = FileOpts {
        stss: Some(vec![0, 2]),
        ..FileOpts::default()
    };
    let file = build_file(&four_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_stss_empty() {
    let opts = FileOpts {
        stss: Some(Vec::new()),
        ..FileOpts::default()
    };
    let file = build_file(&four_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_stss_bad_version() {
    let opts = FileOpts {
        stss: Some(vec![1, 2, 3]),
        stss_version: 1,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn ctts_v0_shifts_pts() {
    let opts = FileOpts {
        ctts: Some(vec![(3, 480)]),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    for (i, packet) in packets.iter().enumerate() {
        assert_eq!(
            packet.pts,
            Timestamp::new(480 + i64::try_from(i).expect("small index") * 960)
        );
    }
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.sample_ctts, vec![480, 480, 480]);
}

#[test]
fn ctts_v1_signed_offsets() {
    // Negative offsets are representable: the first packet predates
    // its decode timestamp.
    let opts = FileOpts {
        ctts: Some(vec![(1, -960), (2, 0)]),
        ctts_version: 1,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].pts, Timestamp::new(-960));
    assert_eq!(packets[1].pts, Timestamp::new(960));
    assert_eq!(packets[2].pts, Timestamp::new(1920));
}

#[test]
fn rejects_ctts_count_mismatch() {
    let opts = FileOpts {
        ctts: Some(vec![(2, 0)]),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_ctts_bad_version() {
    let opts = FileOpts {
        ctts: Some(vec![(3, 0)]),
        ctts_version: 2,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

#[test]
fn ctts_all_zero_collapses() {
    let opts = FileOpts {
        ctts: Some(vec![(3, 0)]),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.sample_ctts, [] as [i64; 0]);
}

#[test]
fn elst_applies_before_ctts() {
    // Edit selection runs on decode timestamps; composition offsets
    // shift the resulting presentation timestamps.
    let frames = four_frames();
    let opts = FileOpts {
        ctts: Some(vec![(4, 100)]),
        edits: vec![media_edit(1920, 960)],
        ..FileOpts::default()
    };
    let file = build_file(&frames, &[opus_obu_section(0)], &opts);
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 2);
    assert_eq!(packets[0].pts, Timestamp::new(100));
    assert_eq!(packets[1].pts, Timestamp::new(1060));
}

/// `traf` spec with per-sample composition offsets and roll-group knobs.
fn cts_frag_spec(
    frag: &[Vec<u8>],
    version: u8,
    cts: Vec<i64>,
    sbgp: Option<TrafSbgp>,
    sgpd_roll: Option<Vec<i16>>,
    sdtp: bool,
) -> FragSpec {
    FragSpec {
        track_id: 1,
        desc: None,
        default_duration: None,
        default_size: None,
        base: FragBase::Patched,
        tfdt: Some(1920),
        empty: false,
        truns: vec![FragTrun {
            data_offset: FragOffset::Explicit(0),
            version,
            first_sample_flags: false,
            cts,
            samples: frag
                .iter()
                .map(|data| FragSample {
                    size: Some(u32::try_from(data.len()).expect("fixture fits in u32")),
                    duration: None,
                    data: data.clone(),
                })
                .collect(),
        }],
        traf_sdtp: sdtp,
        traf_sbgp: sbgp,
        traf_sgpd_roll: sgpd_roll,
        traf_sgpd_raw: None,
        traf_saiz: false,
    }
}

fn trex_960() -> FragTrex {
    FragTrex {
        track_id: 1,
        desc: 1,
        duration: 960,
        size: 0,
    }
}

#[test]
fn fragment_trun_ctts_applies() {
    // Init samples backfill zeros; fragment offsets shift their packets.
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(&frag, 0, vec![100, 200], None, None, false)],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 4);
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(960));
    assert_eq!(packets[2].pts, Timestamp::new(2020));
    assert_eq!(packets[3].pts, Timestamp::new(3080));
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.sample_ctts, vec![0, 0, 100, 200]);
}

#[test]
fn fragment_trun_ctts_v1_signed() {
    let init = vec![vec![0x11; 64]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let mut spec = cts_frag_spec(&frag, 1, vec![-480, 0], None, None, false);
    spec.tfdt = Some(960);
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[spec],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 3);
    assert_eq!(packets[0].pts, Timestamp::ZERO);
    assert_eq!(packets[1].pts, Timestamp::new(480));
    assert_eq!(packets[2].pts, Timestamp::new(1920));
}

#[test]
fn fragment_ctts_zero_fills_without_flag() {
    // `stbl` offsets plus a bare fragment `trun`: fragment samples get
    // zero offsets appended.
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let init_opts = FileOpts {
        ctts: Some(vec![(2, 100)]),
        ..FileOpts::default()
    };
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &init_opts,
        Some(&trex_960()),
        &[cts_frag_spec(&frag, 0, Vec::new(), None, None, false)],
    );
    let mut reader = open_reader(file);
    let packets = drain_packets(&mut reader);
    assert_eq!(packets.len(), 4);
    assert_eq!(packets[0].pts, Timestamp::new(100));
    assert_eq!(packets[1].pts, Timestamp::new(1060));
    assert_eq!(packets[2].pts, Timestamp::new(1920));
    assert_eq!(packets[3].pts, Timestamp::new(2880));
    let config = reader.track_config(1).expect("config");
    assert_eq!(config.sample_table.sample_ctts, vec![100, 100, 0, 0]);
}

#[test]
fn traf_roll_groups_validated() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(
            &frag,
            0,
            Vec::new(),
            Some((*b"roll", vec![(2, 1)])),
            Some(vec![0]),
            false,
        )],
    );
    let mut reader = open_reader(file);
    assert_eq!(drain_packets(&mut reader).len(), 4);
}

#[test]
fn rejects_traf_roll_mismatch() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(
            &frag,
            0,
            Vec::new(),
            Some((*b"roll", vec![(2, 1)])),
            Some(vec![5]),
            false,
        )],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_traf_roll_coverage() {
    // Grouping covers 1 of the 2 fragment samples.
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(
            &frag,
            0,
            Vec::new(),
            Some((*b"roll", vec![(1, 1)])),
            Some(vec![0]),
            false,
        )],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_traf_non_roll_group() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(
            &frag,
            0,
            Vec::new(),
            Some((*b"rap ", vec![(2, 1)])),
            None,
            false,
        )],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("non-roll sample groups (sgpd/sbgp) in fragment")
    ));
}

#[test]
fn rejects_traf_sdtp() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(&frag, 0, Vec::new(), None, None, true)],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("sdtp sample dependencies in fragment")
    ));
}

#[test]
fn rejects_traf_roll_missing_descriptions() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(
            &frag,
            0,
            Vec::new(),
            Some((*b"roll", vec![(2, 1)])),
            None,
            false,
        )],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_trun_bad_version() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(&frag, 2, Vec::new(), None, None, false)],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(_)
    ));
}

// ---------------------------------------------------------------------------
// Protection reporting (M2-6)
// ---------------------------------------------------------------------------

/// A CENC `enca` entry wrapping an `iamf` original.
fn cenc_enca() -> EncaSpec {
    EncaSpec {
        omit_sinf: false,
        scheme: Some((*b"cenc", 1)),
        original: Some(*b"iamf"),
        schm_version: 0,
    }
}

#[test]
fn rejects_enca_cenc_reports_scheme() {
    // The protected entry sits second: every entry is inspected, and
    // the rejection names the scheme, version, and original format.
    let opts = FileOpts {
        enca_entries: vec![cenc_enca()],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(
            "iamf: protected IAMF track: scheme cenc v1, original format iamf"
        )
    ));
}

#[test]
fn rejects_enca_cbcs_audio() {
    let mut spec = cenc_enca();
    spec.scheme = Some((*b"cbcs", 1));
    let opts = FileOpts {
        enca_entries: vec![spec],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported(
            "iamf: protected IAMF track: scheme cbcs v1, original format iamf"
        )
    ));
}

#[test]
fn rejects_enca_missing_sinf() {
    let mut spec = cenc_enca();
    spec.omit_sinf = true;
    let opts = FileOpts {
        enca_entries: vec![spec],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_enca_missing_scheme() {
    let mut spec = cenc_enca();
    spec.scheme = None;
    let opts = FileOpts {
        enca_entries: vec![spec],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_enca_missing_frma() {
    let mut spec = cenc_enca();
    spec.original = None;
    let opts = FileOpts {
        enca_entries: vec![spec],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::DecodeError(_)
    ));
}

#[test]
fn rejects_enca_bad_schm_version() {
    let mut spec = cenc_enca();
    spec.schm_version = 1;
    let opts = FileOpts {
        enca_entries: vec![spec],
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("schm version")
    ));
}

#[test]
fn rejects_seig_sample_groups() {
    let opts = FileOpts {
        with_roll: None,
        sbgp_grouping: Some(*b"seig"),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("encrypted samples (seig sample groups) in IAMF track")
    ));
}

#[test]
fn rejects_seig_descriptions() {
    let opts = FileOpts {
        with_roll: None,
        sgpd_grouping: Some((*b"seig", Vec::new())),
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("encrypted samples (seig sample groups) in IAMF track")
    ));
}

#[test]
fn rejects_saiz_saio() {
    let opts = FileOpts {
        with_saiz_saio: true,
        ..FileOpts::default()
    };
    let file = build_file(&opus_frames(), &[opus_obu_section(0)], &opts);
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("encrypted sample auxiliary info (saiz/saio) in IAMF track")
    ));
}

#[test]
fn rejects_traf_seig() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[cts_frag_spec(
            &frag,
            0,
            Vec::new(),
            Some((*b"seig", vec![(2, 1)])),
            None,
            false,
        )],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("encrypted samples (seig sample groups) in fragment")
    ));
}

#[test]
fn rejects_traf_saiz() {
    let init = vec![vec![0x11; 64], vec![0x22; 128]];
    let frag = vec![vec![0x33; 96], vec![0x44; 48]];
    let spec = FragSpec {
        traf_saiz: true,
        ..cts_frag_spec(&frag, 0, Vec::new(), None, None, false)
    };
    let file = build_fragmented_file(
        &init,
        &[opus_obu_section(0)],
        &FileOpts::default(),
        Some(&trex_960()),
        &[spec],
    );
    assert!(matches!(
        open_expect_error(file),
        SymphoniaError::Unsupported("encrypted sample auxiliary info (saiz/saio) in fragment")
    ));
}

// ---------------------------------------------------------------------------
// Codecs-string metadata (M2-8)
// ---------------------------------------------------------------------------

#[test]
fn codecs_string_per_codec() {
    // (fourcc tag, decoder_config bytes, expected string), per IAMF §6.4.
    let cases: &[([u8; 4], Vec<u8>, &str)] = &[
        (
            *b"Opus",
            vec![1, 2, 0x38, 0x01, 0, 0, 0xBB, 0x80, 0, 0, 0],
            "iamf.000.000.Opus",
        ),
        (*b"mp4a", vec![0x12, 0x10], "iamf.000.000.mp4a.40.2"),
        (*b"mp4a", vec![0x2A, 0x10], "iamf.000.000.mp4a.40.5"),
        (*b"mp4a", vec![0xF8, 0x00], "iamf.000.000.mp4a.40.32"),
        (*b"mp4a", Vec::new(), "iamf.000.000.mp4a.40.2"),
        (*b"fLaC", vec![0x10, 0, 0x22, 0, 0], "iamf.000.000.fLaC"),
        (*b"ipcm", vec![0, 16, 0, 0, 0xBB, 0x80], "iamf.000.000.ipcm"),
    ];
    for (tag, decoder_config, expected) in cases {
        let section = custom_cc_section(*tag, decoder_config, 0);
        let file = build_file(
            &opus_frames(),
            std::slice::from_ref(&section),
            &FileOpts::default(),
        );
        let reader = open_reader(file);
        let config = reader.track_config(1).expect("config");
        assert_eq!(
            config.codecs_string().as_str(),
            *expected,
            "tag {:?}",
            String::from_utf8_lossy(&tag[..])
        );
    }
}

#[test]
fn codecs_string_profiles() {
    for (primary, additional, expected) in [
        (1u8, 5u8, "iamf.001.005.Opus"),
        (255, 255, "iamf.255.255.Opus"),
    ] {
        let section = opus_obu_section_full(0, 0, primary, additional);
        let file = build_file(
            &opus_frames(),
            std::slice::from_ref(&section),
            &FileOpts::default(),
        );
        let reader = open_reader(file);
        let config = reader.track_config(1).expect("config");
        assert_eq!(config.codecs_string().as_str(), expected);
    }
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
        .join("../symphonia-iamf-core/tests/data");
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

/// Split a corpus stream's temporal units into per-frame payloads.
/// Returns the frames plus whether the split ran clean to the end
/// (Opus references interleave redundant copies the splitter rejects).
fn split_corpus_samples(
    name: &str,
    raw: &[u8],
    temporal_offset: usize,
    desc: &symphonia_iamf_core::obu::parser::IamfDescriptors,
) -> (Vec<Vec<u8>>, bool) {
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
    (samples, split_ok)
}

fn roundtrip_corpus(name: &str, raw: &[u8], byte_exact: bool) {
    let (desc, temporal_offset) = parse_descriptors(raw).expect("corpus parses");
    assert!(!desc.codec_configs.is_empty(), "{name}: no codec configs");

    // Descriptor prefix becomes the iacb OBU section verbatim.
    let obu_section = &raw[..temporal_offset];
    let delta = desc.codec_configs[0].num_samples_per_frame.max(1);

    // Split temporal units into per-frame samples in file order.
    let (mut samples, split_ok) = split_corpus_samples(name, raw, temporal_offset, &desc);
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
    // Mirror the entry's codec selection for roll groups: single config
    // wins, else the first audio element's reference.
    let selected_id = if desc.codec_configs.len() == 1 {
        desc.codec_configs[0].codec_config_id
    } else {
        desc.audio_elements
            .first()
            .expect("corpus has audio elements")
            .codec_config_id
    };
    let selected = desc
        .codec_configs
        .iter()
        .find(|c| c.codec_config_id == selected_id)
        .expect("corpus references known config");
    let needs_roll = matches!(
        selected.codec_id,
        symphonia_iamf_core::types::CodecId::Opus | symphonia_iamf_core::types::CodecId::AacLc
    );
    let opts = FileOpts {
        deltas: vec![delta; frames.len()],
        with_roll: needs_roll.then_some(selected.audio_roll_distance),
        ..FileOpts::default()
    };
    let sections = vec![obu_section.to_vec()];
    let file = build_file(&frames, &sections, &opts);
    let mut reader = open_reader(file);

    // Track parameters must reflect the real codec config.
    let codec = reader.track_config(1).expect("config").codec_config.clone();
    assert_eq!(
        codec.codec_config_id, desc.codec_configs[0].codec_config_id,
        "{name}"
    );
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
