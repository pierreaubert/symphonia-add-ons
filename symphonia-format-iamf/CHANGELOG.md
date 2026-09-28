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
  groups, and encryption were initially rejected (milestone 1) and are
  now handled per the milestone-2 entries below (`sdtp` and non-roll
  groupings remain rejected).
- Milestone 2, multi-entry tracks: real `iamf` sample entries with
  entry-embedded `iacb` layout boxes, `stsc` description-index
  switching across entries, and per-entry codec configs.
- Milestone 2, `roll` sample groups: `sbgp`/`sgpd` roll recovery
  accepted and validated against each entry's `audio_roll_distance`
  (required for Opus/AAC); `sdtp` and non-roll groupings rejected.
- Milestone 2, fragmented MP4: `mvex`/`trex` defaults plus
  `moof`/`traf`/`trun` runs merged into each track's sample table at
  open (explicit/`moof`-relative/implicit base offsets, per-fragment
  entry selection, `tfdt` continuity checks, empty init segments).
- Milestone 2, edit lists: full multi-edit `elst` application —
  presentation plans over media ranges with empty-edit gaps,
  movie-timescale conversion, and sample-granularity selection.
- Milestone 2, sync/composition strictness: `stss` validation,
  `ctts` (and fragment `trun`) composition offsets applied to packet
  timestamps, and traf-level `roll` group validation.
- Milestone 2, protection reporting: `enca` entries rejected with the
  `sinf` scheme/version/original format reported
  (`IamfMp4Error::Protected`); `seig`/`saiz`/`saio` signals rejected
  precisely instead of passing ciphertext through as cleartext.
- Milestone 2, BS.2051 acceptance in `sotf-iamf`: mix-presentation
  loudness layouts with unknown sound systems parse (unrenderable
  layouts drop, rendering falls back) instead of failing the file.
- `IamfTrackConfig::codecs_string`: RFC 6381 codecs parameter string
  per track (`iamf.<profile:03>.<additional:03>.<codec elements>`
  per IAMF §6.4, with AAC audio object types from the ASC).

### Changed
- `elst` handling is now full edit-list application (media-range
  selection with presentation rebasing) instead of a first-edit
  timestamp offset; `IamfTrackConfig::start_media_time` is replaced by
  the parsed `edits` plus the derived `edit_plan`, and track
  duration/frame counts follow the selected presentation.
- `IamfMp4Error::Unsupported` display is now `unsupported: {0}`
  (was `unsupported (milestone 2): {0}`).
