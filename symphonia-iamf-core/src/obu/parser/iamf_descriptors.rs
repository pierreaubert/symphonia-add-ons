use crate::types::*;
use std::collections::HashMap;

/// Parsed descriptor section of an IAMF stream
#[derive(Debug, Clone)]
pub struct IamfDescriptors {
    pub primary_profile: u8,
    pub additional_profile: u8,
    pub codec_configs: Vec<CodecConfig>,
    pub audio_elements: Vec<AudioElement>,
    pub mix_presentations: Vec<MixPresentation>,
}

impl IamfDescriptors {
    /// Build a `parameter_id -> kind` map from all audio_element parameter
    /// definitions. Parameter blocks in temporal units reference these IDs;
    /// the kind drives `parse_parameter_block_with_kind` payload dispatch.
    pub fn parameter_kinds(&self) -> HashMap<u32, ParameterDataKind> {
        let mut map = HashMap::new();
        for ae in &self.audio_elements {
            for pd in &ae.parameter_definitions {
                map.insert(pd.parameter_id, pd.parameter_kind);
            }
        }
        map
    }

    /// Build a `parameter_id -> ReconGainLayout` map from channel audio
    /// elements that define a ReconGain parameter. The parser needs the
    /// owning element's layer flags to know how many gain bytes follow.
    pub fn recon_layouts(&self) -> HashMap<u32, ReconGainLayout> {
        let mut map = HashMap::new();
        for ae in &self.audio_elements {
            let defines_recon = ae
                .parameter_definitions
                .iter()
                .any(|pd| pd.parameter_kind == ParameterDataKind::ReconGain);
            if !defines_recon {
                continue;
            }
            if let ElementConfig::Channel(config) = &ae.element_config {
                let layers = &config.layers;
                for pd in &ae.parameter_definitions {
                    if pd.parameter_kind != ParameterDataKind::ReconGain {
                        continue;
                    }
                    map.entry(pd.parameter_id)
                        .or_insert_with(|| ReconGainLayout {
                            num_layers: layers.len() as u8,
                            layers_present: layers
                                .iter()
                                .map(|layer| layer.recon_gain_is_present)
                                .collect(),
                        });
                }
            }
        }
        map
    }
}
