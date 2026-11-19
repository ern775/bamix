//! Validation of untrusted `JobSpec`s. Everything that ends up in generated Nix or shell
//! must pass through here or through the escapers in `render`.

use anyhow::{Result, bail};

use crate::spec::{JobSpec, SCHEMA_VERSION, Source};

pub const MAX_PACKAGES: usize = 64;
pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;
pub const MAX_URL_LEN: usize = 2048;
pub const MAX_NAME_LEN: usize = 100;
pub const MAX_ENV_VARS: usize = 64;
pub const MAX_ENV_VALUE_BYTES: usize = 4096;

pub(crate) fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// Nix's base32 alphabet (no `e`, `o`, `u`, `t`).
fn is_nix32_char(c: char) -> bool {
    "0123456789abcdfghijklmnpqrsvwxyz".contains(c)
}

fn is_base64_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')
}

/// Accepts a sha256 as nix32 (52 chars) or SRI (`sha256-` + 44 base64 chars).
fn is_sha256(s: &str) -> bool {
    if let Some(b64) = s.strip_prefix("sha256-") {
        b64.len() == 44 && b64.chars().all(is_base64_char)
    } else {
        s.len() == 52 && s.chars().all(is_nix32_char)
    }
}

/// Source hashes: sha256 (nix32 or SRI) or sha512 SRI (`sha512-` + 88 base64 chars).
fn is_source_hash(s: &str) -> bool {
    if let Some(b64) = s.strip_prefix("sha512-") {
        b64.len() == 88 && b64.chars().all(is_base64_char)
    } else {
        is_sha256(s)
    }
}

fn is_git_rev(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn check_url(url: &str) -> Result<()> {
    if url.len() > MAX_URL_LEN {
        bail!("url too long ({} bytes)", url.len());
    }
    let Some(rest) = url.strip_prefix("https://") else {
        bail!("url must start with https://: {url:?}");
    };
    if rest.is_empty() || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("url contains whitespace/control characters or has no host: {url:?}");
    }
    Ok(())
}

/// A package is a dotted attribute path into pkgs, e.g. `gnumake` or `python3Packages.numpy`.
/// Every segment must start with a letter/underscore so the generated `pkgs.<attr>` is valid Nix.
pub(crate) fn check_package(p: &str) -> Result<()> {
    if p.is_empty() || !p.chars().all(is_ident_char) {
        bail!("invalid package name: {p:?}");
    }
    for seg in p.split('.') {
        let ok = seg
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        if !ok {
            bail!("invalid package name (bad attribute segment): {p:?}");
        }
    }
    Ok(())
}

/// Env var names must be plain shell identifiers and must not clobber things the build
/// contract or Nix itself relies on.
pub(crate) fn check_env_name(k: &str) -> Result<()> {
    let mut chars = k.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    if !first_ok || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("invalid env var name: {k:?}");
    }
    let reserved = matches!(
        k,
        "PATH" | "HOME" | "TMPDIR" | "TMP" | "TEMP" | "PREFIX" | "out" | "src" | "SHELL"
    ) || k.starts_with("NIX_")
        || k.starts_with("LD_");
    if reserved {
        bail!("env var name is reserved: {k:?}");
    }
    Ok(())
}

fn check_script(what: &str, text: &str) -> Result<()> {
    if text.len() > MAX_SCRIPT_BYTES {
        bail!(
            "{what} script too large ({} > {MAX_SCRIPT_BYTES} bytes)",
            text.len()
        );
    }
    if text.contains('\0') {
        bail!("{what} script contains a NUL byte");
    }
    Ok(())
}

impl JobSpec {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported schema_version {} (this build supports {SCHEMA_VERSION})",
                self.schema_version
            );
        }
        if self.name.is_empty()
            || self.name.len() > MAX_NAME_LEN
            || !self.name.chars().all(is_ident_char)
        {
            bail!("invalid job name: {:?}", self.name);
        }
        if self.name.starts_with(['-', '.']) {
            bail!("job name must not start with '-' or '.': {:?}", self.name);
        }

        if !is_git_rev(&self.nixpkgs.rev) {
            bail!("nixpkgs rev must be a full hex commit hash");
        }
        if !is_sha256(&self.nixpkgs.sha256) {
            bail!("nixpkgs sha256 must be nix32 (52 chars) or SRI (sha256-...)");
        }

        match &self.src {
            Source::Git { url, rev, hash } => {
                check_url(url)?;
                if !is_git_rev(rev) {
                    bail!("git rev must be a full hex commit hash");
                }
                if !is_source_hash(hash) {
                    bail!("invalid source hash: {hash:?}");
                }
            }
            Source::Tarball { url, hash } => {
                check_url(url)?;
                if !is_source_hash(hash) {
                    bail!("invalid source hash: {hash:?}");
                }
            }
        }

        let n_pkgs = self.native_build_inputs.len() + self.build_inputs.len();
        if n_pkgs > MAX_PACKAGES {
            bail!("too many packages ({n_pkgs} > {MAX_PACKAGES})");
        }
        for p in self.native_build_inputs.iter().chain(&self.build_inputs) {
            check_package(p)?;
        }

        if self.env.len() > MAX_ENV_VARS {
            bail!("too many env vars ({} > {MAX_ENV_VARS})", self.env.len());
        }
        for (k, v) in &self.env {
            check_env_name(k)?;
            if v.len() > MAX_ENV_VALUE_BYTES || v.contains('\0') {
                bail!("env var {k:?} has an invalid or oversized value");
            }
        }

        for (what, text) in [
            ("configure", &self.configure),
            ("build", &self.build),
            ("check", &self.check),
            ("install", &self.install),
        ] {
            check_script(what, text)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::hello;

    #[test]
    fn hello_c_is_valid() {
        hello().validate().unwrap();
    }

    #[test]
    fn rejects_bad_package() {
        let mut s = hello();
        s.native_build_inputs.push("foo; evil".into());
        assert!(s.validate().is_err());
        let mut s = hello();
        s.build_inputs.push("a..b".into());
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_bad_nixpkgs_rev_and_hash() {
        let mut s = hello();
        s.nixpkgs.rev = "main".into();
        assert!(s.validate().is_err());
        let mut s = hello();
        s.nixpkgs.rev = "abc123".into(); // hex but not a full commit
        assert!(s.validate().is_err());
        let mut s = hello();
        s.nixpkgs.sha256 = "not-a-hash".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_non_https_urls() {
        for bad in [
            "http://example.com/x.git",
            "file:///etc/passwd",
            "git@github.com:a/b.git",
            "https://",
            "https://exa mple.com/x",
        ] {
            let mut s = hello();
            if let Source::Git { url, .. } = &mut s.src {
                *url = bad.into();
            }
            assert!(s.validate().is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rejects_bad_source_hash_and_rev() {
        let mut s = hello();
        if let Source::Git { hash, .. } = &mut s.src {
            *hash = "sha256-tooshort".into();
        }
        assert!(s.validate().is_err());
        let mut s = hello();
        if let Source::Git { rev, .. } = &mut s.src {
            *rev = "HEAD".into();
        }
        assert!(s.validate().is_err());
    }

    #[test]
    fn package_attr_paths() {
        for ok in ["gnumake", "python3Packages.numpy", "_7zz", "gcc-unwrapped"] {
            check_package(ok).unwrap();
        }
        for bad in [
            "", ".foo", "foo.", "a..b", "1abc", "a.1b", "-x", "a b", "a;b",
        ] {
            assert!(check_package(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn env_names() {
        for ok in ["CFLAGS", "_x", "MY_VAR2"] {
            check_env_name(ok).unwrap();
        }
        for bad in [
            "",
            "1A",
            "A-B",
            "A B",
            "A=B",
            "PATH",
            "out",
            "PREFIX",
            "NIX_BUILD_CORES",
            "LD_PRELOAD",
            "A;rm",
        ] {
            assert!(check_env_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn enforces_size_limits() {
        let mut s = hello();
        s.build = "x".repeat(MAX_SCRIPT_BYTES + 1);
        assert!(s.validate().is_err());
        let mut s = hello();
        s.native_build_inputs = (0..=MAX_PACKAGES).map(|i| format!("p{i}")).collect();
        assert!(s.validate().is_err());
        let mut s = hello();
        s.install = "make\0install".into();
        assert!(s.validate().is_err());
        let mut s = hello();
        s.env
            .insert("BIG".into(), "x".repeat(MAX_ENV_VALUE_BYTES + 1));
        assert!(s.validate().is_err());
    }

    #[test]
    fn rejects_unknown_schema_version() {
        let mut s = hello();
        s.schema_version = 2;
        assert!(s.validate().is_err());
    }
}
