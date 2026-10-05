use super::misc::bounded_capacity;
use super::obu_type::ObuType;
use super::parse::parse_audio_element;
use super::parse::parse_mix_presentation;
use super::parse::parse_obu_header;
use super::parse::parse_parameter_block;
use super::parse::parse_parameter_block_with_kind;
use super::parse::parse_sequence_header;
use super::read::read_leb128;
use crate::error::IamfError;
use crate::types::*;
use std::collections::HashMap;

#[test]
fn test_leb128_single_byte() {
    let (val, consumed) = read_leb128(&[42]).unwrap();
    assert_eq!(val, 42);
    assert_eq!(consumed, 1);
}

#[test]
fn test_leb128_multi_byte() {
    // 300 = 0b100101100 -> leb128: [0xAC, 0x02]
    let (val, consumed) = read_leb128(&[0xAC, 0x02]).unwrap();
    assert_eq!(val, 300);
    assert_eq!(consumed, 2);
}

#[test]
fn test_leb128_zero() {
    let (val, consumed) = read_leb128(&[0]).unwrap();
    assert_eq!(val, 0);
    assert_eq!(consumed, 1);
}

#[test]
fn test_obu_type_from_u8() {
    assert_eq!(ObuType::from_u8(0).unwrap(), ObuType::CodecConfig);
    assert_eq!(ObuType::from_u8(1).unwrap(), ObuType::AudioElement);
    assert_eq!(ObuType::from_u8(2).unwrap(), ObuType::MixPresentation);
    assert_eq!(ObuType::from_u8(4).unwrap(), ObuType::TemporalDelimiter);
    assert_eq!(ObuType::from_u8(5).unwrap(), ObuType::AudioFrame);
    assert_eq!(ObuType::from_u8(6).unwrap(), ObuType::AudioFrameId(0));
    assert_eq!(ObuType::from_u8(31).unwrap(), ObuType::SequenceHeader);
    ObuType::from_u8(25).unwrap_err();
}

#[test]
fn test_parse_sequence_header() {
    let data = [b'i', b'a', b'm', b'f', 0, 0]; // Simple profile
    let (primary, additional) = parse_sequence_header(&data).unwrap();
    assert_eq!(primary, 0);
    assert_eq!(additional, 0);
}

#[test]
fn test_parse_sequence_header_invalid_magic() {
    let data = [b'x', b'x', b'x', b'x', 0, 0];
    assert!(matches!(
        parse_sequence_header(&data),
        Err(IamfError::InvalidMagic)
    ));
}

#[test]
fn test_obu_header_minimal() {
    // OBU type = 31 (SequenceHeader), no flags, size = 6
    let obu_type_byte = 31 << 3; // 0xF8
    let data = [obu_type_byte, 6, b'i', b'a', b'm', b'f', 0, 0];
    let (header, header_size) = parse_obu_header(&data).unwrap();
    assert_eq!(header.obu_type, ObuType::SequenceHeader);
    assert!(!header.redundant_copy);
    assert!(!header.trimming_status);
    assert!(!header.extension_flag);
    assert_eq!(header.payload_size, 6);
    assert_eq!(header_size, 2);
}

/// Build a minimal MixGain-style `parameter_block` payload:
/// `parameter_id` (leb128) + duration (leb128) + `constant_subblock_duration`
/// (leb128) + 1 subblock with `animation_type=Step` + `start_value` (Q7.8).
fn build_param_block_payload(parameter_id: u8) -> Vec<u8> {
    vec![
        parameter_id, // parameter_id (leb128, <128 so 1 byte)
        10,           // duration
        10,           // constant_subblock_duration (==duration => 1 subblock)
        0,            // animation_type=Step (low 3 bits)
        0x10,
        0x00, // start_point_value = 0x1000 i16 BE = 4096/256 = 16.0 dB
    ]
}

/// `DemixingInfo` block carries `dmixp_mode (3) + reserved (5)` — only 1 byte
/// of payload. If the parser silently treats it as `MixGain` it consumes
/// 3 extra bytes (`animation_byte` + i16 start) and corrupts the gain.
/// The fix dispatches on the parameter kind from the descriptor.
#[test]
fn parameter_block_demixing_info_not_silently_mix_gain() {
    // Build a demixing-info payload: parameter_id=7, duration=10, csd=10,
    // then one byte: dmixp_mode=2 (top 3 bits = 010_00000 = 0x40).
    let payload: Vec<u8> = vec![
        7,    // parameter_id
        10,   // duration
        10,   // constant_subblock_duration
        0x40, // dmixp_mode=2, reserved=0
    ];

    // With kind=DemixingInfo, the parse succeeds and emits DemixingInfo.
    let mut kinds = HashMap::new();
    kinds.insert(7u32, ParameterDataKind::DemixingInfo);
    let no_recon: HashMap<u32, ReconGainLayout> = HashMap::new();
    let pb = parse_parameter_block_with_kind(&payload, &kinds, &no_recon)
        .expect("demixing-info parameter block must parse");
    assert_eq!(pb.parameter_id, 7);
    assert_eq!(pb.subblocks.len(), 1);
    match &pb.subblocks[0].param_data {
        ParameterData::DemixingInfo { dmixp_mode } => {
            assert_eq!(*dmixp_mode, 2, "dmixp_mode should be 2");
        }
        other => panic!("expected DemixingInfo, got {other:?}"),
    }

    // With no kind hint, the legacy parser would try to read 3 extra
    // bytes (animation + i16) — the payload is too short, so it errors
    // out instead of silently producing a garbage MixGain.
    let legacy = parse_parameter_block(&payload);
    assert!(
        legacy.is_err(),
        "DemixingInfo payload must NOT silently decode as MixGain"
    );
}

/// `MixGain` payload still parses correctly under the new dispatch.
#[test]
fn parameter_block_mix_gain_still_parses() {
    let payload = build_param_block_payload(3);
    let mut kinds = HashMap::new();
    kinds.insert(3u32, ParameterDataKind::MixGain);
    let no_recon: HashMap<u32, ReconGainLayout> = HashMap::new();
    let pb = parse_parameter_block_with_kind(&payload, &kinds, &no_recon).unwrap();
    assert_eq!(pb.parameter_id, 3);
    assert_eq!(pb.subblocks.len(), 1);
    match &pb.subblocks[0].param_data {
        ParameterData::MixGain {
            start_point_value, ..
        } => {
            assert!((start_point_value - 16.0).abs() < 1e-3);
        }
        other => panic!("expected MixGain, got {other:?}"),
    }
}

/// `ReconGain` payloads are consumed per the v1.1.0 bit layout: for each
/// layer with `recon_gain_is_present`, a leb128 flags bitmask followed by one
/// u8 per set bit (gain = byte / 255).
#[test]
fn parameter_block_recon_gain_parses_values() {
    // 2 layers, both present. Layer 0: flags=0b101 -> gains for b0, b2.
    // Layer 1: flags=0 -> no gain bytes.
    let payload = vec![
        9u8,  // parameter_id
        10,   // duration
        10,   // constant_subblock_duration
        0x05, // layer 0 flags: b0 + b2
        200,  // gain b0 = 200/255
        255,  // gain b2 = 1.0
        0x00, // layer 1 flags: none
    ];
    let mut kinds = HashMap::new();
    kinds.insert(9u32, ParameterDataKind::ReconGain);
    let recon = HashMap::from([(
        9u32,
        ReconGainLayout {
            num_layers: 2,
            layers_present: vec![true, true],
        },
    )]);
    let pb = parse_parameter_block_with_kind(&payload, &kinds, &recon).unwrap();
    assert_eq!(pb.subblocks.len(), 1);
    match &pb.subblocks[0].param_data {
        ParameterData::ReconGain { layers } => {
            assert_eq!(layers.len(), 2);
            let l0 = layers[0].as_ref().expect("layer 0 present");
            assert_eq!(l0.flags, 0x05);
            assert_eq!(l0.gains.len(), 2);
            assert!((l0.gains[0] - 200.0 / 255.0).abs() < 1e-6);
            assert!((l0.gains[1] - 1.0).abs() < 1e-6);
            assert!(layers[1].is_none() || layers[1].as_ref().unwrap().gains.is_empty());
        }
        other => panic!("expected ReconGain, got {other:?}"),
    }
}

/// Layers without `recon_gain_is_present` consume no bytes.
#[test]
fn parameter_block_recon_gain_skips_absent_layers() {
    // Layer 0 absent, layer 1 present with flags=0x01 + 1 gain byte.
    let payload = vec![9u8, 10, 10, 0x01, 128];
    let mut kinds = HashMap::new();
    kinds.insert(9u32, ParameterDataKind::ReconGain);
    let recon = HashMap::from([(
        9u32,
        ReconGainLayout {
            num_layers: 2,
            layers_present: vec![false, true],
        },
    )]);
    let pb = parse_parameter_block_with_kind(&payload, &kinds, &recon).unwrap();
    match &pb.subblocks[0].param_data {
        ParameterData::ReconGain { layers } => {
            assert_eq!(layers.len(), 2);
            assert!(layers[0].is_none());
            let l1 = layers[1].as_ref().expect("layer 1 present");
            assert_eq!(l1.flags, 0x01);
            assert_eq!(l1.gains.len(), 1);
        }
        other => panic!("expected ReconGain, got {other:?}"),
    }
}

/// Flag bits beyond the 12 defined channels are rejected, and truncated
/// payloads fail instead of yielding partial gains.
#[test]
fn parameter_block_recon_gain_rejects_bad_payloads() {
    let mut kinds = HashMap::new();
    kinds.insert(9u32, ParameterDataKind::ReconGain);
    let recon = HashMap::from([(
        9u32,
        ReconGainLayout {
            num_layers: 1,
            layers_present: vec![true],
        },
    )]);
    // flags = 0x1000 (bit 12): outside the 12 defined channels.
    let bad_flags = vec![9u8, 10, 10, 0x80, 0x20];
    parse_parameter_block_with_kind(&bad_flags, &kinds, &recon).unwrap_err();
    // flags promise 2 gains but only 1 byte follows.
    let truncated = vec![9u8, 10, 10, 0x03, 100];
    parse_parameter_block_with_kind(&truncated, &kinds, &recon).unwrap_err();
}

/// `ReconGain` without a descriptor layout cannot be sized, so it errors
/// instead of emitting empty gains.
#[test]
fn parameter_block_recon_gain_without_layout_errors() {
    let payload = vec![9u8, 10, 10, 0x01, 100];
    let mut kinds = HashMap::new();
    kinds.insert(9u32, ParameterDataKind::ReconGain);
    let empty: HashMap<u32, ReconGainLayout> = HashMap::new();
    parse_parameter_block_with_kind(&payload, &kinds, &empty).unwrap_err();
}

/// `bounded_capacity` must reject leb128 counts greater than the byte
/// ceiling, the absolute max, or both.
#[test]
fn bounded_capacity_rejects_unbounded_values() {
    // 100 elements but only 5 bytes remain.
    bounded_capacity(100, 5).unwrap_err();
    // Above MAX_LEB128_CAPACITY.
    bounded_capacity(u32::MAX, 1024 * 1024 * 1024).unwrap_err();
    // Reasonable counts pass.
    assert_eq!(bounded_capacity(10, 1024).unwrap(), 10);
    assert_eq!(bounded_capacity(0, 0).unwrap(), 0);
}

/// Adversarial `audio_element` with a huge `num_substreams` leb128 must be
/// rejected before the allocator is asked for gigabytes.
#[test]
fn parse_audio_element_rejects_unbounded_leb128_substreams() {
    // audio_element_id=0, type_byte=Channel(0), codec_config_id=0,
    // then num_substreams = 0xFFFFFFFF (leb128 5-byte encoding).
    let mut payload = vec![
        0u8, // audio_element_id
        0,   // type byte (top 3 bits = element_type = 0 = Channel)
        0,   // codec_config_id
    ];
    // 0xFFFFFFFF leb128 = 0xff,0xff,0xff,0xff,0x0f
    payload.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0x0f]);
    // A few trailing bytes — far less than 4 billion.
    payload.extend_from_slice(&[0u8; 16]);

    let err =
        parse_audio_element(&payload).expect_err("must reject 4G substreams against tiny payload");
    match err {
        IamfError::ParseError(msg) => {
            assert!(
                msg.contains("leb128 capacity") || msg.contains("remaining"),
                "unexpected error message: {msg}"
            );
        }
        other => panic!("expected ParseError, got {other:?}"),
    }
}

/// Unsigned LEB128 writer for fixtures.
fn leb(mut value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn mix_gain_config(parameter_id: u32) -> Vec<u8> {
    let mut out = leb(parameter_id);
    out.extend(leb(48000)); // parameter_rate
    out.push(0x80); // param_definition_mode = 1 (no durations)
    out.extend_from_slice(&0i16.to_be_bytes()); // unity gain
    out
}

/// One mix-presentation OBU payload with a single sub-mix and the given
/// raw layout bytes (each followed by a fixed loudness record).
fn mix_presentation_layouts(layouts: &[u8]) -> Vec<u8> {
    let mut mp = leb(0); // mix_presentation_id
    mp.extend(leb(0)); // count_label
    mp.extend(leb(1)); // num_sub_mixes
    mp.extend(leb(1)); // num_audio_elements
    mp.extend(leb(0)); // audio_element_id
    mp.push(0x00); // headphones rendering mode
    mp.extend(leb(0)); // rendering extension size
    mp.extend(mix_gain_config(10));
    mp.extend(mix_gain_config(11));
    mp.extend(leb(
        u32::try_from(layouts.len()).expect("fixture holds few layouts")
    )); // num_layouts
    for &layout in layouts {
        mp.push(layout);
        mp.extend_from_slice(&[0x00, 0xE9, 0x00, 0xFF, 0x00]); // loudness
    }
    mp
}

#[test]
fn unknown_bs2051_sound_system_is_skipped() {
    // System 7 has no IAMF equivalent: the layout is dropped (its
    // loudness still consumed), and the following stereo layout drives
    // output. 0x9C = type 2 + system 7; 0x80 = type 2 + system 0.
    let mp = parse_mix_presentation(&mix_presentation_layouts(&[0x9C, 0x80]))
        .expect("unknown systems are accepted");
    let sub = &mp.sub_mixes[0];
    assert_eq!(sub.layouts.len(), 1);
    assert_eq!(sub.layouts[0].layout, IamfChannelLayout::Stereo);
    assert_eq!(sub.output_layout, IamfChannelLayout::Stereo);
}

#[test]
fn all_unknown_bs2051_layouts_fall_back_to_stereo() {
    // 0xB4 = type 2 + system 13.
    let mp = parse_mix_presentation(&mix_presentation_layouts(&[0x9C, 0xB4]))
        .expect("unknown systems are accepted");
    let sub = &mp.sub_mixes[0];
    assert!(sub.layouts.is_empty());
    assert_eq!(sub.output_layout, IamfChannelLayout::Stereo);
}
