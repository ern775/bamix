//! The job specification: a declarative, data-only description of a build.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Current (and only) accepted spec schema version.
pub const SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NixpkgsPin {
    pub rev: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Source {
    Git {
        url: String,
        rev: String,
        hash: String,
    },
    Tarball {
        url: String,
        hash: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    pub name: String,
    pub nixpkgs: NixpkgsPin,
    pub src: Source,
    /// Tools that run on the build machine (compilers, cmake, pkg-config): `nativeBuildInputs`.
    #[serde(default)]
    pub native_build_inputs: Vec<String>,
    /// Libraries linked into the result (openssl, zlib): `buildInputs`.
    #[serde(default)]
    pub build_inputs: Vec<String>,
    /// Extra environment variables exported before every phase script.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub configure: String,
    pub build: String,
    /// If non-empty, runs as the check phase.
    #[serde(default)]
    pub check: String,
    pub install: String,
}

impl JobSpec {
    /// Parse a spec from YAML text (does not validate; `render` / `validate` do).
    pub fn from_yaml(text: &str) -> Result<Self> {
        serde_saphyr::from_str(text).context("failed to parse job spec YAML")
    }

    pub fn from_path(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::from_yaml(&text)
    }
}
