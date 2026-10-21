//! `doum doctor`: checks the whole setup in one go and says what to fix. Read-only.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use crate::client::Client;
use crate::config::{Backend, Config, Paths};
use crate::daemon::StatusView;
use crate::slurm;

struct Report {
    problems: u32,
}

impl Report {
    fn ok(&self, what: &str, detail: &str) {
        println!("  \x1b[32m✓\x1b[0m {what:<22} {detail}");
    }
    fn info(&self, what: &str, detail: &str) {
        println!("  \x1b[2m–\x1b[0m {what:<22} {detail}");
    }
    fn bad(&mut self, what: &str, detail: &str) {
        self.problems += 1;
        println!("  \x1b[31m✗\x1b[0m {what:<22} {detail}");
    }
}

fn run_out(cmd: &str, args: &[&str]) -> Option<String> {
    let o = Command::new(cmd).args(args).output().ok()?;
    let mut s = String::from_utf8_lossy(&o.stdout).to_string();
    s.push_str(&String::from_utf8_lossy(&o.stderr));
    let s = s.trim().to_string();
    (o.status.success() || !s.is_empty()).then_some(s)
}

pub fn run(cfg: &Config, paths: &Paths) -> i32 {
    let mut r = Report { problems: 0 };
    println!("doum doctor\n");

    println!("Daemon");
    r.info("config", &paths.config.display().to_string());
    match Client::new(cfg, paths).and_then(|c| c.get::<StatusView>("/api/status")) {
        Ok(s) => r.ok(
            "daemon",
            &format!(
                "running on {} · {}/{} shards used · backend {:?}",
                cfg.listen, s.used_shards, s.total_shards, s.backend
            )
            .to_lowercase(),
        ),
        Err(_) => r.bad("daemon", "not running: `systemctl --user start doum` (or `doum daemon`)"),
    }
    match std::fs::metadata(&paths.state_dir) {
        Ok(m) if m.permissions().mode() & 0o077 == 0 => r.ok("state directory", "owner only (0700)"),
        Ok(m) => r.bad(
            "state directory",
            &format!(
                "mode {:o}: restart the daemon to make it owner only",
                m.permissions().mode() & 0o777
            ),
        ),
        Err(_) => r.bad("state directory", "missing"),
    }

    println!("\nGPU");
    match run_out("nvidia-smi", &["--query-gpu=name,driver_version", "--format=csv,noheader"]) {
        Some(s) if !s.is_empty() => r.ok("nvidia-smi", &s.replace(", ", " · driver ")),
        _ => r.bad(
            "nvidia-smi",
            "not found: Doum can still queue jobs, but can't see GPU processes, power or temperature",
        ),
    }
    // freezing jobs in place needs NVIDIA's cuda-checkpoint and a driver it supports (595+ on ARM)
    match run_out("cuda-checkpoint", &["--help"]) {
        None => r.info(
            "freeze",
            "not available: cuda-checkpoint is not installed (pausing stops and restarts jobs)",
        ),
        Some(s) if s.contains("Insufficient driver") => r.info(
            "freeze",
            "not available: cuda-checkpoint says this driver is too old (ARM needs 595+)",
        ),
        Some(_) => r.ok("freeze", "cuda-checkpoint works with this driver"),
    }

    println!("\nBackend");
    let slurm_up = slurm::available(&cfg.slurm);
    match (cfg.backend, slurm_up) {
        (Backend::Native, false) => r.ok("backend", "native (Doum runs background jobs itself)"),
        (Backend::Native, true) => r.ok(
            "backend",
            "native · Slurm is installed too: set `backend = \"slurm\"` to run jobs through it",
        ),
        (Backend::Slurm, false) => r.bad(
            "backend",
            "slurm, but `sinfo` fails: is slurmctld running? (`systemctl status slurmctld`)",
        ),
        (Backend::Slurm, true) => r.ok("backend", "slurm (Doum decides, Slurm runs)"),
    }
    if slurm_up {
        let parts = run_out(&cfg.slurm.sinfo, &["-h", "-o", "%P %a"]).unwrap_or_default();
        for p in [&cfg.slurm.interactive_partition, &cfg.slurm.batch_partition] {
            let up = parts.lines().any(|l| {
                l.trim_start_matches(|c: char| c.is_whitespace())
                    .replace('*', "")
                    .starts_with(&format!("{p} up"))
            });
            if up {
                r.ok(&format!("partition {p}"), "up");
            } else {
                r.bad(&format!("partition {p}"), "missing or down (see deploy/slurm)");
            }
        }
        let gres = run_out(&cfg.slurm.sinfo, &["-h", "-o", "%G"]).unwrap_or_default();
        if gres.contains(&format!("{}:", cfg.slurm.gres)) {
            r.ok("gpu shards", &gres);
        } else {
            r.bad(
                "gpu shards",
                &format!("no `{}` GRES in sinfo ({gres}): check gres.conf", cfg.slurm.gres),
            );
        }
    }

    println!("\nAlerts");
    r.info("desktop", if cfg.notify.desktop { "on" } else { "off" });
    if cfg.notify.ntfy_url.is_empty() {
        r.info("phone (ntfy)", "off: see docs/phone-alerts.md");
    } else {
        r.ok("phone (ntfy)", "on (topic set; kept private)");
    }
    r.info("webhooks", &format!("{}", cfg.notify.webhooks.len()));

    println!();
    if r.problems == 0 {
        println!("All good.");
        0
    } else {
        println!("{} problem(s) above.", r.problems);
        1
    }
}
