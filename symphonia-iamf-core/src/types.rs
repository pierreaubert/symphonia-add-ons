// ============================================================================
// IAMF Type Definitions
// ============================================================================
//
// Core types for the IAMF (Immersive Audio Model and Formats) decoder.
// Based on IAMF v1.1.0 specification.

/// IAMF codec identifiers (4-byte codec_id field)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    Opus,
    AacLc,
    Flac,
    Lpcm,
}

impl CodecId {
    pub fn from_bytes(bytes: [u8; 4]) -> Option<Self> {
        match &bytes {
            b"Opus" => Some(Self::Opus),
            b"mp4a" => Some(Self::AacLc),
            b"fLaC" => Some(Self::Flac),
            b"ipcm" => Some(Self::Lpcm),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Opus => "Opus",
            Self::AacLc => "AAC-LC",
            Self::Flac => "FLAC",
            Self::Lpcm => "LPCM",
        }
    }
}

/// Codec configuration descriptor (parsed from codec_config OBU)
#[derive(Debug, Clone)]
pub struct CodecConfig {
    pub codec_config_id: u32,
    pub codec_id: CodecId,
    pub num_samples_per_frame: u32,
    pub audio_roll_distance: i16,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub decoder_config: Vec<u8>,
}

/// Audio element type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioElementType {
    /// Channel-based audio (e.g. 5.1, 7.1.4)
    Channel = 0,
    /// Scene-based audio (Ambisonics/HOA)
    Scene = 1,
}

/// Ambisonics mode for scene-based elements
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmbisonicsMode {
    /// Mono: each ACN channel is a separate substream
    Mono = 0,
    /// Projection: substreams are projected (mixed) channels
    Projection = 1,
}

/// IAMF channel layout (loudspeaker_layout field)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IamfChannelLayout {
    Mono,
    Stereo,
    Layout5_1,
    Layout5_1_2,
    Layout5_1_4,
    Layout7_1,
    Layout7_1_2,
    Layout7_1_4,
    Layout3_1_2,
    Binaural,
}

impl IamfChannelLayout {
    pub fn from_layout_index(idx: u8) -> Option<Self> {
        match idx {
            0 => Some(Self::Mono),
            1 => Some(Self::Stereo),
            2 => Some(Self::Layout5_1),
            3 => Some(Self::Layout5_1_2),
            4 => Some(Self::Layout5_1_4),
            5 => Some(Self::Layout7_1),
            6 => Some(Self::Layout7_1_2),
            7 => Some(Self::Layout7_1_4),
            8 => Some(Self::Layout3_1_2),
            9 => Some(Self::Binaural),
            _ => None,
        }
    }

    /// ITU-R BS.2051 Sound System index (mix presentation loudness
    /// layouts, v1.1.0 §3.7) → layout. Systems with no IAMF loudspeaker
    /// equivalent (3, 5, 6, 7, 9, 13, 14, 15) are `None`; the mix
    /// presentation parser drops those layouts, falling back to stereo
    /// when none remain.
    pub fn from_sound_system(idx: u8) -> Option<Self> {
        match idx {
            0 => Some(Self::Stereo),
            1 => Some(Self::Layout5_1),
            2 => Some(Self::Layout5_1_2),
            4 => Some(Self::Layout5_1_4),
            8 => Some(Self::Layout7_1),
            10 => Some(Self::Layout7_1_2),
            11 => Some(Self::Layout3_1_2),
            12 => Some(Self::Mono),
            _ => None,
        }
    }

    pub fn channel_count(&self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo | Self::Binaural => 2,
            Self::Layout3_1_2 => 6,
            Self::Layout5_1 => 6,
            Self::Layout5_1_2 => 8,
            Self::Layout5_1_4 => 10,
            Self::Layout7_1 => 8,
            Self::Layout7_1_2 => 10,
            Self::Layout7_1_4 => 12,
        }
    }

    /// Map to SotF speaker config ID
    pub fn to_speaker_config_id(&self) -> Option<&'static str> {
        match self {
            Self::Mono => Some("1.0"),
            Self::Stereo | Self::Binaural => Some("2.0"),
            Self::Layout5_1 => Some("5.1"),
            Self::Layout5_1_2 => Some("5.1.2"),
            Self::Layout5_1_4 => Some("5.1.4"),
            Self::Layout7_1 => Some("7.1"),
            Self::Layout7_1_2 => Some("7.1.2"),
            Self::Layout7_1_4 => Some("7.1.4"),
            Self::Layout3_1_2 => None, // No direct SotF equivalent
        }
    }
}

/// Scalable channel audio configuration (for channel-based elements)
#[derive(Debug, Clone)]
pub struct ScalableChannelConfig {
    pub num_layers: u8,
    pub layers: Vec<ChannelLayer>,
}

/// A single layer in a scalable channel configuration
#[derive(Debug, Clone)]
pub struct ChannelLayer {
    pub loudspeaker_layout: IamfChannelLayout,
    pub output_gain_is_present: bool,
    pub recon_gain_is_present: bool,
    pub substream_count: u8,
    pub coupled_substream_count: u8,
    /// 6-bit per-channel mask selecting which mixed channels of this layer
    /// receive `output_gain_db`. Bit order follows the gain-channel order
    /// L, R, Ls, Rs, Ltf, Rtf resolved against the layer layout.
    pub output_gain_flags: u8,
    pub output_gain_db: f32,
}

/// Ambisonics configuration (for scene-based elements)
#[derive(Debug, Clone)]
pub struct AmbisonicsConfig {
    pub ambisonics_mode: AmbisonicsMode,
    pub output_channel_count: u8,
    pub substream_count: u8,
    pub coupled_substream_count: u8,
    /// ACN-to-substream channel mapping
    pub channel_mapping: Vec<u8>,
    /// Demixing matrix for projection mode [output_ch × coupled*2 + uncoupled]
    pub demixing_matrix: Vec<f32>,
}

/// Audio element descriptor (parsed from audio_element OBU)
#[derive(Debug, Clone)]
pub struct AudioElement {
    pub audio_element_id: u32,
    pub element_type: AudioElementType,
    pub codec_config_id: u32,
    pub num_substreams: u32,
    pub substream_ids: Vec<u32>,
    pub element_config: ElementConfig,
    pub parameter_definitions: Vec<ParameterDefinition>,
}

/// Element-specific configuration
#[derive(Debug, Clone)]
pub enum ElementConfig {
    Channel(ScalableChannelConfig),
    Scene(AmbisonicsConfig),
}

/// Parameter definition within an audio element
#[derive(Debug, Clone)]
pub struct ParameterDefinition {
    pub parameter_id: u32,
    pub parameter_rate: u32,
    pub param_definition_mode: bool,
    pub duration: u32,
    pub constant_subblock_duration: u32,
    /// Parameter payload kind (`parameter_definition_type` field in the
    /// audio_element OBU): MixGain / DemixingInfo / ReconGain.
    pub parameter_kind: ParameterDataKind,
    /// Default `dmixp_mode` from `DefaultDemixingInfoParameterData`.
    /// Present only for Demixing definitions; applies until per-frame
    /// parameter blocks arrive.
    pub default_dmixp_mode: Option<u8>,
    /// Default weight index from `DefaultDemixingInfoParameterData`
    /// (`default_w`, 4 bits). Directly indicates w(k) when no DemixingInfo
    /// parameter blocks exist.
    pub default_w: Option<u8>,
}

/// Demixing parameters for one audio frame, selected by `dmixp_mode`
/// (v1.1.0 §3.8.2). α/β feed the S7to5 de-mixer, γ the T4to2 de-mixer,
/// δ the S5to3 de-mixer; `w_offset` steps the wIdx state machine.
#[derive(Debug, Clone, Copy)]
pub struct DmixParams {
    pub alpha: f32,
    pub beta: f32,
    pub gamma: f32,
    pub delta: f32,
    pub w_offset: i8,
}

impl DmixParams {
    /// Parameters for a `dmixp_mode` value. Modes 3 and 7 are reserved.
    pub fn for_mode(mode: u8) -> Option<Self> {
        match mode {
            0 => Some(Self {
                alpha: 1.0,
                beta: 1.0,
                gamma: 0.707,
                delta: 0.707,
                w_offset: -1,
            }),
            1 => Some(Self {
                alpha: 0.707,
                beta: 0.707,
                gamma: 0.707,
                delta: 0.707,
                w_offset: -1,
            }),
            2 => Some(Self {
                alpha: 1.0,
                beta: 0.866,
                gamma: 0.866,
                delta: 0.866,
                w_offset: -1,
            }),
            4 => Some(Self {
                alpha: 1.0,
                beta: 1.0,
                gamma: 0.707,
                delta: 0.707,
                w_offset: 1,
            }),
            5 => Some(Self {
                alpha: 0.707,
                beta: 0.707,
                gamma: 0.707,
                delta: 0.707,
                w_offset: 1,
            }),
            6 => Some(Self {
                alpha: 1.0,
                beta: 0.866,
                gamma: 0.866,
                delta: 0.866,
                w_offset: 1,
            }),
            _ => None,
        }
    }
}

/// What kind of payload a parameter block carries. Determines how
/// `parse_parameter_block` decodes each subblock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParameterDataKind {
    MixGain,
    DemixingInfo,
    ReconGain,
}

impl ParameterDataKind {
    /// IAMF v1.1.0 §3.6.4 parameter_definition_type values.
    pub fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::MixGain),
            1 => Some(Self::DemixingInfo),
            2 => Some(Self::ReconGain),
            _ => None,
        }
    }
}

/// Mix presentation descriptor
#[derive(Debug, Clone)]
pub struct MixPresentation {
    pub mix_presentation_id: u32,
    pub annotations: Vec<MixAnnotation>,
    pub sub_mixes: Vec<SubMix>,
}

/// Language-tagged annotation
#[derive(Debug, Clone)]
pub struct MixAnnotation {
    pub language: String,
    pub label: String,
}

/// One loudness layout of a sub-mix (v1.1.0 §3.7): a BS.2051 sound
/// system or binaural layout plus the loudness measured for it.
#[derive(Debug, Clone)]
pub struct SubMixLayout {
    pub layout: IamfChannelLayout,
    pub loudness: LoudnessInfo,
}

/// Sub-mix within a mix presentation
#[derive(Debug, Clone)]
pub struct SubMix {
    pub num_audio_elements: u32,
    pub element_mix_configs: Vec<ElementMixConfig>,
    pub output_mix_gain: MixGainConfig,
    /// All declared loudness layouts (v1.1 allows several per sub-mix).
    pub layouts: Vec<SubMixLayout>,
    /// Render target: the first declared layout (Stereo fallback when the
    /// bitstream declares none, mirroring the reference decoder).
    pub output_layout: IamfChannelLayout,
    /// Loudness of the first declared layout.
    pub loudness: LoudnessInfo,
}

/// Per-element mix configuration
#[derive(Debug, Clone)]
pub struct ElementMixConfig {
    pub audio_element_id: u32,
    pub mix_gain: MixGainConfig,
}

/// Mix gain configuration
#[derive(Debug, Clone)]
pub struct MixGainConfig {
    pub parameter_id: u32,
    pub default_mix_gain_db: f32,
}

/// Loudness measurement information
#[derive(Debug, Clone)]
pub struct LoudnessInfo {
    pub info_type: u8,
    pub integrated_loudness: f32,
    pub digital_peak: f32,
    pub true_peak: Option<f32>,
}

/// Parameter block (time-varying parameter)
#[derive(Debug, Clone)]
pub struct ParameterBlock {
    pub parameter_id: u32,
    pub duration: u32,
    pub constant_subblock_duration: u32,
    pub subblocks: Vec<ParameterSubblock>,
}

/// Recon gains for one scalable-channel layer, in `recon_gain_flags`
/// bit order (b0 = L … b11 = LFE). Each gain is linear (`byte / 255`).
#[derive(Debug, Clone)]
pub struct ReconLayerGains {
    pub flags: u32,
    pub gains: Vec<f32>,
}

/// Descriptor-side context needed to size a ReconGain payload: the owning
/// audio element's layer count plus per-layer `recon_gain_is_present`.
#[derive(Debug, Clone)]
pub struct ReconGainLayout {
    pub num_layers: u8,
    pub layers_present: Vec<bool>,
}

/// A subblock within a parameter block
#[derive(Debug, Clone)]
pub struct ParameterSubblock {
    pub subblock_duration: u32,
    pub param_data: ParameterData,
}

/// Parameter data variants
#[derive(Debug, Clone)]
pub enum ParameterData {
    MixGain {
        animation_type: AnimationType,
        start_point_value: f32,
        end_point_value: f32,
        control_point_value: f32,
        control_point_relative_time: f32,
    },
    DemixingInfo {
        dmixp_mode: u8,
    },
    ReconGain {
        /// One entry per audio-element layer; `None` where
        /// `recon_gain_is_present` is false for that layer.
        layers: Vec<Option<ReconLayerGains>>,
    },
}

/// Animation type for parameter interpolation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationType {
    Step = 0,
    Linear = 1,
    Bezier = 2,
}

impl AnimationType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Step),
            1 => Some(Self::Linear),
            2 => Some(Self::Bezier),
            _ => None,
        }
    }
}

/// Audio frame from the bitstream
#[derive(Debug, Clone)]
pub struct AudioFrameObu {
    pub substream_id: u32,
    pub samples_to_trim_start: u32,
    pub samples_to_trim_end: u32,
    pub payload: Vec<u8>,
}

/// IAMF stream specification (derived from descriptors)
#[derive(Debug, Clone)]
pub struct IamfSpec {
    pub primary_profile: u8,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub num_samples_per_frame: u32,
    pub output_channels: u16,
    pub output_layout: IamfChannelLayout,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_codec_id_from_bytes() {
        assert_eq!(CodecId::from_bytes(*b"Opus"), Some(CodecId::Opus));
        assert_eq!(CodecId::from_bytes(*b"mp4a"), Some(CodecId::AacLc));
        assert_eq!(CodecId::from_bytes(*b"fLaC"), Some(CodecId::Flac));
        assert_eq!(CodecId::from_bytes(*b"ipcm"), Some(CodecId::Lpcm));
        assert_eq!(CodecId::from_bytes(*b"xxxx"), None);
    }

    #[test]
    fn test_channel_layout_counts() {
        assert_eq!(IamfChannelLayout::Mono.channel_count(), 1);
        assert_eq!(IamfChannelLayout::Stereo.channel_count(), 2);
        assert_eq!(IamfChannelLayout::Layout5_1.channel_count(), 6);
        assert_eq!(IamfChannelLayout::Layout7_1_4.channel_count(), 12);
    }

    #[test]
    fn test_channel_layout_to_speaker_config() {
        assert_eq!(
            IamfChannelLayout::Layout5_1.to_speaker_config_id(),
            Some("5.1")
        );
        assert_eq!(
            IamfChannelLayout::Layout7_1_4.to_speaker_config_id(),
            Some("7.1.4")
        );
        assert_eq!(IamfChannelLayout::Layout3_1_2.to_speaker_config_id(), None);
    }

    #[test]
    fn test_animation_type() {
        assert_eq!(AnimationType::from_u8(0), Some(AnimationType::Step));
        assert_eq!(AnimationType::from_u8(1), Some(AnimationType::Linear));
        assert_eq!(AnimationType::from_u8(2), Some(AnimationType::Bezier));
        assert_eq!(AnimationType::from_u8(3), None);
    }

    #[test]
    fn test_codec_id_as_str() {
        assert_eq!(CodecId::Opus.as_str(), "Opus");
        assert_eq!(CodecId::AacLc.as_str(), "AAC-LC");
        assert_eq!(CodecId::Flac.as_str(), "FLAC");
        assert_eq!(CodecId::Lpcm.as_str(), "LPCM");
    }

    #[test]
    fn test_parameter_data_kind_from_u32() {
        assert_eq!(
            ParameterDataKind::from_u32(0),
            Some(ParameterDataKind::MixGain)
        );
        assert_eq!(
            ParameterDataKind::from_u32(1),
            Some(ParameterDataKind::DemixingInfo)
        );
        assert_eq!(
            ParameterDataKind::from_u32(2),
            Some(ParameterDataKind::ReconGain)
        );
        assert_eq!(ParameterDataKind::from_u32(3), None);
    }

    #[test]
    fn test_channel_layout_from_index_all() {
        assert_eq!(
            IamfChannelLayout::from_layout_index(0),
            Some(IamfChannelLayout::Mono)
        );
        assert_eq!(
            IamfChannelLayout::from_layout_index(1),
            Some(IamfChannelLayout::Stereo)
        );
        assert_eq!(
            IamfChannelLayout::from_layout_index(9),
            Some(IamfChannelLayout::Binaural)
        );
        assert_eq!(IamfChannelLayout::from_layout_index(10), None);
    }

    #[test]
    fn test_channel_layout_counts_all() {
        assert_eq!(IamfChannelLayout::Layout3_1_2.channel_count(), 6);
        assert_eq!(IamfChannelLayout::Layout5_1_2.channel_count(), 8);
        assert_eq!(IamfChannelLayout::Layout5_1_4.channel_count(), 10);
        assert_eq!(IamfChannelLayout::Layout7_1_2.channel_count(), 10);
        assert_eq!(IamfChannelLayout::Layout7_1_4.channel_count(), 12);
        assert_eq!(IamfChannelLayout::Binaural.channel_count(), 2);
    }
}
