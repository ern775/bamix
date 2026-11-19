//! Runs one job: validate -> render -> `nix build` -> closure.
//!
//! Error model (matters for Phase 8 retry policy):
//!
//! - `Err(_)` is an infrastructure problem (nix missing, sandbox off, unparseable output,
//!   cannot create the work dir). Safe to retry on another worker.
//! - `Ok(BuildOutcome::Failed)` means the job itself failed or timed out. Do not blindly retry.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use common::{JobSpec, render};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use crate::nixjson::{self, ClosureEntry};

/// Receives build log lines as they appear (later: forwarded to the dispatcher).
pub type LogTx = UnboundedSender<String>;

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub nix_bin: PathBuf,
    pub work_root: PathBuf,
    pub timeout: Duration,
    pub keep_workdir: bool,
    pub require_sandbox: bool,
    pub log_tail_lines: usize,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            nix_bin: "nix".into(),
            work_root: std::env::temp_dir().join("bamix-worker"),
            timeout: Duration::from_secs(3600),
            keep_workdir: false,
            require_sandbox: true,
            log_tail_lines: 200,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct BuildSuccess {
    pub job_id: String,
    pub out_path: String,
    pub drv_path: Option<String>,
    pub closure: Vec<ClosureEntry>,
    pub closure_nar_size: u64,
    pub build_secs: f64,
}

#[derive(Debug, Serialize)]
pub struct BuildFailure {
    pub job_id: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub drv_path: Option<String>,
    pub log_tail: Vec<String>,
    pub build_secs: f64,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildOutcome {
    Succeeded(BuildSuccess),
    Failed(BuildFailure),
}

/// Per-job work directory, removed on drop unless `keep`.
struct WorkDir {
    path: PathBuf,
    keep: bool,
}

impl WorkDir {
    fn create(path: PathBuf, keep: bool) -> Result<Self> {
        std::fs::create_dir_all(&path)
            .with_context(|| format!("cannot create work dir {}", path.display()))?;
        Ok(Self { path, keep })
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn new_job_id(name: &str) -> String {
    // Phase 6 replaces this with a dispatcher-issued UUIDv7/ULID.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{name}-{nanos:x}-{}", std::process::id())
}

struct Captured {
    status: Option<ExitStatus>,
    stdout: String,
    tail: Vec<String>,
    timed_out: bool,
}

/// Spawn `cmd` in its own process group; stream stderr line by line; capture stdout;
/// kill the whole group on timeout.
async fn run_streaming(
    mut cmd: Command,
    timeout: Option<Duration>,
    log: Option<&LogTx>,
    tail_lines: usize,
) -> Result<Captured> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = cmd.spawn().context("failed to spawn process")?;
    let pid = child.id();

    let mut stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let out_task = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stdout.read_to_string(&mut buf).await;
        buf
    });
    let log = log.cloned();
    let err_task = tokio::spawn(async move {
        let mut tail: VecDeque<String> = VecDeque::with_capacity(tail_lines + 1);
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(tx) = &log {
                let _ = tx.send(line.clone());
            }
            if tail_lines > 0 {
                if tail.len() == tail_lines {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        }
        tail.into_iter().collect::<Vec<_>>()
    });

    let (status, timed_out) = match timeout {
        Some(t) => match tokio::time::timeout(t, child.wait()).await {
            Ok(r) => (Some(r.context("wait failed")?), false),
            Err(_) => {
                if let Some(pid) = pid {
                    // SAFETY: plain syscall; pid is our child's pgid (process_group(0)).
                    unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
                }
                let _ = child.wait().await;
                (None, true)
            }
        },
        None => (Some(child.wait().await.context("wait failed")?), false),
    };

    Ok(Captured {
        status,
        stdout: out_task.await.unwrap_or_default(),
        tail: err_task.await.unwrap_or_default(),
        timed_out,
    })
}

fn nix_cmd(opts: &RunOptions, args: &[&str]) -> Command {
    let mut c = Command::new(&opts.nix_bin);
    c.args(["--extra-experimental-features", "nix-command"])
        .args(args);
    c
}

/// Run a short nix command and return stdout, failing on non-zero exit.
async fn nix_output(opts: &RunOptions, args: &[&str]) -> Result<String> {
    let cap = run_streaming(
        nix_cmd(opts, args),
        Some(Duration::from_secs(120)),
        None,
        20,
    )
    .await?;
    match cap.status {
        Some(s) if s.success() => Ok(cap.stdout),
        _ => bail!("`nix {}` failed: {}", args.join(" "), cap.tail.join(" | ")),
    }
}

/// Refuse to build unless Nix reports `sandbox = true`.
pub async fn preflight_sandbox(opts: &RunOptions) -> Result<()> {
    let cfg = match nix_output(opts, &["config", "show"]).await {
        Ok(s) => s,
        Err(_) => nix_output(opts, &["show-config"])
            .await
            .context("could not read nix config (tried `nix config show` and `nix show-config`)")?,
    };
    match nixjson::parse_sandbox_setting(&cfg).as_deref() {
        Some("true") => Ok(()),
        Some(other) => bail!("nix `sandbox` is {other:?}; refusing to run untrusted builds"),
        None => bail!("could not find `sandbox` in nix config output"),
    }
}

pub async fn run_job(
    spec: &JobSpec,
    opts: &RunOptions,
    log: Option<LogTx>,
) -> Result<BuildOutcome> {
    let rendered = render(spec)?; // validates first
    if opts.require_sandbox {
        preflight_sandbox(opts).await?;
    }

    let job_id = new_job_id(&spec.name);
    let work = WorkDir::create(opts.work_root.join(&job_id), opts.keep_workdir)?;
    rendered.write_to(&work.path)?;
    let default_nix = work.path.join("default.nix");
    info!(%job_id, dir = %work.path.display(), "starting build");

    let started = Instant::now();
    let cap = run_streaming(
        nix_cmd(
            opts,
            &[
                "build",
                "-f",
                &default_nix.to_string_lossy(),
                "--json",
                "--no-link",
                "-L",
            ],
        ),
        Some(opts.timeout),
        log.as_ref(),
        opts.log_tail_lines,
    )
    .await?;
    let build_secs = started.elapsed().as_secs_f64();

    if cap.timed_out || !cap.status.is_some_and(|s| s.success()) {
        let drv_path = nixjson::find_drv_path(&cap.tail);
        let mut log_tail = cap.tail;
        if let Some(drv) = &drv_path {
            // Full log of the failed derivation is more useful than nix's trailer.
            if let Ok(full) = nix_output(opts, &["log", drv]).await {
                let lines: Vec<String> = full.lines().map(str::to_owned).collect();
                let skip = lines.len().saturating_sub(opts.log_tail_lines);
                log_tail = lines[skip..].to_vec();
            }
        }
        warn!(%job_id, timed_out = cap.timed_out, "build failed");
        return Ok(BuildOutcome::Failed(BuildFailure {
            job_id,
            exit_code: cap.status.and_then(|s| s.code()),
            timed_out: cap.timed_out,
            drv_path,
            log_tail,
            build_secs,
        }));
    }

    let (out_path, drv_path) = nixjson::parse_build_json(&cap.stdout)?;
    let info_json = nix_output(opts, &["path-info", "-r", "--json", &out_path]).await?;
    let closure = nixjson::parse_path_info(&info_json)?;
    if !closure.iter().any(|e| e.path == out_path) {
        return Err(anyhow!(
            "closure does not contain the build output {out_path}"
        ));
    }
    let closure_nar_size = closure.iter().filter_map(|e| e.nar_size).sum();
    info!(%job_id, %out_path, entries = closure.len(), "build succeeded");

    Ok(BuildOutcome::Succeeded(BuildSuccess {
        job_id,
        out_path,
        drv_path,
        closure,
        closure_nar_size,
        build_secs,
    }))
}
