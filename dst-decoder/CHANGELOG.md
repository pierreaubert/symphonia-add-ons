# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Added decode coverage for the uncompressed-DSD escape path and the
  stuffing-pattern rejection, without requiring external fixtures.
- Added a deterministic randomized differential test pinning the SSE2/AVX2 FIR
  paths against the scalar fallback, plus direct SSE2 assertions.

### Changed
- Documented the SIMD rationale (per-bit hot path) next to `fir_predict`.

## [0.1.1](https://github.com/bleggett/dst-decoder/compare/v0.1.0...v0.1.1) - 2026-05-02

### Other

- rustdoc
- Add release workflow
