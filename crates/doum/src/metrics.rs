//! A small in-memory history (last 24 h, one sample every 10 s) for charts, plus GPU energy per job.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const EVERY_S: u64 = 10;
const KEEP: usize = (24 * 3600 / EVERY_S) as usize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub t: u64,
    pub used_shards: u32,
    pub mem_free_gb: f64,
    pub gpu_c: Option<f64>,
    pub gpu_w: Option<f64>,
    /// Hottest thermal zone (the SoC on a GB10).
    pub soc_c: Option<f64>,
    pub running: u32,
    pub waiting: u32,
}

#[derive(Default)]
pub struct History {
    samples: VecDeque<Sample>,
}

impl History {
    pub fn push(&mut self, s: Sample) {
        if self.samples.len() >= KEEP {
            self.samples.pop_front();
        }
        self.samples.push_back(s);
    }
    pub fn since(&self, t: u64) -> Vec<Sample> {
        self.samples.iter().filter(|s| s.t >= t).cloned().collect()
    }
    pub fn last(&self) -> Option<&Sample> {
        self.samples.back()
    }
}

/// (GPU temperature °C, GPU power W) from nvidia-smi.
pub fn gpu_temp_power() -> (Option<f64>, Option<f64>) {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu,power.draw", "--format=csv,noheader,nounits"])
        .output();
    let Ok(out) = out else { return (None, None) };
    let s = String::from_utf8_lossy(&out.stdout);
    let f: Vec<Option<f64>> = s.lines().next().unwrap_or("").split(',').map(|v| v.trim().parse().ok()).collect();
    (f.first().copied().flatten(), f.get(1).copied().flatten())
}

/// Hottest thermal zone in °C.
pub fn soc_temp() -> Option<f64> {
    let mut best: Option<f64> = None;
    for e in std::fs::read_dir("/sys/class/thermal").ok()?.flatten() {
        if let Ok(v) = std::fs::read_to_string(e.path().join("temp")) {
            if let Ok(m) = v.trim().parse::<f64>() {
                let c = m / 1000.0;
                best = Some(best.map_or(c, |b: f64| b.max(c)));
            }
        }
    }
    best
}

/// Split `watts` over `dt` seconds between running jobs in proportion to their shards. Returns (job index, joules).
pub fn split_energy(shards: &[u32], watts: f64, dt: f64) -> Vec<f64> {
    let total: u32 = shards.iter().sum();
    if total == 0 {
        return vec![0.0; shards.len()];
    }
    shards.iter().map(|s| watts * dt * (*s as f64) / (total as f64)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_is_split_by_shards() {
        let e = split_energy(&[30, 90], 100.0, 10.0);
        assert_eq!(e, vec![250.0, 750.0]);
        assert!(split_energy(&[], 100.0, 10.0).is_empty());
    }

    #[test]
    fn history_is_bounded() {
        let mut h = History::default();
        for t in 0..(KEEP as u64 + 5) {
            h.push(Sample {
                t,
                used_shards: 0,
                mem_free_gb: 0.0,
                gpu_c: None,
                gpu_w: None,
                soc_c: None,
                running: 0,
                waiting: 0,
            });
        }
        assert_eq!(h.since(0).len(), KEEP);
        assert_eq!(h.last().unwrap().t, KEEP as u64 + 4);
    }
}
