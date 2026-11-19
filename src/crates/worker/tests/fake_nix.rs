//! Runner tests against a fake `nix` shell script (no real Nix needed).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::JobSpec;
use tokio::sync::mpsc;
use worker::runner::{BuildOutcome, RunOptions, run_job};

#[derive(Clone, Copy)]
enum Mode {
    Ok,
    BuildFails,
    Hangs,
    SandboxOff,
    BadBuildJson,
}

fn fake_nix(dir: &Path, mode: Mode) -> PathBuf {
    fake_nix_with(dir, mode, "true")
}

fn fake_nix_with(dir: &Path, mode: Mode, copy_cmd: &str) -> PathBuf {
    let calls = dir.join("calls.log").display().to_string();
    let build = match mode {
        Mode::Ok => {
            r#"echo "compiling..." >&2
echo '[{"drvPath":"/nix/store/aaa-hello-c.drv","outputs":{"out":"/nix/store/bbb-hello-c"}}]'"#
        }
        Mode::BuildFails => {
            r#"echo "make: *** [all] Error 2" >&2
echo "error: builder for '/nix/store/zzz-hello-c.drv' failed with exit code 2;" >&2
exit 100"#
        }
        Mode::Hangs => "echo started >&2\nsleep 30",
        Mode::SandboxOff => "echo unreachable >&2",
        Mode::BadBuildJson => "echo 'not json'",
    };
    let sandbox = if matches!(mode, Mode::SandboxOff) {
        "false"
    } else {
        "true"
    };
    let script = format!(
        r#"#!/bin/sh
case "$*" in
  *"config show"*) echo "sandbox = {sandbox}" ;;
  *" store sign "*) echo "$*" >> "{calls}" ;;
  *" copy "*) echo "$*" >> "{calls}"; {copy_cmd} ;;
  *" log "*) echo "full log line 1"; echo "@nix {{ \"action\": \"setPhase\" }}"; echo "full log line 2" ;;
  *" path-info "*)
    echo '[{{"path":"/nix/store/bbb-hello-c","narSize":100,"references":["/nix/store/ccc-glibc"]}},{{"path":"/nix/store/ccc-glibc","narSize":50,"references":[]}}]' ;;
  *" build "*)
    {build}
    ;;
  *) echo "fake nix: unhandled: $*" >&2; exit 64 ;;
esac
"#
    );
    let p = dir.join("nix");
    std::fs::write(&p, script).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn spec() -> JobSpec {
    JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap()
}

fn opts(dir: &Path, mode: Mode) -> RunOptions {
    RunOptions {
        nix_bin: fake_nix(dir, mode),
        work_root: dir.join("work"),
        timeout: Duration::from_secs(10),
        ..RunOptions::default()
    }
}

fn work_dirs(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("work"))
        .map(|d| d.count())
        .unwrap_or(0)
}

#[tokio::test]
async fn success_streams_logs_and_reports_closure() {
    let tmp = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let out = run_job(&spec(), &opts(tmp.path(), Mode::Ok), Some(tx))
        .await
        .unwrap();
    let BuildOutcome::Succeeded(s) = out else {
        panic!("expected success")
    };
    assert_eq!(s.out_path, "/nix/store/bbb-hello-c");
    assert_eq!(s.drv_path.as_deref(), Some("/nix/store/aaa-hello-c.drv"));
    assert_eq!(s.closure.len(), 2);
    assert_eq!(s.closure_nar_size, 150);
    let mut lines = vec![];
    while let Some(l) = rx.recv().await {
        lines.push(l);
    }
    assert!(lines.iter().any(|l| l == "compiling..."));
    assert_eq!(work_dirs(tmp.path()), 0, "work dir must be cleaned up");
}

#[tokio::test]
async fn keep_workdir_keeps_rendered_files() {
    let tmp = tempfile::tempdir().unwrap();
    let mut o = opts(tmp.path(), Mode::Ok);
    o.keep_workdir = true;
    run_job(&spec(), &o, None).await.unwrap();
    let dir = std::fs::read_dir(tmp.path().join("work"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(dir.join("default.nix").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("build.sh"))
            .unwrap()
            .trim(),
        "make"
    );
}

#[tokio::test]
async fn failing_build_is_a_structured_failure_with_full_log() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_job(&spec(), &opts(tmp.path(), Mode::BuildFails), None)
        .await
        .unwrap();
    let BuildOutcome::Failed(f) = out else {
        panic!("expected failure")
    };
    assert_eq!(f.exit_code, Some(100));
    assert!(!f.timed_out);
    assert_eq!(f.drv_path.as_deref(), Some("/nix/store/zzz-hello-c.drv"));
    assert_eq!(f.log_tail, vec!["full log line 1", "full log line 2"]);
    assert_eq!(work_dirs(tmp.path()), 0);
}

#[tokio::test]
async fn timeout_kills_the_process_group() {
    let tmp = tempfile::tempdir().unwrap();
    let mut o = opts(tmp.path(), Mode::Hangs);
    o.timeout = Duration::from_millis(500);
    let t = Instant::now();
    let out = run_job(&spec(), &o, None).await.unwrap();
    let BuildOutcome::Failed(f) = out else {
        panic!("expected failure")
    };
    assert!(f.timed_out);
    assert!(
        t.elapsed() < Duration::from_secs(10),
        "sleep child was not killed"
    );
}

#[tokio::test]
async fn refuses_to_run_without_sandbox() {
    let tmp = tempfile::tempdir().unwrap();
    let err = run_job(&spec(), &opts(tmp.path(), Mode::SandboxOff), None)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("sandbox"));
    assert_eq!(
        work_dirs(tmp.path()),
        0,
        "must fail before creating a work dir"
    );
}

#[tokio::test]
async fn garbage_build_output_is_an_infra_error_not_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        run_job(&spec(), &opts(tmp.path(), Mode::BadBuildJson), None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_spec_is_rejected_before_running_nix() {
    let tmp = tempfile::tempdir().unwrap();
    let mut s = spec();
    s.native_build_inputs.push("foo; evil".into());
    assert!(
        run_job(&s, &opts(tmp.path(), Mode::Ok), None)
            .await
            .is_err()
    );
    assert_eq!(work_dirs(tmp.path()), 0);
}

#[tokio::test]
async fn missing_nix_binary_is_an_infra_error() {
    let tmp = tempfile::tempdir().unwrap();
    let mut o = opts(tmp.path(), Mode::Ok);
    o.nix_bin = tmp.path().join("does-not-exist");
    assert!(run_job(&spec(), &o, None).await.is_err());
}

fn cache_cfg(dir: &Path) -> worker::cache::CacheConfig {
    let key = dir.join("worker.secret");
    std::fs::write(&key, "bamix-worker-1:AAAA").unwrap();
    worker::cache::CacheConfig {
        store_uri: "ssh-ng://bamix-push@cache".into(),
        signing_key: key,
    }
}

#[tokio::test]
async fn success_signs_then_pushes_before_reporting() {
    let tmp = tempfile::tempdir().unwrap();
    let mut o = opts(tmp.path(), Mode::Ok);
    o.cache = Some(cache_cfg(tmp.path()));
    let out = run_job(&spec(), &o, None).await.unwrap();
    let BuildOutcome::Succeeded(s) = out else {
        panic!("expected success")
    };
    assert_eq!(s.pushed_to.as_deref(), Some("ssh-ng://bamix-push@cache"));
    let calls = std::fs::read_to_string(tmp.path().join("calls.log")).unwrap();
    let lines: Vec<&str> = calls.lines().collect();
    assert_eq!(lines.len(), 2, "{calls}");
    assert!(
        lines[0].contains("store sign")
            && lines[0].contains("worker.secret")
            && lines[0].contains(" -r ")
    );
    assert!(lines[1].contains("copy --to ssh-ng://bamix-push@cache /nix/store/bbb-hello-c"));
}

#[tokio::test]
async fn push_failure_is_an_infra_error() {
    let tmp = tempfile::tempdir().unwrap();
    let mut o = opts(tmp.path(), Mode::Ok);
    o.nix_bin = fake_nix_with(
        tmp.path(),
        Mode::Ok,
        "echo 'ssh: connection refused' >&2; exit 1",
    );
    o.cache = Some(cache_cfg(tmp.path()));
    let err = run_job(&spec(), &o, None).await.unwrap_err();
    assert!(format!("{err:#}").contains("connection refused"), "{err:#}");
}

#[tokio::test]
async fn failed_builds_are_not_pushed() {
    let tmp = tempfile::tempdir().unwrap();
    let mut o = opts(tmp.path(), Mode::BuildFails);
    o.cache = Some(cache_cfg(tmp.path()));
    run_job(&spec(), &o, None).await.unwrap();
    assert!(!tmp.path().join("calls.log").exists());
}

#[tokio::test]
async fn bad_cache_config_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let mut o = opts(tmp.path(), Mode::Ok);
    let mut cfg = cache_cfg(tmp.path());
    cfg.store_uri = "http://evil/".into();
    o.cache = Some(cfg);
    assert!(run_job(&spec(), &o, None).await.is_err());
    let mut o = opts(tmp.path(), Mode::Ok);
    let mut cfg = cache_cfg(tmp.path());
    cfg.signing_key = tmp.path().join("missing.key");
    o.cache = Some(cfg);
    assert!(run_job(&spec(), &o, None).await.is_err());
}
