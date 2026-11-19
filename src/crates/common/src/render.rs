//! Renders a validated `JobSpec` into `default.nix` plus phase scripts.
//!
//! Safety rules: strings embedded in Nix go through `nix_string`; env values embedded in
//! shell go through `shell_quote`; user scripts are never spliced into Nix source, they are
//! read from files with `builtins.readFile`.

use anyhow::Result;

use crate::spec::{JobSpec, Source};

pub(crate) fn nix_string(s: &str) -> String {
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

/// POSIX single-quote escaping.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[derive(Debug, Clone)]
pub struct RenderedJob {
    pub default_nix: String,
    pub build_sh: String,
    pub install_sh: String,
    pub configure_sh: Option<String>,
    pub check_sh: Option<String>,
}

impl RenderedJob {
    pub fn write_to(&self, dir: &std::path::Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("default.nix"), &self.default_nix)?;
        std::fs::write(dir.join("build.sh"), &self.build_sh)?;
        std::fs::write(dir.join("install.sh"), &self.install_sh)?;
        if let Some(s) = &self.configure_sh {
            std::fs::write(dir.join("configure.sh"), s)?;
        }
        if let Some(s) = &self.check_sh {
            std::fs::write(dir.join("check.sh"), s)?;
        }
        Ok(())
    }
}

fn pkg_list(pkgs: &[String]) -> String {
    pkgs.iter()
        .map(|p| format!("pkgs.{p}"))
        .collect::<Vec<_>>()
        .join(" ")
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

    // Env goes into the scripts, not into derivation attributes: an attribute named e.g.
    // `__noChroot` would otherwise let a spec switch off the sandbox.
    let env_prefix: String = spec
        .env
        .iter()
        .map(|(k, v)| format!("export {k}={}\n", shell_quote(v)))
        .collect();
    let with_env = |script: &str| format!("{env_prefix}{script}");

    let configure_sh = (!spec.configure.trim().is_empty()).then(|| with_env(&spec.configure));
    let check_sh = (!spec.check.trim().is_empty()).then(|| with_env(&spec.check));

    let mut phases = String::new();
    if configure_sh.is_some() {
        phases.push_str("  configurePhase = builtins.readFile ./configure.sh;\n");
    }
    phases.push_str("  buildPhase = builtins.readFile ./build.sh;\n");
    if check_sh.is_some() {
        phases.push_str("  doCheck = true;\n  checkPhase = builtins.readFile ./check.sh;\n");
    }
    phases.push_str("  installPhase = builtins.readFile ./install.sh;\n");

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
  nativeBuildInputs = [ {native} ];
  buildInputs = [ {build} ];
  PREFIX = placeholder "out";
{phases}}}
"#,
        nixpkgs_url = nix_string(&nixpkgs_url),
        nixpkgs_sha = nix_string(&spec.nixpkgs.sha256),
        name = nix_string(&spec.name),
        native = pkg_list(&spec.native_build_inputs),
        build = pkg_list(&spec.build_inputs),
    );

    Ok(RenderedJob {
        default_nix,
        build_sh: with_env(&spec.build),
        install_sh: with_env(&spec.install),
        configure_sh,
        check_sh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::hello;

    #[test]
    fn renders_hello_c() {
        let out = render(&hello()).unwrap();
        assert!(
            out.default_nix
                .contains("d1bb09c5cab53abf9fd79bcd524671aaf79da68a")
        );
        assert!(
            out.default_nix
                .contains("nativeBuildInputs = [ pkgs.gnumake ];")
        );
        assert!(out.default_nix.contains("buildInputs = [  ];"));
        assert!(!out.default_nix.contains("configurePhase"));
        assert!(!out.default_nix.contains("doCheck"));
        assert_eq!(out.install_sh.trim(), "make install");
        assert!(out.configure_sh.is_none() && out.check_sh.is_none());
    }

    #[test]
    fn renders_optional_phases_and_inputs() {
        let mut s = hello();
        s.build_inputs = vec!["zlib".into(), "openssl".into()];
        s.configure = "./configure --prefix=$PREFIX".into();
        s.check = "make test".into();
        let out = render(&s).unwrap();
        assert!(
            out.default_nix
                .contains("buildInputs = [ pkgs.zlib pkgs.openssl ];")
        );
        assert!(
            out.default_nix
                .contains("configurePhase = builtins.readFile ./configure.sh;")
        );
        assert!(out.default_nix.contains("doCheck = true;"));
        assert_eq!(
            out.configure_sh.as_deref(),
            Some("./configure --prefix=$PREFIX")
        );
        assert_eq!(out.check_sh.as_deref(), Some("make test"));
    }

    #[test]
    fn env_is_exported_in_scripts_never_in_nix() {
        let mut s = hello();
        s.env
            .insert("CFLAGS".into(), "-O2 -DNAME='x y'; rm -rf /".into());
        let out = render(&s).unwrap();
        assert!(!out.default_nix.contains("CFLAGS"));
        assert!(
            out.build_sh
                .starts_with("export CFLAGS='-O2 -DNAME='\\''x y'\\''; rm -rf /'\n")
        );
        assert!(out.install_sh.starts_with("export CFLAGS="));
        // `__noChroot` is a legal shell identifier; as an exported variable it is harmless,
        // whereas as a derivation attribute it would disable the sandbox.
        s.env.insert("__noChroot".into(), "1".into());
        let out = render(&s).unwrap();
        assert!(!out.default_nix.contains("__noChroot"));
    }

    #[test]
    fn rejects_invalid_spec() {
        let mut s = hello();
        s.native_build_inputs.push("foo; evil".into());
        assert!(render(&s).is_err());
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
    fn escapes_nix_strings() {
        assert_eq!(nix_string(r#"a"b${c}\"#), r#""a\"b\${c}\\""#);
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(x)`y`"), "'$(x)`y`'");
    }

    #[test]
    #[ignore]
    fn rendered_job_builds_with_nix() {
        let dir = std::env::temp_dir().join("bamix-render-test");
        render(&hello()).unwrap().write_to(&dir).unwrap();

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
