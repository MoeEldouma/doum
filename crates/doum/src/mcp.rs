//! `doum mcp`: an MCP server (stdio) so AI agents use the GPU queue with real tools instead of shell guesses.
//!
//! Agent-safe by default (config `[mcp]`): an agent's job is capped in shards, an agent can have only a few jobs
//! queued, it waits instead of pausing other people's work, it can only pause/cancel its own jobs, and it never
//! answers the user's GPU questions.

use anyhow::Result;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::client::Client;
use crate::config::{Config, Paths};
use crate::daemon::{fmt_dur, StatusView, SubmitResp};
use crate::job::{Job, Lane, State};

pub const OWNER: &str = "agent";

#[derive(Clone)]
pub struct DoumMcp {
    cfg: Arc<Config>,
    paths: Arc<Paths>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunParams {
    /// Short job name, e.g. "sdxl-batch" or "eval-qwen".
    pub name: String,
    /// Shell command to run (executed with `bash -lc`). It runs in the background; follow it with gpu_job / gpu_wait.
    pub command: String,
    /// GPU memory shards to reserve (~1 GB each). Ask for what the job really needs.
    pub shards: u32,
    /// Memory (GB) the job will really use, for the free-memory check. Optional.
    #[serde(default)]
    pub mem_gb: Option<f64>,
    /// Working directory (defaults to the agent's current directory).
    #[serde(default)]
    pub cwd: Option<String>,
    /// true if a person is waiting for the result right now (scheduled before batch work).
    #[serde(default)]
    pub interactive: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct JobParams {
    /// Job id.
    pub id: u64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WaitParams {
    /// Job id.
    pub id: u64,
    /// Seconds to wait at most (max 600). Returns early when the job starts running or finishes.
    #[serde(default)]
    pub timeout_s: Option<u64>,
    /// Wait until the job has finished (true) or only until it starts running (false, default).
    #[serde(default)]
    pub until_finished: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AnswerParams {
    /// Question id (from gpu_status).
    pub question: u64,
    /// One of the question's options.
    pub choice: String,
}

impl DoumMcp {
    async fn call<T, F>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(Client) -> Result<T> + Send + 'static,
    {
        let (cfg, paths) = (self.cfg.clone(), self.paths.clone());
        tokio::task::spawn_blocking(move || {
            let c = Client::new(&cfg, &paths)?;
            f(c)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
    }

    async fn own_job(&self, id: u64) -> Result<Job, String> {
        let j: Job = self.call(move |c| c.get(&format!("/api/jobs/{id}"))).await?;
        if j.owner != OWNER && !self.cfg.mcp.allow_control_others {
            return Err(format!("job {id} '{}' belongs to {}, not to an agent; ask the user to pause or cancel it", j.name, j.owner));
        }
        Ok(j)
    }
}

fn summarize(s: &StatusView) -> String {
    let mut out = vec![format!(
        "GPU: {}/{} shards used, {} free, {:.0} GB memory free.",
        s.used_shards,
        s.total_shards,
        s.total_shards - s.used_shards,
        s.mem_available_gb
    )];
    let running: Vec<&Job> = s.jobs.iter().filter(|j| j.state.holds_shards()).collect();
    out.push(format!("Running ({}):", running.len()));
    for j in running {
        let eta = j.eta_left(s.now).map(|e| format!(", ~{} left", fmt_dur(e))).unwrap_or_default();
        out.push(format!("  #{} {} [{} shards, {:?}, owner {}{eta}] {}", j.id, j.name, j.shards, j.lane, j.owner, j.progress.message));
    }
    out.push(format!("Waiting in start order ({}):", s.queue.len()));
    for id in &s.queue {
        if let Some(j) = s.jobs.iter().find(|j| j.id == *id) {
            out.push(format!("  #{} {} [{} shards, owner {}] {}", j.id, j.name, j.shards, j.owner, j.reason));
        }
    }
    for d in &s.decisions {
        out.push(format!("Open question #{} for the user: {} (options: {})", d.id, d.text.replace('\n', " "), d.options.join(", ")));
    }
    for p in &s.unmanaged {
        out.push(format!("Outside the queue: {} pid {} (~{:.1} GB)", p.name, p.pid, p.mem_gb));
    }
    out.join("\n")
}

fn tail(path: &str, n: usize) -> String {
    std::fs::read_to_string(path)
        .map(|t| {
            let lines: Vec<&str> = t.lines().collect();
            lines[lines.len().saturating_sub(n)..].join("\n")
        })
        .unwrap_or_default()
}

fn describe(j: &Job, now: u64) -> String {
    let mut s = format!("#{} {} — {:?}: {}. {} shards, lane {:?}.", j.id, j.name, j.state, j.reason, j.shards, j.lane);
    if !j.progress.message.is_empty() {
        s.push_str(&format!(" Progress: {}", j.progress.message));
        if let Some(e) = j.eta_left(now) {
            s.push_str(&format!(" (~{} left)", fmt_dur(e)));
        }
        s.push('.');
    }
    if let Some(c) = j.exit_code {
        s.push_str(&format!(" Exit code {c}."));
    }
    if let Some(log) = &j.log {
        let t = tail(log, 25);
        if !t.is_empty() {
            s.push_str(&format!("\nLast log lines:\n{t}"));
        }
    }
    s
}

#[tool_router]
impl DoumMcp {
    pub fn new(cfg: Config, paths: Paths) -> Self {
        Self { cfg: Arc::new(cfg), paths: Arc::new(paths) }
    }

    #[tool(
        description = "Show what is on the GPU: running jobs with progress and time left, the waiting queue in start order, open questions for the user, and GPU processes outside the queue. Call this before starting any GPU work."
    )]
    async fn gpu_status(&self) -> Result<String, String> {
        let s: StatusView = self.call(|c| c.get("/api/status")).await?;
        Ok(summarize(&s))
    }

    #[tool(
        description = "Run GPU work through the queue (in the background). It starts when enough shards are free, after the machine's safety checks. Never start GPU programs directly; use this. Returns the job id."
    )]
    async fn gpu_run(&self, Parameters(p): Parameters<RunParams>) -> Result<String, String> {
        let m = &self.cfg.mcp;
        if p.shards == 0 || p.shards > m.max_shards {
            return Err(format!("shards must be 1..={} for agent jobs (ask the user to raise [mcp] max_shards)", m.max_shards));
        }
        let s: StatusView = self.call(|c| c.get("/api/status")).await?;
        let mine = s.jobs.iter().filter(|j| j.owner == OWNER && !j.state.is_terminal()).count();
        if mine >= m.max_jobs as usize {
            return Err(format!("agents may have at most {} jobs queued or running; wait for one to finish", m.max_jobs));
        }
        let cwd = p.cwd.unwrap_or_else(|| std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_else(|_| "/".into()));
        let env: std::collections::BTreeMap<String, String> = std::env::vars().filter(|(k, _)| !k.starts_with("DOUM_")).collect();
        let body = json!({
            "name": p.name, "cmd": ["bash", "-lc", p.command], "cwd": cwd, "env": env, "shards": p.shards, "mem_gb": p.mem_gb,
            "lane": if p.interactive { Lane::Interactive } else { Lane::Batch }, "mode": m.mode, "detached": true,
            "no_pause": false, "owner": OWNER,
        });
        let r: SubmitResp = self.call(move |c| c.post("/api/jobs", &body)).await?;
        let mut msg = format!("Queued job #{} '{}' ({} shards): {}.", r.job.id, r.job.name, r.job.shards, r.job.reason);
        if let Some(e) = r.plan.and_then(|p| p.eta_s) {
            msg.push_str(&format!(" Expected start in ~{}.", fmt_dur(e)));
        }
        msg.push_str(" Use gpu_wait or gpu_job to follow it.");
        Ok(msg)
    }

    #[tool(description = "Details of one job: state, why it is waiting, progress, exit code and the last lines of its log.")]
    async fn gpu_job(&self, Parameters(p): Parameters<JobParams>) -> Result<String, String> {
        let id = p.id;
        let j: Job = self.call(move |c| c.get(&format!("/api/jobs/{id}"))).await?;
        Ok(describe(&j, crate::job::now()))
    }

    #[tool(description = "Wait (up to timeout_s, max 600) until a job starts running, or until it finishes if until_finished is true.")]
    async fn gpu_wait(&self, Parameters(p): Parameters<WaitParams>) -> Result<String, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(p.timeout_s.unwrap_or(120).min(600));
        let id = p.id;
        loop {
            let j: Job = self.call(move |c| c.get(&format!("/api/jobs/{id}"))).await?;
            let done = if p.until_finished { j.state.is_terminal() } else { j.state == State::Running || j.state.is_terminal() };
            if done || std::time::Instant::now() >= deadline {
                return Ok(describe(&j, crate::job::now()));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    #[tool(description = "Pause one of your (agent) jobs: it gets a signal to save and stop, and resumes on its own later.")]
    async fn gpu_pause(&self, Parameters(p): Parameters<JobParams>) -> Result<String, String> {
        let j = self.own_job(p.id).await?;
        let id = j.id;
        let _: serde_json::Value = self.call(move |c| c.post(&format!("/api/jobs/{id}/pause"), &json!({}))).await?;
        Ok(format!("Pausing job #{id} '{}'.", j.name))
    }

    #[tool(description = "Cancel one of your (agent) jobs, waiting or running.")]
    async fn gpu_cancel(&self, Parameters(p): Parameters<JobParams>) -> Result<String, String> {
        let j = self.own_job(p.id).await?;
        let id = j.id;
        let _: serde_json::Value = self.call(move |c| c.post(&format!("/api/jobs/{id}/cancel"), &json!({}))).await?;
        Ok(format!("Cancelled job #{id} '{}'.", j.name))
    }

    #[tool(
        description = "Answer an open GPU question, only if the user has allowed agents to (otherwise tell the user about the question instead)."
    )]
    async fn gpu_answer(&self, Parameters(p): Parameters<AnswerParams>) -> Result<String, String> {
        if !self.cfg.mcp.allow_answer {
            return Err("agents may not answer the user's GPU questions (config [mcp] allow_answer = false); tell the user about it".into());
        }
        let (q, choice) = (p.question, p.choice.clone());
        let _: serde_json::Value = self.call(move |c| c.post(&format!("/api/decisions/{q}"), &json!({ "choice": choice }))).await?;
        Ok(format!("Answered question #{} with '{}'.", p.question, p.choice))
    }
}

#[tool_handler(
    name = "doum",
    instructions = "doum is the GPU queue on this machine. Before any GPU work (model inference or serving, training, image/video generation, benchmarks), call gpu_status, then start the work with gpu_run instead of running it directly, and follow it with gpu_wait / gpu_job. Ask for the shards the job needs (~1 GB each). Do not pause or cancel other people's jobs; tell the user if the GPU is busy."
)]
impl ServerHandler for DoumMcp {}

pub fn serve(cfg: Config, paths: Paths) -> Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async move {
        let server = DoumMcp::new(cfg, paths).serve(rmcp::transport::stdio()).await?;
        server.waiting().await?;
        Ok(())
    })
}
