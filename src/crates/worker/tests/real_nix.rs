//! Runs the worker against the real `nix`. Needs network and a working Nix:
//!   cargo test -p worker --test real_nix -- --ignored --nocapture

use common::JobSpec;
use worker::runner::{BuildOutcome, RunOptions, run_job};

fn spec() -> JobSpec {
    JobSpec::from_yaml(include_str!("../../../examples/hello-c/job.yaml")).unwrap()
}

#[tokio::test]
#[ignore]
async fn hello_c_builds_for_real() {
    let out = run_job(&spec(), &RunOptions::default(), None)
        .await
        .unwrap();
    let BuildOutcome::Succeeded(s) = out else {
        panic!("build failed: {out:?}")
    };
    assert!(std::path::Path::new(&s.out_path).join("bin/hello").exists());
    assert!(s.closure.iter().any(|e| e.path == s.out_path));
    assert!(s.closure_nar_size > 0);
}

#[tokio::test]
#[ignore]
async fn failing_build_is_reported_for_real() {
    let mut s = spec();
    s.build = "echo about-to-fail\nexit 1\n".into();
    let out = run_job(&s, &RunOptions::default(), None).await.unwrap();
    let BuildOutcome::Failed(f) = out else {
        panic!("expected failure, got {out:?}")
    };
    assert_eq!(f.exit_code, Some(1));
    assert!(f.drv_path.is_some());
    assert!(
        f.log_tail.iter().any(|l| l.contains("about-to-fail")),
        "{:?}",
        f.log_tail
    );
    assert!(f.log_tail.iter().all(|l| !l.starts_with("@nix ")));
}
