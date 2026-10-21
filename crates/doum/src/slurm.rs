//! Optional Slurm backend (`backend = "slurm"`). Doum still decides when a background job starts and what
//! pauses; Slurm runs it (`sbatch --gres=shard:N`), so it shows in `squeue`, gets Slurm's process tracking and
//! job records, and plain Slurm jobs submitted by hand count against the GPU too.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::config::{Paths, SlurmCfg};
use crate::job::{Job, Lane};

/// Slurm job names Doum gives its own jobs: `doum-<id>-<name>`.
pub const PREFIX: &str = "doum-";

/// A job in Slurm's queue, as `squeue` reports it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SlurmJob {
    pub id: u64,
    pub name: String,
    pub user: String,
    /// RUNNING, PENDING, COMPLETING, COMPLETED, FAILED, ...
    pub state: String,
    /// Why Slurm is holding it (`Resources`, `Priority`, `InvalidAccount`, ...), `None` when nothing.
    #[serde(default)]
    pub reason: String,
    /// GPU shards it holds or asks for (a whole `gpu` counts as every shard).
    pub shards: u32,
}

impl SlurmJob {
    pub fn is_doum(&self) -> bool {
        self.name.starts_with(PREFIX)
    }
    /// Still in Slurm's hands (waiting, starting, running or being stopped).
    pub fn is_active(&self) -> bool {
        matches!(
            self.state.as_str(),
            "PENDING"
                | "CONFIGURING"
                | "RUNNING"
                | "COMPLETING"
                | "SUSPENDED"
                | "REQUEUED"
                | "REQUEUE_HOLD"
                | "RESIZING"
                | "SIGNALING"
                | "STAGE_OUT"
        )
    }
    pub fn holds_gpu(&self) -> bool {
        matches!(
            self.state.as_str(),
            "CONFIGURING" | "RUNNING" | "COMPLETING" | "SUSPENDED" | "SIGNALING"
        )
    }
}

/// How long any Slurm command may take. The daemon calls these while it holds its state, so a stuck
/// slurmctld must not freeze Doum.
const TIMEOUT: Duration = Duration::from_secs(8);

/// Run a Slurm command with a time limit; `None` if it could not run or timed out (it is killed).
fn run(mut c: Command, input: Option<&str>) -> Option<Output> {
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = c.spawn().ok()?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(text.as_bytes());
    }
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Quote one argument for a POSIX shell.
fn q(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
        .take(40)
        .collect()
}

/// The batch script Slurm runs: `doum _exec`, which runs the pre-start checks and the command and records the exit.
pub fn script(exe: &str, job: &Job, mem_gb: f64) -> String {
    let mut a: Vec<String> = vec![
        exe.into(),
        "_exec".into(),
        "--id".into(),
        job.id.to_string(),
        "--attempt".into(),
        job.requeues.to_string(),
        "--name".into(),
        job.name.clone(),
        "--mem-gb".into(),
        format!("{mem_gb}"),
    ];
    if job.no_hooks {
        a.push("--no-hooks".into());
    }
    a.push("--".into());
    a.extend(job.cmd.iter().cloned());
    format!(
        "#!/bin/sh\n# submitted by doum (job {})\nexec {}\n",
        job.id,
        a.iter().map(|s| q(s)).collect::<Vec<_>>().join(" ")
    )
}

/// Submit a background job to Slurm. Returns the Slurm job id.
pub fn submit(cfg: &SlurmCfg, paths: &Paths, job: &Job, mem_gb: f64, api: &str, token: &str) -> Result<u64> {
    let exe = std::env::current_exe()?.display().to_string();
    let log = paths.log_file(job.id, &job.name).display().to_string();
    let partition = match job.lane {
        Lane::Interactive => &cfg.interactive_partition,
        Lane::Batch => &cfg.batch_partition,
    };
    let mut c = Command::new(&cfg.sbatch);
    c.args(["--parsable", "--job-name", &format!("{PREFIX}{}-{}", job.id, slug(&job.name))])
        .args(["--partition", partition])
        .args(["--gres", &format!("{}:{}", cfg.gres, job.shards)])
        .args(["--output", &log, "--open-mode=append", "--chdir", &job.cwd, "--export=ALL"])
        .args(&cfg.extra_args)
        // the job gets exactly the environment of the shell that ran `doum run`, plus how to reach the daemon
        .env_clear()
        .envs(&job.env)
        .env("DOUM_JOB_ID", job.id.to_string())
        .env("DOUM_API", api)
        .env("DOUM_TOKEN", token)
        .env("DOUM_CONFIG", &paths.config)
        .env("DOUM_STATE_DIR", &paths.state_dir);
    let out = run(c, Some(&script(&exe, job, mem_gb))).with_context(|| format!("{} did not answer", cfg.sbatch))?;
    if !out.status.success() {
        bail!("sbatch: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.trim()
        .split(';')
        .next()
        .unwrap_or("")
        .parse()
        .with_context(|| format!("sbatch printed {:?}", text.trim()))
}

/// Everything in Slurm's queue (all users). `None` if `squeue` could not be run, so callers can tell
/// "no jobs" from "don't know".
pub fn queue(cfg: &SlurmCfg, total_shards: u32) -> Option<Vec<SlurmJob>> {
    let mut c = Command::new(&cfg.squeue);
    c.args(["-h", "-a", "-o", "%i|%j|%u|%T|%r|%b"]);
    let out = run(c, None)?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| parse_line(l, &cfg.gres, total_shards))
            .collect(),
    )
}

/// One `squeue -o "%i|%j|%u|%T|%r|%b"` line. `%b` is the GRES request, e.g. `gres/shard:20`, `gres:shard:20`,
/// `gres/gpu:1` or `N/A`.
pub fn parse_line(line: &str, gres: &str, total_shards: u32) -> Option<SlurmJob> {
    let mut p = line.splitn(6, '|');
    let id = p.next()?.trim().split('_').next()?.parse().ok()?;
    let name = p.next()?.trim().to_string();
    let user = p.next()?.trim().to_string();
    let state = p.next()?.trim().to_string();
    let reason = p.next()?.trim().to_string();
    let tres = p.next().unwrap_or("").trim();
    let mut shards = 0;
    for part in tres.split(',') {
        let part = part.trim_start_matches("gres/").trim_start_matches("gres:");
        let mut f = part.split(':');
        let kind = f.next().unwrap_or("");
        let count: u32 = f.next_back().and_then(|c| c.parse().ok()).unwrap_or(1);
        if kind == gres {
            shards += count;
        } else if kind == "gpu" {
            shards = total_shards; // a whole GPU
        }
    }
    Some(SlurmJob {
        id,
        name,
        user,
        state,
        reason,
        shards: shards.min(total_shards),
    })
}

/// Signal a Slurm job: its batch script (which is `doum _exec`, and forwards to the command) and all its steps.
pub fn signal(cfg: &SlurmCfg, id: u64, sig: &str) {
    let mut c = Command::new(&cfg.scancel);
    c.args(["--full", &format!("--signal={sig}"), &id.to_string()]);
    let _ = run(c, None);
}

/// Cancel a job outright (one Slurm has not started yet can't be signalled).
pub fn cancel(cfg: &SlurmCfg, id: u64) {
    let mut c = Command::new(&cfg.scancel);
    c.arg(id.to_string());
    let _ = run(c, None);
}

/// `sinfo` works: Slurm is installed and the controller answers.
pub fn available(cfg: &SlurmCfg) -> bool {
    let mut c = Command::new(&cfg.sinfo);
    c.args(["-h", "-o", "%P"]);
    run(c, None).is_some_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::State;
    use crate::sched::tests::job;

    #[test]
    fn parses_squeue_lines() {
        let j = parse_line("41|doum-7-comfy|moe|RUNNING|None|gres/shard:40", "shard", 110).unwrap();
        assert_eq!((j.id, j.shards, j.is_doum(), j.holds_gpu()), (41, 40, true, true));
        let j = parse_line("42|train|alice|PENDING|Resources|gres:shard:20", "shard", 110).unwrap();
        assert_eq!((j.shards, j.is_doum(), j.is_active(), j.holds_gpu()), (20, false, true, false));
        assert_eq!(j.reason, "Resources");
        assert_eq!(parse_line("43|big|bob|RUNNING|None|gres/gpu:1", "shard", 110).unwrap().shards, 110);
        assert_eq!(parse_line("44|cpu|bob|RUNNING|None|N/A", "shard", 110).unwrap().shards, 0);
        assert_eq!(
            parse_line("45_3|array|bob|COMPLETED|None|gres/shard:2", "shard", 110).unwrap().id,
            45
        );
        assert!(parse_line("garbage", "shard", 110).is_none());
    }

    #[test]
    fn script_quotes_every_argument() {
        let mut j = job(9, Lane::Batch, 4, State::Pending, 0);
        j.name = "it's".into();
        j.cmd = vec!["python".into(), "train.py".into(), "--note".into(), "a b; rm -rf /".into()];
        let s = script("/usr/bin/doum", &j, 4.0);
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("exec '/usr/bin/doum' '_exec' '--id' '9'"));
        assert!(s.contains(r"'it'\''s'"));
        assert!(s.contains("'a b; rm -rf /'"));
    }
}
