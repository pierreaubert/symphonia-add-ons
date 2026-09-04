# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- Replaced panicking `unwrap()`s on decorrelation-weight parsing and Matroska
  block-sample handling with proper decode errors on truncated input.
- Migrated stereo chunk iteration to `as_chunks` (clippy gate is green).

### Added
- Added corrupt-input rejection tests for weights, channel info, and Matroska
  packet expansion.

## [0.1.0] - 2026-06-09

### Added
- Added a native Symphonia WavPack reader and decoder.
- Added support for native `.wv` streams and Matroska `A_WAVPACK4` packets.
- Added WavPack word decoding, decorrelation, joint stereo, false stereo,
  integer and float sample fixup, hybrid lossy streams, embedded correction
  bitstreams, and trailing metadata handling.
- Added raw-mode packed-byte DSD support.

### Known Limitations
- Compressed WavPack DSD modes are not implemented yet.
- External `.wvc` correction-file reconstruction is not implemented yet.
