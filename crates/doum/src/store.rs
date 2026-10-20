//! The daemon's state (jobs + open questions to the user), persisted atomically after every change so a
//! daemon restart loses nothing.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::job::{Job, JobId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DecisionKind {
    /// A new job does not fit: smart / wait / pause?
    Fit,
    /// A job has waited `max_wait_s`: pause-now / keep-waiting / cancel?
    CheckBack,
}

/// A question to the user. Answered from the desktop pop-up, the panel, the phone, the CLI or an agent (MCP);
/// if nobody answers by `deadline`, `recommended` is applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Decision {
    pub id: u64,
    pub job_id: JobId,
    pub kind: DecisionKind,
    pub text: String,
    pub options: Vec<String>,
    pub recommended: String,
    pub created_at: u64,
    pub deadline: u64,
    pub answer: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    pub next_id: u64,
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
}

impl Store {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                next_id: 1,
                ..Default::default()
            });
        }
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id.max(1);
        self.next_id = id + 1;
        id
    }

    pub fn job(&self, id: JobId) -> Option<&Job> {
        self.jobs.iter().find(|j| j.id == id)
    }
    pub fn job_mut(&mut self, id: JobId) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.id == id)
    }

    pub fn open_decisions(&self) -> impl Iterator<Item = &Decision> {
        self.decisions.iter().filter(|d| d.answer.is_none())
    }

    /// Finished jobs older than `keep_s` and answered decisions are dropped so the file stays small.
    pub fn prune(&mut self, now: u64, keep_s: u64) {
        self.jobs
            .retain(|j| !(j.state.is_terminal() && j.ended_at.is_some_and(|e| now.saturating_sub(e) > keep_s)));
        let live: Vec<JobId> = self.jobs.iter().map(|j| j.id).collect();
        self.decisions.retain(|d| d.answer.is_none() && live.contains(&d.job_id));
    }
}
