//! Running jobs.
//!
//! A detached job is started by the daemon as `doum _exec … -- <cmd>` in its own session/process group, so it
//! survives a daemon restart. A foreground job runs the same `_exec` steps inside the `doum run` process.
//! `_exec` does the pre-start checks (free memory, ComfyUI unload, user hooks such as a thermal gate), runs the
//! command in its own process group, forwards stop signals to it, and records the exit code on disk.

use anyhow::{Context, Result};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::unistd::Pid;
use std::fs::OpenOptions;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use crate::config::{Config, Paths};
use crate::job::Job;
use crate::sys;

/// Exit code when a pre-start hook refused to start the job.
pub const EXIT_PRESTART_FAILED: i32 = 97;

pub struct ExecArgs<'a> {
    pub id: u64,
    pub attempt: u32,
    pub name: &'a str,
    pub mem_gb: f64,
    pub no_hooks: bool,
    pub cmd: &'a [String],
}

/// Start a detached job. Returns the child (`_exec`, a session leader: its pid is the process group id).
pub fn spawn_detached(paths: &Paths, job: &Job, mem_gb: f64, api: &str, token: &str) -> Result<Child> {
    let exe = std::env::current_exe()?;
    let log = paths.log_file(job.id, &job.name);
    let out = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("log {}", log.display()))?;
    let err = out.try_clone()?;
    let mut c = Command::new(exe);
    c.arg("_exec")
        .args([
            "--id",
            &job.id.to_string(),
            "--attempt",
            &job.requeues.to_string(),
            "--name",
            &job.name,
        ])
        .args(["--mem-gb", &format!("{mem_gb}")])
        .args(if job.no_hooks { vec!["--no-hooks"] } else { vec![] })
        .arg("--")
        .args(&job.cmd)
        .env_clear()
        .envs(&job.env)
        .env("DOUM_JOB_ID", job.id.to_string())
        .env("DOUM_API", api)
        .env("DOUM_TOKEN", token)
        // the job's own doum commands (`_exec`, `doum progress`) must use the daemon's config and state dir
        .env("DOUM_CONFIG", &paths.config)
        .env("DOUM_STATE_DIR", &paths.state_dir)
        .current_dir(&job.cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    // New session: own process group, detached from the daemon's terminal and lifetime.
    unsafe {
        c.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::from)?;
            Ok(())
        });
    }
    c.spawn().context("spawning job")
}

/// Send a signal to a whole process group (the job's `_exec` group, or its command's group).
pub fn signal_group(pgid: i32, sig: Signal) {
    if pgid > 1 {
        let _ = signal::killpg(Pid::from_raw(pgid), sig);
    }
}

/// The command's own process group id for a running job (written by `_exec`), if known.
pub fn child_pgid(paths: &Paths, id: u64, attempt: u32) -> Option<i32> {
    std::fs::read_to_string(paths.state_dir.join("exit").join(format!("{id}.{attempt}.pgid")))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Exit code recorded by `_exec`, if the job finished.
pub fn read_exit(paths: &Paths, id: u64, attempt: u32) -> Option<i32> {
    std::fs::read_to_string(paths.exit_file(id, attempt)).ok()?.trim().parse().ok()
}

static CHILD_PGID: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: i32) {
    let pg = CHILD_PGID.load(Ordering::SeqCst);
    if pg > 1 {
        // killpg is async-signal-safe
        unsafe {
            libc_killpg(pg, sig);
        }
    }
}

unsafe fn libc_killpg(pg: i32, sig: i32) {
    if let Ok(s) = Signal::try_from(sig) {
        let _ = signal::killpg(Pid::from_raw(pg), s);
    }
}

fn say(msg: &str) {
    eprintln!("[doum] {msg}");
}

fn subst(args: &[String], a: &ExecArgs) -> Vec<String> {
    args.iter()
        .map(|s| s.replace("{name}", a.name).replace("{id}", &a.id.to_string()))
        .collect()
}

fn run_hook(hook: &[String], a: &ExecArgs, what: &str) -> bool {
    if hook.is_empty() {
        return true;
    }
    let h = subst(hook, a);
    say(&format!("{what}: {}", h.join(" ")));
    match Command::new(&h[0]).args(&h[1..]).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            say(&format!("{what} hook exited with {s}"));
            false
        }
        Err(e) => {
            say(&format!("{what} hook could not run: {e}"));
            false
        }
    }
}

/// Wait until enough memory is free for this job (unified memory: what the job will actually use).
fn wait_for_memory(cfg: &Config, a: &ExecArgs) {
    if !cfg.memory.check || a.mem_gb <= 0.0 {
        return;
    }
    let need = a.mem_gb + cfg.memory.headroom_gb;
    let mut tried_comfy = false;
    let mut last_note = Instant::now() - Duration::from_secs(3600);
    loop {
        let free = sys::mem_available_gb();
        if free >= need {
            say(&format!(
                "memory OK: {free:.0} GB free, job needs ~{:.0} GB + {:.0} GB headroom",
                a.mem_gb, cfg.memory.headroom_gb
            ));
            return;
        }
        if !tried_comfy && cfg.comfyui.free_when_idle && !cfg.comfyui.url.is_empty() {
            tried_comfy = true;
            if sys::comfy_free_if_idle(&cfg.comfyui.url) {
                say("ComfyUI was idle: asked it to unload its cached models");
                std::thread::sleep(Duration::from_secs(5));
                continue;
            }
        }
        if last_note.elapsed() >= Duration::from_secs(60) {
            say(&format!("WAIT for memory: {free:.0} GB free, need {need:.0} GB"));
            last_note = Instant::now();
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// The `_exec` steps. Returns the job's exit code (and records it on disk).
pub fn run(cfg: &Config, paths: &Paths, a: &ExecArgs) -> i32 {
    let code = run_inner(cfg, paths, a);
    let _ = std::fs::write(paths.exit_file(a.id, a.attempt), code.to_string());
    if !a.no_hooks {
        run_hook(&cfg.hooks.post_end, a, "post-end");
    }
    code
}

fn run_inner(cfg: &Config, paths: &Paths, a: &ExecArgs) -> i32 {
    if a.cmd.is_empty() {
        say("no command");
        return 2;
    }
    // Stop signals that arrive during the checks end the job before the command starts.
    if !a.no_hooks {
        wait_for_memory(cfg, a);
        if !run_hook(&cfg.hooks.pre_start, a, "pre-start") {
            return EXIT_PRESTART_FAILED;
        }
    }
    let mut c = Command::new(&a.cmd[0]);
    c.args(&a.cmd[1..]).process_group(0);
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => {
            say(&format!("could not start {}: {e}", a.cmd[0]));
            return 127;
        }
    };
    let pg = child.id() as i32;
    CHILD_PGID.store(pg, Ordering::SeqCst);
    let _ = std::fs::write(
        paths.state_dir.join("exit").join(format!("{}.{}.pgid", a.id, a.attempt)),
        pg.to_string(),
    );
    for s in [Signal::SIGTERM, Signal::SIGINT, Signal::SIGHUP, Signal::SIGUSR1] {
        unsafe {
            let _ = signal::signal(s, SigHandler::Handler(forward));
        }
    }
    say(&format!("started '{}' (pid {pg})", a.name));
    let status = loop {
        match child.wait() {
            Ok(s) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                say(&format!("wait failed: {e}"));
                return 1;
            }
        }
    };
    CHILD_PGID.store(0, Ordering::SeqCst);
    let code = status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
    say(&format!("'{}' exited with {code}", a.name));
    code
}
