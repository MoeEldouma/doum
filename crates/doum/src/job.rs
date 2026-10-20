//! The job model shared by the daemon, the CLI, the panel and the MCP server.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type JobId = u64;

/// Which lane a job waits in. Interactive (someone is waiting at the screen) is always scheduled before batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    #[default]
    Batch,
    Interactive,
}

/// What to do when a job does not fit on the GPU right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Let jobs that finish soon complete; pause the long ones.
    #[default]
    Smart,
    /// Queue and wait; pause nothing.
    Wait,
    /// Pause whatever is in the way now.
    Pause,
    /// Ask the user (desktop / phone / panel); fall back to the recommendation after a timeout.
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Waiting for shards (or for a decision).
    Pending,
    /// Has its shards; running (or in its pre-start checks).
    Running,
    /// Asked to stop so it can be requeued; becomes Pending again once it has exited.
    Pausing,
    Done,
    Failed,
    Cancelled,
}

impl State {
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
    /// States that hold shards.
    pub fn holds_shards(self) -> bool {
        matches!(self, State::Running | State::Pausing)
    }
}

/// Progress a running job reports about itself (`doum progress`). Drives ETAs and smart decisions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Progress {
    pub message: String,
    /// Seconds left, as estimated by the job, at `updated_at`.
    pub eta_s: Option<u64>,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Job {
    pub id: JobId,
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub shards: u32,
    /// Memory (GB) the job will really use, for the free-memory check. Defaults from `shards`.
    #[serde(default)]
    pub mem_gb: Option<f64>,
    pub lane: Lane,
    pub mode: Mode,
    /// Detached jobs are run by the daemon and can be paused (stopped + requeued).
    /// Foreground jobs are run by the `doum run` process that submitted them and are never paused.
    pub detached: bool,
    /// The job (or its owner) said it must never be paused.
    pub no_pause: bool,
    pub owner: String,
    /// Skip the pre-start checks and hooks (for leases whose app runs its own checks, e.g. ComfyUI's guard plugin).
    #[serde(default)]
    pub no_hooks: bool,
    /// Processes this job stands for (e.g. a ComfyUI server holding a lease): not reported as outside the queue.
    #[serde(default)]
    pub covers_pids: Vec<i32>,
    pub state: State,
    pub submitted_at: u64,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    /// Moved to the front of its lane by the user.
    #[serde(default)]
    pub top: bool,
    /// How many times it was paused and requeued.
    #[serde(default)]
    pub requeues: u32,
    /// When it last became Pending (submit or requeue); drives the check-back timer.
    pub pending_since: u64,
    /// This job was paused to make room for that job; it stays held until that job has started.
    #[serde(default)]
    pub paused_for: Option<JobId>,
    /// Set while Pausing: when the stop signal was sent (SIGKILL after the grace period).
    #[serde(default)]
    pub pausing_since: Option<u64>,
    /// Ask the user after this many seconds still pending (0 = never).
    pub max_wait_s: u64,
    #[serde(default)]
    pub progress: Progress,
    pub exit_code: Option<i32>,
    /// GPU energy attributed to this job so far (joules): GPU power split between running jobs by shards.
    #[serde(default)]
    pub energy_j: f64,
    /// pid of the process group leader (detached) or of the `doum run` client (foreground).
    pub pid: Option<i32>,
    pub log: Option<String>,
    /// Why it is in its current state, in plain words ("waiting for 70 shards, 26 free").
    #[serde(default)]
    pub reason: String,
}

impl Job {
    /// Seconds left according to the job's last progress report, aged by the time since that report.
    pub fn eta_left(&self, now: u64) -> Option<u64> {
        let p = &self.progress;
        p.eta_s.map(|eta| eta.saturating_sub(now.saturating_sub(p.updated_at)))
    }

    pub fn pausable(&self) -> bool {
        self.detached && !self.no_pause && self.state == State::Running
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
