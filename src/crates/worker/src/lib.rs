//! Bamix worker: runs one `JobSpec` through `nix build` locally (Phase 3).
//!
//! Layout:
//! - `nixjson`: tolerant parsers for Nix's JSON output (pure, unit-tested)
//! - `runner`:  work dir, process supervision, log streaming, timeouts, outcome types

pub mod nixjson;
pub mod runner;
