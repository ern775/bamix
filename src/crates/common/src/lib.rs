use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NixpkgsPin {
    pub rev: String,
    pub sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub name: String,
    pub nixpkgs: NixpkgsPin,
    pub src: Source,
    pub packages: Vec<String>,
    pub build: String,
    pub install: String,
}

pub const MAX_PACKAGES: usize = 64;
pub const MAX_SCRIPT_BYTES: usize = 64 * 1024;
pub const MAX_URL_LEN: usize = 2048;
pub const MAX_NAME_LEN: usize = 100;

fn is_ident_char(c: char) -> bool {
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
fn check_package(p: &str) -> Result<()> {
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

    pub fn validate(&self) -> Result<()> {
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

        if self.packages.len() > MAX_PACKAGES {
            bail!(
                "too many packages ({} > {MAX_PACKAGES})",
                self.packages.len()
            );
        }
        for p in &self.packages {
            check_package(p)?;
        }

        for (what, text) in [("build", &self.build), ("install", &self.install)] {
            if text.len() > MAX_SCRIPT_BYTES {
                bail!(
                    "{what} script too large ({} > {MAX_SCRIPT_BYTES} bytes)",
                    text.len()
                );
            }
            if text.contains('\0') {
                bail!("{what} script contains a NUL byte");
            }
        }
        Ok(())
    }
}

fn nix_string(s: &str) -> String {
    let mut out = String::from("\"");
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' if chars.peek() == Some(&'{') => out.push_str("\\$"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

pub struct RenderedJob {
    pub default_nix: String,
    pub build_sh: String,
    pub install_sh: String,
}

impl RenderedJob {
    pub fn write_to(&self, dir: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("default.nix"), &self.default_nix)?;
        std::fs::write(dir.join("build.sh"), &self.build_sh)?;
        std::fs::write(dir.join("install.sh"), &self.install_sh)?;
        Ok(())
    }
}

pub fn render(spec: &JobSpec) -> Result<RenderedJob> {
    spec.validate()?;

    let nixpkgs_url = format!(
        "https://github.com/NixOS/nixpkgs/archive/{}.tar.gz",
        spec.nixpkgs.rev
    );

    let src_expr = match &spec.src {
        Source::Git { url, rev, hash } => format!(
            "pkgs.fetchgit {{\n    url = {};\n    rev = {};\n    hash = {};\n  }}",
            nix_string(url),
            nix_string(rev),
            nix_string(hash)
        ),
        Source::Tarball { url, hash } => format!(
            "pkgs.fetchurl {{\n    url = {};\n    hash = {};\n  }}",
            nix_string(url),
            nix_string(hash)
        ),
    };

    let packages = spec
        .packages
        .iter()
        .map(|p| format!("pkgs.{p}"))
        .collect::<Vec<_>>()
        .join(" ");

    let default_nix = format!(
        r#"let
  pkgs = import (builtins.fetchTarball {{
    url = {nixpkgs_url};
    sha256 = {nixpkgs_sha};
  }}) {{}};
in
pkgs.stdenv.mkDerivation {{
  name = {name};
  src = {src_expr};
  nativeBuildInputs = [ {packages} ];
  PREFIX = placeholder "out";
  buildPhase = builtins.readFile ./build.sh;
  installPhase = builtins.readFile ./install.sh;
}}
"#,
        nixpkgs_url = nix_string(&nixpkgs_url),
        nixpkgs_sha = nix_string(&spec.nixpkgs.sha256),
        name = nix_string(&spec.name),
    );

    Ok(RenderedJob {
        default_nix,
        build_sh: spec.build.clone(),
        install_sh: spec.install.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hello_c() {
        let spec: JobSpec =
            JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap();
        assert_eq!(spec.name, "hello-c");
        assert_eq!(spec.packages, vec!["gnumake"]);
        assert!(matches!(spec.src, Source::Git { .. }));
    }

    #[test]
    fn renders_hello_c() {
        let spec: JobSpec =
            JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap();
        let out = render(&spec).unwrap();
        assert!(
            out.default_nix
                .contains("d1bb09c5cab53abf9fd79bcd524671aaf79da68a")
        );
        assert!(out.default_nix.contains("pkgs.gnumake"));
        assert_eq!(out.install_sh.trim(), "make install");
    }

    #[test]
    fn rejects_bad_package() {
        let mut spec: JobSpec =
            JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap();
        spec.packages.push("foo; evil".into());
        assert!(render(&spec).is_err());
    }

    fn hello() -> JobSpec {
        JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap()
    }

    #[test]
    fn hello_c_is_valid() {
        hello().validate().unwrap();
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
    fn enforces_size_limits() {
        let mut s = hello();
        s.build = "x".repeat(MAX_SCRIPT_BYTES + 1);
        assert!(s.validate().is_err());
        let mut s = hello();
        s.packages = (0..=MAX_PACKAGES).map(|i| format!("p{i}")).collect();
        assert!(s.validate().is_err());
        let mut s = hello();
        s.install = "make\0install".into();
        assert!(s.validate().is_err());
    }

    #[test]
    fn tarball_source_renders_fetchurl() {
        let mut s = hello();
        s.src = Source::Tarball {
            url: "https://example.com/a.tar.gz".into(),
            hash: "sha256-S9W/Quh7StBSLx7ofSaGPcOCOSoqHdm/bGnfEHAE/bk=".into(),
        };
        assert!(render(&s).unwrap().default_nix.contains("pkgs.fetchurl"));
    }

    #[test]
    fn rejects_unknown_and_missing_fields() {
        let base = include_str!("../../../examples/hello-c/job.yaml");
        // typo: `package` instead of `packages`
        let typo = base.replace("packages:", "package:");
        assert!(JobSpec::from_yaml(&typo).is_err());
        // extra top-level key
        assert!(JobSpec::from_yaml(&format!("{base}bogus: 1\n")).is_err());
        // missing required key
        let missing = base.replace("name: hello-c\n", "");
        assert!(JobSpec::from_yaml(&missing).is_err());
    }

    #[test]
    fn escapes_nix_strings() {
        assert_eq!(nix_string(r#"a"b${c}\"#), r#""a\"b\${c}\\""#);
    }

    #[test]
    #[ignore]
    fn rendered_job_builds_with_nix() {
        let spec: JobSpec =
            JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap();
        let dir = std::env::temp_dir().join("bamix-render-test");
        render(&spec).unwrap().write_to(&dir).unwrap();

        let out = std::process::Command::new("nix")
            .args(["build", "-f"])
            .arg(dir.join("default.nix"))
            .args(["--no-link", "--print-out-paths"])
            .output()
            .unwrap();

        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(std::path::Path::new(&path).join("bin/hello").exists());
    }
}
