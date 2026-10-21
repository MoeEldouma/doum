//! The doum daemon: owns the queue, starts / pauses / requeues jobs, asks the user when the GPU is busy,
//! and serves the local API (CLI, panel, MCP) on 127.0.0.1.

use anyhow::Result;
use axum::extract::{Path, State as AxState};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use nix::sys::signal::Signal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::{Backend, Config, Paths};
use crate::exec;
use crate::job::{now, Job, JobId, Lane, Mode, Progress, State};
use crate::metrics::{self, History, Sample};
use crate::notify::{self, Kind, Notifier};
use crate::sched::{self, Plan};
use crate::slurm;
use crate::store::{Decision, DecisionKind, Store};
use crate::sys;

pub struct Daemon {
    pub cfg: Config,
    pub paths: Paths,
    pub store: Store,
    pub token: String,
    pub api: String,
    children: HashMap<JobId, Child>,
    notifier: Notifier,
    known_unmanaged: HashSet<i32>,
    unmanaged_cache: Vec<sys::GpuProc>,
    /// Slurm's queue (all users), refreshed every tick under the Slurm backend.
    slurm_jobs: Vec<slurm::SlurmJob>,
    /// The last `squeue` worked (so a job missing from it has really left Slurm).
    slurm_seen: bool,
    last_gpu_scan: u64,
    last_prune: u64,
    last_sample: u64,
    last_replan: u64,
    /// The notifier's alert count when the state was last saved.
    saved_alerts: u64,
    pub history: History,
    dirty: bool,
}

type Shared = Arc<Mutex<Daemon>>;

// ---------- API types ----------

#[derive(Debug, Deserialize, Serialize, Default)]
pub struct SubmitReq {
    pub name: String,
    pub cmd: Vec<String>,
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub shards: u32,
    #[serde(default)]
    pub mem_gb: Option<f64>,
    #[serde(default)]
    pub lane: Lane,
    #[serde(default)]
    pub mode: Option<Mode>,
    #[serde(default)]
    pub detached: bool,
    #[serde(default)]
    pub no_pause: bool,
    #[serde(default)]
    pub max_wait_s: Option<u64>,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub no_hooks: bool,
    #[serde(default)]
    pub covers_pids: Vec<i32>,
    /// Foreground jobs: the pid of the `doum run` client that will run the command.
    #[serde(default)]
    pub client_pid: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SubmitResp {
    pub job: Job,
    pub plan: Option<PlanView>,
    pub decision: Option<Decision>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PlanView {
    pub mode: Mode,
    pub pause: Vec<JobId>,
    pub wait_for: Vec<JobId>,
    pub eta_s: Option<u64>,
    pub free_now: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StatusView {
    pub total_shards: u32,
    pub used_shards: u32,
    pub mem_available_gb: f64,
    pub jobs: Vec<Job>,
    pub queue: Vec<JobId>,
    pub decisions: Vec<Decision>,
    pub unmanaged: Vec<sys::GpuProc>,
    #[serde(default)]
    pub latest: Option<Sample>,
    /// `native` or `slurm`.
    #[serde(default)]
    pub backend: Backend,
    /// Recent alerts, newest first.
    #[serde(default)]
    pub alerts: Vec<notify::AlertRec>,
    /// Slurm jobs that are not Doum's (submitted with plain sbatch/srun), with the shards they hold.
    #[serde(default)]
    pub external: Vec<slurm::SlurmJob>,
    pub now: u64,
}

#[derive(Debug, Deserialize)]
pub struct ProgressReq {
    pub message: String,
    #[serde(default)]
    pub eta_s: Option<u64>,
    #[serde(default)]
    pub no_pause: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct DoneReq {
    pub code: i32,
}

#[derive(Debug, Deserialize)]
pub struct AnswerReq {
    pub choice: String,
    /// The question's one-time code (phone answers).
    #[serde(default)]
    pub code: Option<String>,
    /// Where the answer came from: "phone", "desktop", or empty.
    #[serde(default)]
    pub from: Option<String>,
}

// ---------- core ----------

impl Daemon {
    pub fn new(cfg: Config, paths: Paths) -> Result<Self> {
        let store = Store::load(&paths.state_file())?;
        let token = load_or_create_token(&paths)?;
        let api = format!("http://{}", cfg.listen);
        let notifier = Notifier::new(cfg.notify.clone(), store.alerts.clone());
        Ok(Self {
            cfg,
            paths,
            store,
            token,
            api,
            children: HashMap::new(),
            notifier,
            known_unmanaged: HashSet::new(),
            unmanaged_cache: vec![],
            slurm_jobs: vec![],
            slurm_seen: false,
            last_gpu_scan: 0,
            last_prune: 0,
            last_sample: 0,
            last_replan: 0,
            saved_alerts: 0,
            history: History::default(),
            dirty: true,
        })
    }

    /// The `[mcp]` limits for jobs submitted by AI agents, enforced here as well as in the MCP server, so an agent job
    /// that reaches the API another way gets the same rules. (Agents run as you, so this is a guard rail for
    /// well-behaved agents, not a security boundary.)
    pub fn agent_limits(&self, r: &mut SubmitReq) -> Result<(), String> {
        let m = &self.cfg.mcp;
        if r.shards == 0 || r.shards > m.max_shards {
            return Err(format!("agent jobs may use 1..={} shards ([mcp] max_shards)", m.max_shards));
        }
        let open = self
            .store
            .jobs
            .iter()
            .filter(|j| j.owner == crate::mcp::OWNER && !j.state.is_terminal())
            .count();
        if open >= m.max_jobs as usize {
            return Err(format!(
                "agents may have at most {} jobs queued or running ([mcp] max_jobs)",
                m.max_jobs
            ));
        }
        r.mode = Some(m.mode);
        r.no_pause = false;
        Ok(())
    }

    /// Shards Doum can hand out: the configured total minus what plain Slurm jobs (not Doum's) hold.
    fn capacity(&self) -> u32 {
        self.cfg.total_shards.saturating_sub(self.external_shards())
    }

    fn external_shards(&self) -> u32 {
        self.slurm_jobs
            .iter()
            .filter(|j| !j.is_doum() && j.holds_gpu())
            .map(|j| j.shards)
            .sum()
    }

    fn mem_for(&self, j: &Job) -> f64 {
        j.mem_gb.unwrap_or_else(|| {
            let total = (self.cfg.total_shards as f64) * self.cfg.gb_per_shard;
            (j.shards as f64 * self.cfg.gb_per_shard).min(total * 0.6)
        })
    }

    /// A job in one short phrase: "comfyui (60 shards, done in ~4 min, rendering 2/5)".
    fn describe(&self, id: JobId, t: u64) -> String {
        match self.store.job(id) {
            Some(j) => {
                let eta = match j.eta_left(t) {
                    Some(0) => "finishing".to_string(),
                    Some(s) => format!("done in ~{}", fmt_dur(s)),
                    None => "no estimate".to_string(),
                };
                let msg: String = j.progress.message.trim().chars().take(40).collect();
                let msg = if msg.is_empty() { String::new() } else { format!(", {msg}") };
                format!("{} ({} shards, {eta}{msg})", j.name, j.shards)
            }
            None => format!("job {id}"),
        }
    }

    fn names(&self, ids: &[JobId]) -> String {
        ids.iter()
            .filter_map(|i| self.store.job(*i).map(|j| j.name.clone()))
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn submit(&mut self, r: SubmitReq) -> SubmitResp {
        let t = now();
        let id = self.store.next_id();
        let mode = r.mode.unwrap_or(self.cfg.default_mode);
        let job = Job {
            id,
            name: if r.name.is_empty() {
                r.cmd.first().cloned().unwrap_or_else(|| "job".into())
            } else {
                r.name
            },
            cmd: r.cmd,
            cwd: r.cwd,
            env: r.env,
            shards: r.shards.clamp(1, self.cfg.total_shards),
            mem_gb: r.mem_gb,
            lane: r.lane,
            mode,
            detached: r.detached,
            no_pause: r.no_pause || !r.detached,
            owner: r.owner,
            no_hooks: r.no_hooks,
            covers_pids: r.covers_pids,
            state: State::Pending,
            submitted_at: t,
            started_at: None,
            ended_at: None,
            top: false,
            requeues: 0,
            pending_since: t,
            pausing_since: None,
            paused_for: None,
            runs: vec![],
            max_wait_s: r.max_wait_s.unwrap_or(self.cfg.max_wait_s),
            progress: Progress::default(),
            exit_code: None,
            energy_j: 0.0,
            slurm_id: None,
            pid: r.client_pid,
            log: None,
            reason: "queued".into(),
        };
        let need = job.shards;
        let lane = job.lane;
        self.store.jobs.push(job);
        self.dirty = true;

        let p = sched::plan(
            &self.store.jobs,
            self.capacity(),
            need,
            lane,
            mode,
            t,
            self.cfg.smart_wait_s,
            self.cfg.pause_grace_s,
        );
        let mut plan_view = None;
        let mut decision = None;
        if need > p.free_now {
            match mode {
                Mode::Ask => decision = Some(self.open_fit_question(id, need, &p)),
                Mode::Smart | Mode::Pause | Mode::Wait => {
                    self.apply_plan(id, &p);
                    plan_view = Some(view(mode, &p));
                }
            }
        }
        self.set_reason(id);
        let job = self.store.job(id).cloned().expect("just added");
        SubmitResp {
            job,
            plan: plan_view,
            decision,
        }
    }

    fn open_fit_question(&mut self, id: JobId, need: u32, p: &Plan) -> Decision {
        let lane = self.store.job(id).map(|j| j.lane).unwrap_or(Lane::Batch);
        let t = now();
        let jobs = &self.store.jobs;
        let total = self.capacity();
        let smart = sched::plan(
            jobs,
            total,
            need,
            lane,
            Mode::Smart,
            t,
            self.cfg.smart_wait_s,
            self.cfg.pause_grace_s,
        );
        let pause = sched::plan(
            jobs,
            total,
            need,
            lane,
            Mode::Pause,
            t,
            self.cfg.smart_wait_s,
            self.cfg.pause_grace_s,
        );
        let wait = sched::plan(
            jobs,
            total,
            need,
            lane,
            Mode::Wait,
            t,
            self.cfg.smart_wait_s,
            self.cfg.pause_grace_s,
        );
        let in_way: Vec<String> = jobs
            .iter()
            .filter(|j| j.state.holds_shards())
            .map(|j| self.describe(j.id, t))
            .collect();
        let name = self.store.job(id).map(|j| j.name.clone()).unwrap_or_default();
        let when = |p: &Plan| match p.eta_s {
            Some(0) => "right away".to_string(),
            Some(s) => format!("in about {}", fmt_dur(s)),
            None => "when they finish (no estimate)".to_string(),
        };
        let smart_line = match (smart.pause.is_empty(), smart.wait_for.is_empty()) {
            (true, true) => format!("Smart: start {}", when(&smart)),
            (true, false) => format!("Smart: wait for {}, start {}", self.names(&smart.wait_for), when(&smart)),
            (false, true) => format!("Smart: pause {}, start {}", self.names(&smart.pause), when(&smart)),
            (false, false) => format!(
                "Smart: pause {}, wait for {}, start {}",
                self.names(&smart.pause),
                self.names(&smart.wait_for),
                when(&smart)
            ),
        };
        let pause_line = if pause.pause.is_empty() {
            format!("Pause now: nothing can be paused, start {}", when(&pause))
        } else {
            format!("Pause now: pause {}, start {}", self.names(&pause.pause), when(&pause))
        };
        let choices = vec![smart_line, format!("Wait: pause nothing, start {}", when(&wait)), pause_line];
        let text = format!(
            "Needs {need} of {} shards; {} free.\nIn the way: {}.\n\n{}",
            self.cfg.total_shards,
            p.free_now,
            if in_way.is_empty() {
                "jobs ahead in the queue".to_string()
            } else {
                in_way.join(", ")
            },
            choices.join("\n")
        );
        let d = Decision {
            id: self.store.next_id(),
            job_id: id,
            kind: DecisionKind::Fit,
            title: format!("{name} is waiting for the GPU"),
            text,
            choices,
            options: vec!["smart".into(), "wait".into(), "pause".into()],
            recommended: "smart".into(),
            code: new_code(),
            created_at: t,
            deadline: t + self.cfg.ask_timeout_s,
            answer: None,
        };
        self.store.decisions.push(d.clone());
        self.notifier.ask(&d, &self.api, &self.token);
        self.dirty = true;
        d
    }

    fn open_checkback(&mut self, id: JobId) {
        let t = now();
        let Some(j) = self.store.job(id) else { return };
        let waited = t.saturating_sub(j.pending_since);
        let p = sched::plan(
            &self.store.jobs,
            self.capacity(),
            j.shards,
            j.lane,
            Mode::Wait,
            t,
            self.cfg.smart_wait_s,
            self.cfg.pause_grace_s,
        );
        let blockers: Vec<String> = p.wait_for.iter().map(|i| self.describe(*i, t)).collect();
        let pausable = sched::plan(
            &self.store.jobs,
            self.capacity(),
            j.shards,
            j.lane,
            Mode::Pause,
            t,
            self.cfg.smart_wait_s,
            self.cfg.pause_grace_s,
        );
        let name = j.name.clone();
        let choices = vec![
            if pausable.pause.is_empty() {
                "Pause now: nothing can be paused".to_string()
            } else {
                format!("Pause now: pause {}", self.names(&pausable.pause))
            },
            format!("Keep waiting: ask again in {}", fmt_dur(j.max_wait_s.max(60))),
            format!("Cancel: stop waiting, cancel {name}"),
        ];
        let text = format!(
            "Still in the way: {}.\n\n{}",
            if blockers.is_empty() {
                "jobs ahead in the queue".to_string()
            } else {
                blockers.join(", ")
            },
            choices.join("\n")
        );
        let d = Decision {
            id: self.store.next_id(),
            job_id: id,
            kind: DecisionKind::CheckBack,
            title: format!("{name} has waited {}", fmt_dur(waited)),
            text,
            choices,
            options: vec!["pause-now".into(), "keep-waiting".into(), "cancel".into()],
            recommended: "keep-waiting".into(),
            code: new_code(),
            created_at: t,
            deadline: t + self.cfg.ask_timeout_s.max(300),
            answer: None,
        };
        self.store.decisions.push(d.clone());
        self.notifier.ask(&d, &self.api, &self.token);
        self.dirty = true;
    }

    /// Answer a question. `code` must match the question's one-time code when given (phone answers carry it);
    /// `from` is "phone", "desktop", "timeout" or empty. A phone answer and a timed-out question each get a
    /// short follow-up alert saying what was chosen.
    pub fn answer_from(&mut self, did: u64, choice: &str, code: Option<&str>, from: &str) -> Result<(), String> {
        let d = self
            .store
            .decisions
            .iter()
            .find(|d| d.id == did)
            .cloned()
            .ok_or("no such question")?;
        if let Some(c) = code {
            if d.code.is_empty() || c != d.code {
                return Err("this answer's code does not match the question".into());
            }
        }
        self.apply_answer(did, choice)?;
        let name = self.store.job(d.job_id).map(|j| j.name.clone()).unwrap_or_default();
        let line = d
            .options
            .iter()
            .position(|o| o == choice)
            .and_then(|i| d.choices.get(i))
            .cloned()
            .unwrap_or_default();
        match from {
            "phone" => self.notifier.alert(
                Kind::Decided,
                &format!("{} for {name}", notify::label(choice)),
                &format!("Answered from your phone. {line}"),
            ),
            "timeout" => self
                .notifier
                .alert(Kind::Decided, &format!("No answer: {} for {name}", notify::label(choice)), &line),
            _ => {}
        }
        Ok(())
    }

    fn apply_answer(&mut self, did: u64, choice: &str) -> Result<(), String> {
        let d = self
            .store
            .decisions
            .iter()
            .find(|d| d.id == did)
            .cloned()
            .ok_or("no such question")?;
        if d.answer.is_some() {
            return Err("already answered".into());
        }
        if !d.options.iter().any(|o| o == choice) {
            return Err(format!("choose one of: {}", d.options.join(", ")));
        }
        if let Some(x) = self.store.decisions.iter_mut().find(|x| x.id == did) {
            x.answer = Some(choice.to_string());
        }
        let t = now();
        match (d.kind, choice) {
            (DecisionKind::Fit, c) => {
                let mode = match c {
                    "wait" => Mode::Wait,
                    "pause" => Mode::Pause,
                    _ => Mode::Smart,
                };
                if let Some(j) = self.store.job(d.job_id).cloned() {
                    if j.state == State::Pending {
                        let p = sched::plan(
                            &self.store.jobs,
                            self.capacity(),
                            j.shards,
                            j.lane,
                            mode,
                            t,
                            self.cfg.smart_wait_s,
                            self.cfg.pause_grace_s,
                        );
                        self.apply_plan(j.id, &p);
                    }
                }
            }
            (DecisionKind::CheckBack, "pause-now") => {
                if let Some(j) = self.store.job(d.job_id).cloned() {
                    let p = sched::plan(
                        &self.store.jobs,
                        self.capacity(),
                        j.shards,
                        j.lane,
                        Mode::Pause,
                        t,
                        self.cfg.smart_wait_s,
                        self.cfg.pause_grace_s,
                    );
                    self.apply_plan(j.id, &p);
                }
            }
            (DecisionKind::CheckBack, "keep-waiting") => {
                if let Some(j) = self.store.job_mut(d.job_id) {
                    j.pending_since = t;
                }
            }
            (DecisionKind::CheckBack, "cancel") => {
                let _ = self.cancel(d.job_id);
            }
            _ => {}
        }
        self.set_reason(d.job_id);
        self.dirty = true;
        Ok(())
    }

    fn apply_plan(&mut self, for_job: JobId, p: &Plan) {
        self.apply_plan_why(for_job, p, "")
    }

    /// `why` is added in front of each paused job's reason, e.g. "ran past its estimate; ".
    fn apply_plan_why(&mut self, for_job: JobId, p: &Plan, why: &str) {
        if p.pause.is_empty() {
            return;
        }
        let name = self.store.job(for_job).map(|j| j.name.clone()).unwrap_or_default();
        // The job we make room for goes to the front of its lane, and the paused jobs wait until it has
        // started, so they cannot take the space straight back.
        if let Some(j) = self.store.job_mut(for_job) {
            j.top = true;
        }
        for id in p.pause.clone() {
            let lead = if why.is_empty() || !self.store.job(id).is_some_and(|j| j.overran(now())) {
                ""
            } else {
                why
            };
            if self.pause(id, &format!("{lead}paused to make room for '{name}'")).is_ok() {
                if let Some(j) = self.store.job_mut(id) {
                    j.paused_for = Some(for_job);
                }
            }
        }
    }

    pub fn pause(&mut self, id: JobId, why: &str) -> Result<(), String> {
        let t = now();
        let j = self.store.job_mut(id).ok_or("no such job")?;
        if !j.pausable() {
            return Err(if j.detached {
                "job is not running or asked not to be paused".into()
            } else {
                "foreground jobs cannot be paused (cancel instead)".into()
            });
        }
        j.state = State::Pausing;
        j.pausing_since = Some(t);
        j.reason = format!("{why}: saving and stopping");
        if let Some(sid) = j.slurm_id {
            stop_slurm_job(&self.cfg.slurm, &self.slurm_jobs, sid);
        } else if let Some(pg) = j.pid {
            exec::signal_group(pg, Signal::SIGTERM);
        }
        let (name, msg) = (j.name.clone(), why.to_string());
        let mut why = msg.replacen("paused to make room", "to make room", 1);
        if let Some(c) = why.get(..1) {
            why = c.to_uppercase() + &why[1..];
        }
        self.notifier.alert(
            Kind::Paused,
            &format!("Paused {name}"),
            &format!("{why}. It resumes on its own when the GPU is free."),
        );
        self.dirty = true;
        Ok(())
    }

    pub fn cancel(&mut self, id: JobId) -> Result<(), String> {
        let t = now();
        let j = self.store.job_mut(id).ok_or("no such job")?;
        match j.state {
            State::Pending => {
                j.state = State::Cancelled;
                j.ended_at = Some(t);
                j.reason = "cancelled".into();
            }
            State::Running | State::Pausing => {
                // exit_code -1 marks "cancel requested": when the process is gone the job ends as Cancelled
                // (a Pausing job is switched back to Running so it is not requeued).
                j.reason = "cancelling".into();
                j.state = State::Running;
                j.exit_code = Some(-1);
                j.pausing_since = Some(t); // SIGKILL after the grace period, as for a pause
                if let Some(sid) = j.slurm_id {
                    stop_slurm_job(&self.cfg.slurm, &self.slurm_jobs, sid);
                } else if let Some(pg) = j.pid {
                    exec::signal_group(pg, Signal::SIGTERM);
                }
            }
            _ => return Err("job already finished".into()),
        }
        self.dirty = true;
        Ok(())
    }

    pub fn top(&mut self, id: JobId) -> Result<(), String> {
        let j = self.store.job_mut(id).ok_or("no such job")?;
        if j.state != State::Pending {
            return Err("only waiting jobs can be moved up".into());
        }
        j.top = true;
        self.dirty = true;
        Ok(())
    }

    pub fn progress(&mut self, id: JobId, r: ProgressReq) -> Result<(), String> {
        let j = self.store.job_mut(id).ok_or("no such job")?;
        j.progress = Progress {
            message: r.message,
            eta_s: r.eta_s,
            updated_at: now(),
        };
        if let Some(np) = r.no_pause {
            j.no_pause = np || !j.detached;
        }
        self.dirty = true;
        Ok(())
    }

    /// Foreground client reports its command finished.
    pub fn done(&mut self, id: JobId, code: i32) -> Result<(), String> {
        let t = now();
        let j = self.store.job_mut(id).ok_or("no such job")?;
        if j.detached {
            return Err("detached jobs are tracked by the daemon".into());
        }
        let cancelled = j.exit_code == Some(-1);
        j.exit_code = Some(code);
        j.ended_at = Some(t);
        j.state = if cancelled {
            State::Cancelled
        } else if code == 0 {
            State::Done
        } else {
            State::Failed
        };
        j.reason = format!("exited with {code}");
        self.dirty = true;
        Ok(())
    }

    fn set_reason(&mut self, id: JobId) {
        let held_by_slurm = self.store.job(id).and_then(|j| j.slurm_id).and_then(|sid| {
            self.slurm_jobs
                .iter()
                .find(|s| s.id == sid && s.state == "PENDING")
                .map(|s| s.reason.clone())
        });
        if let Some(j) = self.store.job_mut(id) {
            if let (State::Running, Some(sid)) = (j.state, j.slurm_id) {
                j.reason = match held_by_slurm {
                    Some(why) => format!("waiting in Slurm: {why}"),
                    None => format!("running as Slurm job {sid}"),
                };
                return;
            }
        }
        let total = self.capacity();
        let free = sched::free_shards(&self.store.jobs, total);
        let open = self.store.open_decisions().any(|d| d.job_id == id);
        let pos = sched::order(&self.store.jobs).iter().position(|j| j.id == id);
        if let Some(j) = self.store.job_mut(id) {
            if j.state == State::Pending {
                j.reason = if open {
                    "waiting for your answer (GPU busy)".into()
                } else if j.shards > free {
                    format!(
                        "waiting for {} shards ({free} free){}",
                        j.shards,
                        if j.requeues > 0 { ", paused earlier" } else { "" }
                    )
                } else if pos.unwrap_or(0) > 0 {
                    "waiting behind jobs ahead in the queue".into()
                } else {
                    "starting".into()
                };
            }
        }
    }

    /// Remove files nothing needs any more: exit records of finished jobs (their exit code is already in the
    /// state), and logs of jobs Doum has forgotten (finished more than 7 days ago).
    fn clean_files(&self) {
        let live = |id: u64| self.store.jobs.iter().find(|j| j.id == id);
        let id_of = |name: &str| name.split(['.', '-']).next().and_then(|s| s.parse::<u64>().ok());
        for (dir, keep) in [("exit", false), ("logs", true)] {
            let Ok(rd) = std::fs::read_dir(self.paths.state_dir.join(dir)) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Some(id) = id_of(&name) else { continue };
                let stale = match live(id) {
                    None => true,
                    Some(j) => !keep && j.state.is_terminal(),
                };
                if stale {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    /// One scheduling pass. Called every second.
    pub fn tick(&mut self) {
        let t = now();
        if self.cfg.backend == Backend::Slurm {
            match slurm::queue(&self.cfg.slurm, self.cfg.total_shards) {
                Some(q) => {
                    self.slurm_jobs = q;
                    self.slurm_seen = true;
                }
                None => self.slurm_seen = false,
            }
        }
        self.reap(t);
        self.expire_questions(t);
        self.checkbacks(t);
        self.stuck_leases(t);
        self.replan(t);
        self.start_ready(t);
        if t.saturating_sub(self.last_gpu_scan) >= 15 {
            self.last_gpu_scan = t;
            self.scan_unmanaged();
        }
        if t.saturating_sub(self.last_sample) >= metrics::EVERY_S {
            self.sample(t);
        }
        if t.saturating_sub(self.last_prune) >= 3600 {
            self.last_prune = t;
            self.store.prune(t, 7 * 86400);
            self.clean_files();
            self.dirty = true;
        }
        let ids: Vec<JobId> = self
            .store
            .jobs
            .iter()
            .filter(|j| j.state == State::Pending || (j.state == State::Running && j.slurm_id.is_some()))
            .map(|j| j.id)
            .collect();
        for id in ids {
            self.set_reason(id);
        }
        if self.notifier.version() != self.saved_alerts {
            self.saved_alerts = self.notifier.version();
            self.store.alerts = self.notifier.all();
            self.dirty = true;
        }
        if self.dirty {
            if let Err(e) = self.store.save(&self.paths.state_file()) {
                tracing::error!("saving state: {e}");
            }
            self.dirty = false;
        }
    }

    fn reap(&mut self, t: u64) {
        // A lease that stands for other processes (e.g. a ComfyUI server) ends when all of them have exited,
        // so a restarted or crashed app never leaves its shards reserved.
        let orphans: Vec<JobId> = self
            .store
            .jobs
            .iter()
            .filter(|j| !j.state.is_terminal() && !j.covers_pids.is_empty() && !j.covers_pids.iter().any(|p| sys::pid_alive(*p)))
            .map(|j| j.id)
            .collect();
        for id in orphans {
            let _ = self.cancel(id);
            if let Some(j) = self.store.job_mut(id) {
                j.reason = "the app it stood for has exited".into();
            }
        }
        let grace = self.cfg.pause_grace_s;
        let ids: Vec<JobId> = self
            .store
            .jobs
            .iter()
            .filter(|j| j.state.holds_shards() || (j.state == State::Pending && !j.detached))
            .map(|j| j.id)
            .collect();
        for id in ids {
            let Some(j) = self.store.job(id).cloned() else { continue };
            if !j.detached {
                // foreground: the client process must stay alive
                let alive = j.pid.is_some_and(sys::pid_alive);
                if !alive {
                    let jm = self.store.job_mut(id).unwrap();
                    jm.ended_at = Some(t);
                    if jm.state == State::Pending {
                        jm.state = State::Cancelled;
                        jm.reason = "the doum run that submitted it went away".into();
                    } else {
                        jm.state = if jm.exit_code == Some(-1) {
                            State::Cancelled
                        } else {
                            State::Failed
                        };
                        jm.reason = "the doum run process ended without reporting".into();
                    }
                    self.dirty = true;
                }
                continue;
            }
            // detached: reap our child, or check the pid after a daemon restart
            let finished = if let Some(sid) = j.slurm_id {
                // the command's process group, once `_exec` has started it (for display and GPU-process matching)
                if j.pid.is_none() {
                    if let (Some(pg), Some(jm)) = (exec::child_pgid(&self.paths, id, j.requeues), self.store.job_mut(id)) {
                        jm.pid = Some(pg);
                    }
                }
                let gone = self.slurm_seen
                    && t.saturating_sub(j.started_at.unwrap_or(t)) > 10
                    && !self.slurm_jobs.iter().any(|s| s.id == sid && s.is_active());
                exec::read_exit(&self.paths, id, j.requeues).is_some() || gone
            } else {
                match self.children.get_mut(&id) {
                    Some(c) => matches!(c.try_wait(), Ok(Some(_))),
                    None => !j.pid.is_some_and(sys::pid_alive),
                }
            };
            if !finished {
                let stopping = j.state == State::Pausing || j.exit_code == Some(-1);
                if stopping && j.pausing_since.is_some_and(|s| t.saturating_sub(s) > grace) {
                    if let Some(sid) = j.slurm_id {
                        slurm::signal(&self.cfg.slurm, sid, "KILL");
                    }
                    if let Some(pg) = j.pid {
                        exec::signal_group(pg, Signal::SIGKILL);
                    }
                    if let Some(pg) = exec::child_pgid(&self.paths, id, j.requeues) {
                        exec::signal_group(pg, Signal::SIGKILL);
                    }
                }
                continue;
            }
            self.children.remove(&id);
            let code = exec::read_exit(&self.paths, id, j.requeues);
            let jm = self.store.job_mut(id).unwrap();
            let cancelled = jm.exit_code == Some(-1);
            if jm.state == State::Pausing {
                jm.state = State::Pending;
                jm.requeues += 1;
                jm.pending_since = t;
                jm.pausing_since = None;
                if let Some(s) = jm.started_at {
                    jm.runs.push([s, t]);
                }
                jm.started_at = None;
                jm.pid = None;
                jm.slurm_id = None;
                jm.reason = "paused; resumes when the GPU is free".into();
            } else {
                jm.ended_at = Some(t);
                jm.exit_code = code;
                jm.state = if cancelled {
                    State::Cancelled
                } else if code == Some(0) {
                    State::Done
                } else {
                    State::Failed
                };
                jm.reason = match code {
                    Some(c) => format!("exited with {c}"),
                    None => "ended without an exit code (killed?)".into(),
                };
                let (name, st, dur) = (jm.name.clone(), jm.state, jm.started_at.map(|s| t.saturating_sub(s)).unwrap_or(0));
                match st {
                    State::Done => self
                        .notifier
                        .alert(Kind::Finished, &format!("{name} finished"), &format!("After {}.", fmt_dur(dur))),
                    State::Failed => self.notifier.alert(
                        Kind::Failed,
                        &format!("{name} failed"),
                        &format!(
                            "Exit code {}. Log: doum logs {id}",
                            code.map_or_else(|| "unknown".to_string(), |c| c.to_string())
                        ),
                    ),
                    _ => {}
                }
            }
            self.dirty = true;
        }
    }

    fn expire_questions(&mut self, t: u64) {
        let due: Vec<(u64, String)> = self
            .store
            .open_decisions()
            .filter(|d| t >= d.deadline)
            .map(|d| (d.id, d.recommended.clone()))
            .collect();
        for (id, rec) in due {
            let _ = self.answer_from(id, &rec, None, "timeout");
        }
        // questions about jobs that are no longer waiting are moot
        let moot: Vec<u64> = self
            .store
            .open_decisions()
            .filter(|d| self.store.job(d.job_id).is_none_or(|j| j.state != State::Pending))
            .map(|d| d.id)
            .collect();
        for id in moot {
            if let Some(d) = self.store.decisions.iter_mut().find(|d| d.id == id) {
                d.answer = Some("moot".into());
                self.dirty = true;
            }
        }
    }

    /// A job that stands in for an app (a lease, like ComfyUI's) reports progress every few seconds while the app is
    /// healthy. One that has reported before and then goes quiet for `stale_lease_s` is stuck: the app froze (it
    /// can stay alive for a long time like that). Its shards are released, and you are told.
    fn stuck_leases(&mut self, t: u64) {
        if self.cfg.stale_lease_s == 0 {
            return;
        }
        let stuck: Vec<(JobId, String, u32, u64)> = self
            .store
            .jobs
            .iter()
            .filter(|j| j.state == State::Running && !j.covers_pids.is_empty() && j.progress.updated_at > 0)
            .filter(|j| t.saturating_sub(j.progress.updated_at) > self.cfg.stale_lease_s)
            .map(|j| (j.id, j.name.clone(), j.shards, t - j.progress.updated_at))
            .collect();
        for (id, name, shards, quiet) in stuck {
            let _ = self.cancel(id);
            if let Some(j) = self.store.job_mut(id) {
                j.reason = format!("stopped responding (no check-in for {})", fmt_dur(quiet));
            }
            self.notifier.alert(
                Kind::Stuck,
                &format!("{name} stopped responding"),
                &format!(
                    "No check-in for {}, so its {shards} shards are released. If it is still running, it asks again when it needs the GPU.",
                    fmt_dur(quiet)
                ),
            );
        }
    }

    /// A smart decision is only as good as the estimates it used. Every 15 s, look again at the job at the front
    /// of the queue: if it still doesn't fit and its mode would now pause something (a job it waited for ran
    /// past its estimate, or reported a longer one), do that. Ask-mode jobs get the check-back question instead.
    fn replan(&mut self, t: u64) {
        if t.saturating_sub(self.last_replan) < 15 {
            return;
        }
        self.last_replan = t;
        if self.store.jobs.iter().any(|j| j.state == State::Pausing) {
            return; // already making room
        }
        let Some(head) = sched::order(&self.store.jobs).first().map(|j| (j.id, j.shards, j.lane, j.mode)) else {
            return;
        };
        let (id, need, lane, mode) = head;
        if !matches!(mode, Mode::Smart | Mode::Pause) || self.store.open_decisions().any(|d| d.job_id == id) {
            return;
        }
        let p = sched::plan(
            &self.store.jobs,
            self.capacity(),
            need,
            lane,
            mode,
            t,
            self.cfg.smart_wait_s,
            self.cfg.pause_grace_s,
        );
        if need > p.free_now && !p.pause.is_empty() {
            tracing::info!("re-plan for job {id}: pausing {:?}", p.pause);
            self.apply_plan_why(id, &p, "ran past its estimate; ");
        }
    }

    fn checkbacks(&mut self, t: u64) {
        let due: Vec<JobId> = self
            .store
            .jobs
            .iter()
            // only for work someone is waiting for: batch jobs waiting in line is normal and not worth a question
            .filter(|j| j.lane == Lane::Interactive || !j.detached)
            .filter(|j| j.state == State::Pending && j.max_wait_s > 0 && t.saturating_sub(j.pending_since) >= j.max_wait_s)
            .filter(|j| !self.store.open_decisions().any(|d| d.job_id == j.id))
            .map(|j| j.id)
            .collect();
        for id in due {
            self.open_checkback(id);
            if let Some(j) = self.store.job_mut(id) {
                j.pending_since = t; // re-arm: ask again after another max_wait if still waiting
            }
        }
    }

    fn start_ready(&mut self, t: u64) {
        let mut held: Vec<JobId> = self.store.open_decisions().map(|d| d.job_id).collect();
        let waiting: Vec<JobId> = self.store.jobs.iter().filter(|j| j.state == State::Pending).map(|j| j.id).collect();
        held.extend(
            self.store
                .jobs
                .iter()
                .filter(|j| j.paused_for.is_some_and(|b| waiting.contains(&b)))
                .map(|j| j.id),
        );
        for id in sched::to_start(&self.store.jobs, self.capacity(), &held) {
            // jobs paused to make room for this one are not waiting for anything any more; a stale mark could
            // later hold them behind a job that is in turn held for them (a deadlock the simulator found)
            for other in self.store.jobs.iter_mut().filter(|o| o.paused_for == Some(id)) {
                other.paused_for = None;
            }
            let Some(j) = self.store.job(id).cloned() else { continue };
            if j.detached && self.cfg.backend == Backend::Slurm {
                let mem = self.mem_for(&j);
                match slurm::submit(&self.cfg.slurm, &self.paths, &j, mem, &self.api, &self.token) {
                    Ok(sid) => {
                        let log = self.paths.log_file(id, &j.name).display().to_string();
                        let jm = self.store.job_mut(id).unwrap();
                        jm.state = State::Running;
                        jm.started_at = Some(t);
                        jm.paused_for = None;
                        jm.top = false;
                        jm.pid = None;
                        jm.slurm_id = Some(sid);
                        jm.log = Some(log);
                        jm.reason = format!("running as Slurm job {sid}");
                    }
                    Err(e) => {
                        let jm = self.store.job_mut(id).unwrap();
                        jm.state = State::Failed;
                        jm.ended_at = Some(t);
                        jm.reason = format!("could not start: {e}");
                        let name = jm.name.clone();
                        self.notifier
                            .alert(Kind::Failed, &format!("{name} could not start"), &e.to_string());
                    }
                }
            } else if j.detached {
                let mem = self.mem_for(&j);
                match exec::spawn_detached(&self.paths, &j, mem, &self.api, &self.token) {
                    Ok(child) => {
                        let pid = child.id() as i32;
                        self.children.insert(id, child);
                        let log = self.paths.log_file(id, &j.name).display().to_string();
                        let jm = self.store.job_mut(id).unwrap();
                        jm.state = State::Running;
                        jm.started_at = Some(t);
                        jm.paused_for = None;
                        jm.top = false;
                        jm.pid = Some(pid);
                        jm.log = Some(log);
                        jm.reason = "running".into();
                    }
                    Err(e) => {
                        let jm = self.store.job_mut(id).unwrap();
                        jm.state = State::Failed;
                        jm.ended_at = Some(t);
                        jm.reason = format!("could not start: {e}");
                        let name = jm.name.clone();
                        self.notifier
                            .alert(Kind::Failed, &format!("{name} could not start"), &e.to_string());
                    }
                }
            } else {
                let jm = self.store.job_mut(id).unwrap();
                jm.state = State::Running;
                jm.started_at = Some(t);
                jm.top = false;
                jm.reason = "running (in the doum run that submitted it)".into();
            }
            if let Some(jm) = self.store.job(id) {
                if jm.requeues > 0 && jm.state == State::Running {
                    self.notifier
                        .alert(Kind::Resumed, &format!("Resumed {}", jm.name), "The GPU is free again.");
                }
            }
            self.dirty = true;
        }
    }

    /// Record a history sample and attribute GPU energy to the running jobs.
    fn sample(&mut self, t: u64) {
        let dt = if self.last_sample == 0 {
            0.0
        } else {
            (t - self.last_sample) as f64
        };
        self.last_sample = t;
        let (gpu_c, gpu_w) = metrics::gpu_temp_power();
        let running: Vec<usize> = (0..self.store.jobs.len())
            .filter(|i| self.store.jobs[*i].state.holds_shards())
            .collect();
        if let (Some(w), true) = (gpu_w, dt > 0.0 && dt < 120.0) {
            let shards: Vec<u32> = running.iter().map(|i| self.store.jobs[*i].shards).collect();
            for (k, e) in metrics::split_energy(&shards, w, dt).into_iter().enumerate() {
                self.store.jobs[running[k]].energy_j += e;
            }
            if !running.is_empty() {
                self.dirty = true;
            }
        }
        self.history.push(Sample {
            t,
            used_shards: sched::used_shards(&self.store.jobs) + self.external_shards(),
            mem_free_gb: (sys::mem_available_gb() * 10.0).round() / 10.0,
            gpu_c,
            gpu_w,
            soc_c: metrics::soc_temp().map(|c| (c * 10.0).round() / 10.0),
            running: running.len() as u32,
            waiting: self.store.jobs.iter().filter(|j| j.state == State::Pending).count() as u32,
        });
    }

    /// GPU processes that are not part of any doum job (they bypass the queue). Alert once per process.
    fn scan_unmanaged(&mut self) {
        let procs = self.unmanaged();
        self.unmanaged_cache = procs.clone();
        for p in &procs {
            if self.known_unmanaged.insert(p.pid) {
                self.notifier.alert(
                    Kind::Outside,
                    "GPU used outside the queue",
                    &format!(
                        "{} (pid {}, ~{:.0} GB) is using the GPU without Doum. Run it with `doum run` so it waits its turn.",
                        p.name, p.pid, p.mem_gb
                    ),
                );
            }
        }
        self.known_unmanaged.retain(|pid| procs.iter().any(|p| p.pid == *pid));
    }

    fn unmanaged(&self) -> Vec<sys::GpuProc> {
        let running: Vec<&Job> = self.store.jobs.iter().filter(|j| j.state.holds_shards()).collect();
        sys::gpu_procs()
            .into_iter()
            .filter(|p| {
                !running.iter().any(|j| {
                    let Some(root) = j.pid else { return false };
                    let pg = sys::pgid(p.pid);
                    pg == Some(root)
                        || exec::child_pgid(&self.paths, j.id, j.requeues).is_some_and(|c| pg == Some(c))
                        || sys::descends_from(p.pid, root)
                        || j.covers_pids.iter().any(|c| sys::descends_from(p.pid, *c))
                })
            })
            .filter(|p| p.mem_gb >= self.cfg.unmanaged_min_gb)
            .collect()
    }

    pub fn status(&self) -> StatusView {
        StatusView {
            total_shards: self.cfg.total_shards,
            used_shards: sched::used_shards(&self.store.jobs) + self.external_shards(),
            mem_available_gb: (sys::mem_available_gb() * 10.0).round() / 10.0,
            jobs: self.store.jobs.iter().map(public).collect(),
            queue: sched::order(&self.store.jobs).iter().map(|j| j.id).collect(),
            decisions: self
                .store
                .open_decisions()
                .map(|d| Decision {
                    code: String::new(),
                    ..d.clone()
                })
                .collect(),
            alerts: self.notifier.recent(30),
            unmanaged: self.unmanaged_cache.clone(),
            latest: self.history.last().cloned(),
            backend: self.cfg.backend,
            external: self.slurm_jobs.iter().filter(|j| !j.is_doum() && j.is_active()).cloned().collect(),
            now: now(),
        }
    }
}

/// Ask a Slurm job to stop: SIGTERM if it is running (so it can save a checkpoint), cancel if Slurm has not
/// started it yet (a pending job can't be signalled).
fn stop_slurm_job(cfg: &crate::config::SlurmCfg, queue: &[slurm::SlurmJob], sid: u64) {
    match queue.iter().find(|s| s.id == sid) {
        Some(s) if s.state == "PENDING" => slurm::cancel(cfg, sid),
        _ => slurm::signal(cfg, sid, "TERM"),
    }
}

/// A random one-time code that a phone answer must carry.
fn new_code() -> String {
    use std::io::Read;
    let mut b = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A job as the API shows it: without its environment, which can hold the API keys of the shell that ran
/// `doum run`. The daemon keeps the environment to restart paused jobs; it is never sent back out.
fn public(j: &Job) -> Job {
    let mut j = j.clone();
    j.env.clear();
    j
}

fn view(mode: Mode, p: &Plan) -> PlanView {
    PlanView {
        mode,
        pause: p.pause.clone(),
        wait_for: p.wait_for.clone(),
        eta_s: p.eta_s,
        free_now: p.free_now,
    }
}

pub fn fmt_dur(s: u64) -> String {
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{} min", (s + 30) / 60),
        _ => format!("{}h{:02}", s / 3600, (s % 3600) / 60),
    }
}

fn load_or_create_token(paths: &Paths) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let f = paths.token_file();
    if let Ok(t) = std::fs::read_to_string(&f) {
        if t.trim().len() >= 32 {
            return Ok(t.trim().to_string());
        }
    }
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom").and_then(|mut u| std::io::Read::read_exact(&mut u, &mut b))?;
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::write(&f, &t)?;
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600))?;
    Ok(t)
}

// ---------- HTTP ----------

struct ApiErr(StatusCode, String);
impl IntoResponse for ApiErr {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}
type ApiResult<T> = std::result::Result<Json<T>, ApiErr>;

/// Mutating calls need the token: as the `x-doum-token` header (CLI, MCP, jobs) or as the panel's cookie
/// (set by /login, HttpOnly + SameSite=Strict, and the request's Origin must be this server).
fn authed(d: &Daemon, h: &HeaderMap) -> Result<(), ApiErr> {
    if h.get("x-doum-token").and_then(|v| v.to_str().ok()) == Some(d.token.as_str()) {
        return Ok(());
    }
    let cookie_ok = h
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .any(|c| c.trim() == format!("doum={}", d.token));
    let origin_ok = match h.get("origin").and_then(|v| v.to_str().ok()) {
        None => true,
        Some(o) => o == d.api || o == d.api.replace("127.0.0.1", "localhost"),
    };
    if cookie_ok && origin_ok {
        return Ok(());
    }
    Err(ApiErr(
        StatusCode::UNAUTHORIZED,
        "not authorised (open the panel with `doum panel`)".into(),
    ))
}

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, Daemon> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

macro_rules! act {
    ($s:expr, $h:expr, $f:expr) => {{
        let mut d = lock(&$s);
        authed(&d, &$h)?;
        $f(&mut d).map_err(|e: String| ApiErr(StatusCode::BAD_REQUEST, e))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }};
}

async fn h_status(AxState(s): AxState<Shared>) -> Json<StatusView> {
    Json(lock(&s).status())
}
async fn h_job(AxState(s): AxState<Shared>, Path(id): Path<JobId>) -> ApiResult<Job> {
    lock(&s)
        .store
        .job(id)
        .map(public)
        .map(Json)
        .ok_or(ApiErr(StatusCode::NOT_FOUND, "no such job".into()))
}
async fn h_submit(AxState(s): AxState<Shared>, h: HeaderMap, Json(mut r): Json<SubmitReq>) -> ApiResult<SubmitResp> {
    let mut d = lock(&s);
    authed(&d, &h)?;
    if r.cmd.is_empty() {
        return Err(ApiErr(StatusCode::BAD_REQUEST, "empty command".into()));
    }
    if r.owner == crate::mcp::OWNER {
        d.agent_limits(&mut r).map_err(|e| ApiErr(StatusCode::FORBIDDEN, e))?;
    }
    Ok(Json(d.submit(r)))
}
async fn h_pause(AxState(s): AxState<Shared>, h: HeaderMap, Path(id): Path<JobId>) -> ApiResult<serde_json::Value> {
    act!(s, h, |d: &mut Daemon| d.pause(id, "paused by you"))
}
async fn h_cancel(AxState(s): AxState<Shared>, h: HeaderMap, Path(id): Path<JobId>) -> ApiResult<serde_json::Value> {
    act!(s, h, |d: &mut Daemon| d.cancel(id))
}
async fn h_top(AxState(s): AxState<Shared>, h: HeaderMap, Path(id): Path<JobId>) -> ApiResult<serde_json::Value> {
    act!(s, h, |d: &mut Daemon| d.top(id))
}
async fn h_progress(
    AxState(s): AxState<Shared>,
    h: HeaderMap,
    Path(id): Path<JobId>,
    Json(r): Json<ProgressReq>,
) -> ApiResult<serde_json::Value> {
    act!(s, h, |d: &mut Daemon| d.progress(id, r))
}
async fn h_done(AxState(s): AxState<Shared>, h: HeaderMap, Path(id): Path<JobId>, Json(r): Json<DoneReq>) -> ApiResult<serde_json::Value> {
    act!(s, h, |d: &mut Daemon| d.done(id, r.code))
}
async fn h_answer(
    AxState(s): AxState<Shared>,
    h: HeaderMap,
    Path(id): Path<u64>,
    Json(r): Json<AnswerReq>,
) -> ApiResult<serde_json::Value> {
    act!(s, h, |d: &mut Daemon| d.answer_from(
        id,
        &r.choice,
        r.code.as_deref(),
        r.from.as_deref().unwrap_or("")
    ))
}
async fn h_index() -> Html<&'static str> {
    Html(include_str!("panel.html"))
}

#[derive(Deserialize)]
struct LoginQ {
    token: String,
}
/// `doum panel` opens /login?token=… once; the token becomes an HttpOnly, SameSite=Strict cookie.
async fn h_login(AxState(s): AxState<Shared>, axum::extract::Query(q): axum::extract::Query<LoginQ>) -> Response {
    let ok = q.token == lock(&s).token;
    if !ok {
        return (StatusCode::UNAUTHORIZED, "wrong token").into_response();
    }
    (
        StatusCode::SEE_OTHER,
        [
            ("location", "/".to_string()),
            (
                "set-cookie",
                format!("doum={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000", q.token),
            ),
        ],
    )
        .into_response()
}

#[derive(Deserialize)]
struct MetricsQ {
    #[serde(default)]
    minutes: Option<u64>,
}
async fn h_metrics(AxState(s): AxState<Shared>, axum::extract::Query(q): axum::extract::Query<MetricsQ>) -> Json<Vec<Sample>> {
    let since = now().saturating_sub(q.minutes.unwrap_or(60) * 60);
    Json(lock(&s).history.since(since))
}

async fn h_prometheus(AxState(s): AxState<Shared>) -> Response {
    let body = crate::prom::render(&lock(&s).status());
    ([("content-type", "text/plain; version=0.0.4; charset=utf-8")], body).into_response()
}

fn allowed_hosts(addr: std::net::SocketAddr) -> Vec<String> {
    let mut h: Vec<String> = ["127.0.0.1", "localhost", "[::1]"]
        .iter()
        .map(|h| format!("{h}:{}", addr.port()))
        .collect();
    h.push(addr.to_string()); // the configured address itself, e.g. 127.0.0.2:8420
    h
}

/// Only answer requests addressed to this machine by name. A web page can point its own domain at 127.0.0.1
/// (DNS rebinding); its requests then carry that domain in Host and are refused here.
async fn host_guard(AxState(hosts): AxState<Arc<Vec<String>>>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let host = req.headers().get("host").and_then(|v| v.to_str().ok()).unwrap_or("");
    if hosts.iter().any(|h| h == host) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "doum: requests must be addressed to 127.0.0.1 or localhost").into_response()
    }
}

pub fn router(s: Shared) -> Router {
    Router::new()
        .route("/", get(h_index))
        .route("/login", get(h_login))
        .route("/api/status", get(h_status))
        .route("/api/metrics", get(h_metrics))
        .route("/metrics", get(h_prometheus))
        .route("/api/jobs", post(h_submit))
        .route("/api/jobs/{id}", get(h_job))
        .route("/api/jobs/{id}/pause", post(h_pause))
        .route("/api/jobs/{id}/cancel", post(h_cancel))
        .route("/api/jobs/{id}/top", post(h_top))
        .route("/api/jobs/{id}/progress", post(h_progress))
        .route("/api/jobs/{id}/done", post(h_done))
        .route("/api/decisions/{id}", post(h_answer))
        .with_state(s)
}

pub async fn serve(cfg: Config, paths: Paths) -> Result<()> {
    let listen = cfg.listen.clone();
    let d = Daemon::new(cfg, paths)?;
    notify::listen_for_answers(&d.cfg.notify, d.api.clone(), d.token.clone());
    tracing::info!("doum daemon on http://{listen} ({} jobs in state)", d.store.jobs.len());
    let shared: Shared = Arc::new(Mutex::new(d));
    let ticker = shared.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(1));
        loop {
            iv.tick().await;
            let s = ticker.clone();
            let _ = tokio::task::spawn_blocking(move || lock(&s).tick()).await;
        }
    });
    let addr: std::net::SocketAddr = listen
        .parse()
        .map_err(|_| anyhow::anyhow!("listen = {listen:?} must be an address like 127.0.0.1:8420"))?;
    if !addr.ip().is_loopback() {
        anyhow::bail!("listen = {listen:?}: doum only listens on this machine (127.0.0.1 or ::1)");
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let hosts = Arc::new(allowed_hosts(addr));
    let app = router(shared).layer(axum::middleware::from_fn_with_state(hosts, host_guard));
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
