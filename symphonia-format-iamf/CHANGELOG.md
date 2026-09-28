# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Initial `symphonia-format-iamf` release: pure-Rust IAMF v1.1.0 ISO-BMFF
  (`.mp4`) demuxer exposing a Symphonia `FormatReader` (`register_all`).
- `ftyp` brand check, `trak` walk with `iacb` descriptor OBUs validated by
  `sotf-iamf`, and single-`iamf`-entry `stsd` handling with sample-entry
  codec-config cross-check.
- Sample-table expansion (`stts`/`stsc`/`stsz`/`stz2`/`stco`/`co64`,
  `largesize`, `mdhd`, first-edit `elst` media time) with per-packet
  timestamps; one raw codec frame (Opus/AAC/FLAC/LPCM) per packet.
- `reassemble_ia_sequence` bridge rebuilding the canonical IA Sequence
  descriptor section from per-track `iacb` sections for `sotf-iamf`.
- Rewind-to-start seeking; fragmented MP4 (`moof`/`mvex`), sample
  dependencies (`sdtp`), sample groups, and encryption are rejected as
  milestone-2 scope.
