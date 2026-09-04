# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Added a `file-io` feature (enabled by default) gating the file-conversion
  API (`Rdsd2Pcm`, writers, dither, file discovery, metadata). The DSP core
  (`DsdPcmConverter`, `DsdPcmOptions`) now builds without `rand`, `flac-codec`,
  or `id3`.
- Added a bit-identical repeat-conversion test pinning DSP determinism.

### Changed
- `is_dsd_file` now takes `&Path` instead of `&PathBuf` (call sites passing
  `&PathBuf` keep working through deref coercion).
- Removed the blanket `#![allow(clippy::all)]`; the crate is clean under
  `cargo clippy -- -D warnings` with narrowly scoped allows for the imported
  filter tables and legacy constructors.
- Removed the unused direct `dsf-meta`/`dff-meta` dependencies (still used
  through `dsd-reader`).

## [0.3.0] - 2026-06-09

### Added
- Added the local SOTF workspace integration of `rdsd2pcm` for DSD-to-PCM
  conversion.
- Added reusable converter APIs used by Symphonia DSD/DST decoders and SACD
  extraction examples.
- Added configurable PCM output support for floating-point and integer WAV
  extraction paths.

### Changed
- Adapted conversion helpers for SACD-style packed DSD input and configurable
  output sample rates.
