//! Symphonia [`Probe`](symphonia_core::formats::probe::Probe) registration.

use symphonia_core::codecs::registry::CodecRegistry;
use symphonia_core::formats::probe::Probe;

use super::reader::IamfFormatReader;

/// Register the IAMF format reader on a Symphonia probe.
pub fn register_all(probe: &mut Probe) {
    probe.register_format::<IamfFormatReader<'_>>();
}

/// IAMF audio frames decode through the engine's existing codec path
/// (`sotf-iamf` substream decoders), so no decoder registration is needed.
/// This no-op keeps the same shape as the sibling format crates.
pub fn register_decoders(_registry: &mut CodecRegistry) {}
