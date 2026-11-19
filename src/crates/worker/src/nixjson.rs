//! Parsers for the JSON that `nix build --json` and `nix path-info --json` print.
//!
//! Nix has changed these formats between releases, so the parsers accept every
//! shape we know about and ignore unknown fields.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClosureEntry {
    pub path: String,
    pub nar_size: Option<u64>,
    pub references: Vec<String>,
}

#[derive(Deserialize)]
struct BuildEntry {
    #[serde(rename = "drvPath")]
    drv_path: Option<String>,
    #[serde(default)]
    outputs: BTreeMap<String, String>,
}

/// `nix build --json` -> (out path, drv path).
pub fn parse_build_json(s: &str) -> Result<(String, Option<String>)> {
    let entries: Vec<BuildEntry> =
        serde_json::from_str(s).context("`nix build --json` output is not the expected array")?;
    let first = entries
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("`nix build --json` returned no entries"))?;
    let out = first
        .outputs
        .get("out")
        .cloned()
        .ok_or_else(|| anyhow!("build result has no `out` output"))?;
    Ok((out, first.drv_path))
}

fn normalize_store_path(p: &str) -> String {
    if p.starts_with('/') {
        p.to_string()
    } else {
        format!("/nix/store/{p}")
    }
}

fn entry_from(path: &str, v: &Value) -> ClosureEntry {
    ClosureEntry {
        path: normalize_store_path(path),
        nar_size: v.get("narSize").and_then(Value::as_u64),
        references: v
            .get("references")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(normalize_store_path)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// `nix path-info -r --json` -> closure entries, sorted by path.
///
/// Accepted shapes:
/// - array of objects with a `path` field (Nix 2.4+)
/// - object keyed by store path (newer Nix)
/// - `{"version": N, "info": { ... }}` wrapper (json-format 2)
pub fn parse_path_info(s: &str) -> Result<Vec<ClosureEntry>> {
    let v: Value = serde_json::from_str(s).context("path-info output is not JSON")?;
    let mut out = match &v {
        Value::Array(items) => items
            .iter()
            .filter_map(|it| {
                it.get("path")
                    .and_then(Value::as_str)
                    .map(|p| entry_from(p, it))
            })
            .collect::<Vec<_>>(),
        Value::Object(map) => {
            let map = match map.get("info") {
                Some(Value::Object(inner)) if map.contains_key("version") => inner,
                _ => map,
            };
            map.iter()
                .filter(|(_, val)| val.is_object())
                .map(|(k, val)| entry_from(k, val))
                .collect()
        }
        _ => return Err(anyhow!("unexpected path-info JSON shape")),
    };
    out.sort_by(|a, b| a.path.cmp(&b.path));
    if out.is_empty() {
        return Err(anyhow!("path-info returned an empty closure"));
    }
    Ok(out)
}

/// Value of `sandbox` from `nix config show` / `nix show-config` text.
pub fn parse_sandbox_setting(config: &str) -> Option<String> {
    config.lines().find_map(|l| {
        let (k, v) = l.split_once('=')?;
        (k.trim() == "sandbox").then(|| v.trim().to_string())
    })
}

/// Best-effort: find the last `/nix/store/...drv` mentioned in a log.
pub fn find_drv_path(lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|line| {
        let start = line.find("/nix/store/")?;
        let rest = &line[start..];
        let end = rest.find(".drv")? + 4;
        let cand = &rest[..end];
        (!cand.chars().any(char::is_whitespace)).then(|| cand.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_json() {
        let j = r#"[{"drvPath":"/nix/store/a-x.drv","outputs":{"out":"/nix/store/b-x"},"startTime":1}]"#;
        let (out, drv) = parse_build_json(j).unwrap();
        assert_eq!(out, "/nix/store/b-x");
        assert_eq!(drv.as_deref(), Some("/nix/store/a-x.drv"));
        assert!(parse_build_json("[]").is_err());
        assert!(parse_build_json(r#"[{"outputs":{"dev":"/nix/store/c"}}]"#).is_err());
    }

    #[test]
    fn path_info_array_and_object_shapes() {
        let arr = r#"[{"path":"/nix/store/b-x","narSize":10,"references":["/nix/store/c-glibc"]},
                      {"path":"/nix/store/c-glibc","narSize":5,"references":[]}]"#;
        let a = parse_path_info(arr).unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].path, "/nix/store/b-x");
        assert_eq!(a[0].references, vec!["/nix/store/c-glibc"]);

        let obj = r#"{"/nix/store/b-x":{"narSize":10,"references":[]},"/nix/store/c-glibc":{"narSize":5,"references":[]}}"#;
        assert_eq!(
            parse_path_info(obj).unwrap(),
            a.iter()
                .map(|e| ClosureEntry {
                    references: vec![],
                    ..e.clone()
                })
                .collect::<Vec<_>>()
        );

        let wrapped = r#"{"version":2,"info":{"b-x":{"narSize":10,"references":["c-glibc"]}}}"#;
        let w = parse_path_info(wrapped).unwrap();
        assert_eq!(w[0].path, "/nix/store/b-x");
        assert_eq!(w[0].references, vec!["/nix/store/c-glibc"]);

        assert!(parse_path_info("[]").is_err());
    }

    #[test]
    fn real_nix_path_info_v1_output() {
        // Captured from a real `nix path-info -r --json --json-format 1` run on NixOS.
        let c = parse_path_info(include_str!("../tests/fixtures/path-info-v1.json")).unwrap();
        assert_eq!(c.len(), 5);
        let hello = c.iter().find(|e| e.path.ends_with("-hello-c")).unwrap();
        assert_eq!(hello.nar_size, Some(16176));
        assert_eq!(hello.references.len(), 1);
        assert_eq!(c.iter().filter_map(|e| e.nar_size).sum::<u64>(), 38_252_104);
    }

    #[test]
    fn sandbox_and_drv() {
        assert_eq!(
            parse_sandbox_setting("cores = 0\nsandbox = true\nfoo = bar\n").as_deref(),
            Some("true")
        );
        assert_eq!(parse_sandbox_setting("cores = 0\n"), None);
        let lines = vec![
            "error: builder for '/nix/store/abc-hello.drv' failed with exit code 2;".to_string(),
        ];
        assert_eq!(
            find_drv_path(&lines).as_deref(),
            Some("/nix/store/abc-hello.drv")
        );
    }
}
