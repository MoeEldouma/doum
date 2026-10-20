//! Facts about the machine: free memory, process liveness, GPU processes that bypass the queue, ComfyUI.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// MemAvailable in GB. On unified-memory machines (GB10 / DGX Spark) this is the real GPU headroom.
pub fn mem_available_gb() -> f64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("MemAvailable:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|kb| kb / 1_048_576.0)
        .unwrap_or(0.0)
}

/// Alive and not a zombie.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s.rsplit(')').next().map(|r| !r.trim_start().starts_with('Z')).unwrap_or(false),
        Err(_) => false,
    }
}

fn stat_fields(pid: i32) -> Option<Vec<String>> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = s.rsplit_once(')')?.1;
    Some(rest.split_whitespace().map(String::from).collect())
}
pub fn ppid(pid: i32) -> Option<i32> {
    stat_fields(pid)?.get(1)?.parse().ok()
}
pub fn pgid(pid: i32) -> Option<i32> {
    stat_fields(pid)?.get(2)?.parse().ok()
}

/// True if `pid` is `root` or one of its descendants.
pub fn descends_from(mut pid: i32, root: i32) -> bool {
    for _ in 0..64 {
        if pid == root {
            return true;
        }
        match ppid(pid) {
            Some(p) if p > 1 => pid = p,
            _ => return false,
        }
    }
    false
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GpuProc {
    pub pid: i32,
    pub name: String,
    /// GB. From nvidia-smi when the driver reports it; on unified memory (reported "N/A") the process RSS.
    pub mem_gb: f64,
}

fn rss_gb(pid: i32) -> f64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .map(|kb| kb / 1_048_576.0)
        .unwrap_or(0.0)
}

/// Processes currently using the GPU (via nvidia-smi). Empty if nvidia-smi is missing.
pub fn gpu_procs() -> Vec<GpuProc> {
    let out = std::process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,process_name,used_gpu_memory",
            "--format=csv,noheader,nounits",
        ])
        .output();
    let Ok(out) = out else { return vec![] };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            let pid: i32 = f.first()?.parse().ok()?;
            let name = f.get(1).unwrap_or(&"?").rsplit('/').next().unwrap_or("?").to_string();
            let mem_gb = f
                .get(2)
                .and_then(|m| m.parse::<f64>().ok())
                .map(|mib| mib / 1024.0)
                .unwrap_or_else(|| rss_gb(pid));
            Some(GpuProc { pid, name, mem_gb })
        })
        .collect()
}

/// ComfyUI queue state: (running, pending), or None if ComfyUI is not reachable.
pub fn comfy_queue(url: &str) -> Option<(usize, usize)> {
    let c = reqwest::blocking::Client::builder().timeout(Duration::from_secs(3)).build().ok()?;
    let v: serde_json::Value = c.get(format!("{url}/queue")).send().ok()?.json().ok()?;
    let n = |k: &str| v.get(k).and_then(|a| a.as_array()).map_or(0, |a| a.len());
    Some((n("queue_running"), n("queue_pending")))
}

/// Ask an idle ComfyUI to unload its cached models. Returns true if it was idle and accepted.
pub fn comfy_free_if_idle(url: &str) -> bool {
    match comfy_queue(url) {
        Some((0, 0)) => {
            let c = reqwest::blocking::Client::builder().timeout(Duration::from_secs(10)).build();
            c.ok()
                .and_then(|c| {
                    c.post(format!("{url}/free"))
                        .json(&serde_json::json!({"unload_models": true, "free_memory": true}))
                        .send()
                        .ok()
                })
                .is_some_and(|r| r.status().is_success())
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_is_alive_and_descends_from_itself() {
        let me = std::process::id() as i32;
        assert!(pid_alive(me));
        assert!(descends_from(me, me));
        assert!(!pid_alive(-1));
    }

    #[test]
    fn meminfo_reads_something() {
        assert!(mem_available_gb() > 0.0);
    }
}
