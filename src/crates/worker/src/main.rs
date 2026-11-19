use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use common::JobSpec;
use tokio::sync::mpsc;
use worker::cache::CacheConfig;
use worker::runner::{BuildOutcome, RunOptions, run_job};

#[derive(Parser)]
#[command(name = "bamix-worker", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Validate a job spec and write the generated default.nix/build.sh/install.sh
    /// to a directory, without building (debugging / golden files).
    Render {
        /// Path to job.yaml
        spec: PathBuf,
        /// Output directory
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a job spec locally and print the store path and closure.
    Run {
        /// Path to job.yaml
        spec: PathBuf,
        /// nix binary to use
        #[arg(long, default_value = "nix")]
        nix_bin: PathBuf,
        /// Base directory for per-job work dirs
        #[arg(long)]
        work_dir: Option<PathBuf>,
        /// Kill the build after this many seconds
        #[arg(long, default_value_t = 3600)]
        timeout_secs: u64,
        /// Keep the rendered work dir after the run (debugging)
        #[arg(long)]
        keep_workdir: bool,
        /// Skip the `sandbox = true` preflight (development only)
        #[arg(long)]
        no_sandbox_check: bool,
        /// Push results here after a successful build, e.g. ssh-ng://bamix-push@laptop
        #[arg(long, requires = "signing_key")]
        cache_uri: Option<String>,
        /// Secret key used to sign results before pushing
        #[arg(long, requires = "cache_uri")]
        signing_key: Option<PathBuf>,
        /// Print the outcome as JSON on stdout (build logs always go to stderr)
        #[arg(long)]
        json: bool,
    },
}

/// Exit codes: 0 built, 1 job failed/timed out, 2 infrastructure error.
#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match real_main().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

async fn real_main() -> Result<ExitCode> {
    let (spec, nix_bin, work_dir, timeout_secs, keep_workdir, no_sandbox_check, json, cache) =
        match Cli::parse().cmd {
            Cmd::Render { spec, out } => {
                let spec = JobSpec::from_path(&spec)?;
                common::render(&spec)?.write_to(&out)?;
                println!("wrote {}", out.display());
                return Ok(ExitCode::SUCCESS);
            }
            Cmd::Run {
                spec,
                nix_bin,
                work_dir,
                timeout_secs,
                keep_workdir,
                no_sandbox_check,
                cache_uri,
                signing_key,
                json,
            } => (
                spec,
                nix_bin,
                work_dir,
                timeout_secs,
                keep_workdir,
                no_sandbox_check,
                json,
                cache_uri
                    .zip(signing_key)
                    .map(|(store_uri, signing_key)| CacheConfig {
                        store_uri,
                        signing_key,
                    }),
            ),
        };

    let spec = JobSpec::from_path(&spec)?;
    let mut opts = RunOptions {
        nix_bin,
        timeout: Duration::from_secs(timeout_secs),
        keep_workdir,
        require_sandbox: !no_sandbox_check,
        cache,
        ..RunOptions::default()
    };
    if let Some(d) = work_dir {
        opts.work_root = d;
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let printer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            eprintln!("| {line}");
        }
    });

    let outcome = run_job(&spec, &opts, Some(tx)).await;
    let _ = printer.await;
    let outcome = outcome?;

    if json {
        println!("{}", serde_json::to_string_pretty(&outcome)?);
    } else {
        match &outcome {
            BuildOutcome::Succeeded(s) => {
                println!("out:     {}", s.out_path);
                println!(
                    "closure: {} paths, {} bytes (nar)",
                    s.closure.len(),
                    s.closure_nar_size
                );
                println!("time:    {:.1}s", s.build_secs);
                if let Some(c) = &s.pushed_to {
                    println!("pushed:  {c}");
                }
            }
            BuildOutcome::Failed(f) => {
                println!(
                    "FAILED (exit {:?}{}) after {:.1}s",
                    f.exit_code,
                    if f.timed_out { ", timed out" } else { "" },
                    f.build_secs
                );
                for l in &f.log_tail {
                    println!("  {l}");
                }
            }
        }
    }
    Ok(match outcome {
        BuildOutcome::Succeeded(_) => ExitCode::SUCCESS,
        BuildOutcome::Failed(_) => ExitCode::from(1),
    })
}
