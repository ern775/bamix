//! Pushing build results to the binary cache (Phase 4).
//!
//! Flow: sign the closure with the worker signing key, then `nix copy` it to the cache
//! machine over ssh-ng. The cache machine accepts the paths because it trusts the worker
//! public key (see `nixos/bamix-cache.nix`); no trusted-user on the cache is needed.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::runner::{RunOptions, nix_output};

#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Destination store URI, e.g. `ssh-ng://bamix-push@laptop`.
    pub store_uri: String,
    /// Secret key file made with `nix-store --generate-binary-cache-key`.
    pub signing_key: PathBuf,
}

pub async fn push(opts: &RunOptions, cfg: &CacheConfig, out_path: &str) -> Result<()> {
    if !cfg.store_uri.starts_with("ssh-ng://") && !cfg.store_uri.starts_with("ssh://") {
        bail!(
            "cache store URI must be ssh-ng:// or ssh://: {:?}",
            cfg.store_uri
        );
    }
    if !cfg.signing_key.is_file() {
        bail!("signing key not found: {}", cfg.signing_key.display());
    }
    let key = cfg.signing_key.to_string_lossy();

    // `-r` signs the whole closure: locally built dependencies carry no signature yet, and
    // adding ours to paths that already have cache.nixos.org signatures is harmless.
    nix_output(opts, &["store", "sign", "--key-file", &key, "-r", out_path])
        .await
        .context("signing closure failed")?;
    nix_output(opts, &["copy", "--to", &cfg.store_uri, out_path])
        .await
        .with_context(|| format!("copying {out_path} to {} failed", cfg.store_uri))?;
    Ok(())
}
