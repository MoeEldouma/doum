//! Scheduling rules, as pure functions (no I/O) so every rule is unit-tested.
//!
//! * Order: interactive lane before batch, then jobs moved to the top, then first come first served
//!   (a paused job keeps its original place, so it resumes before newer work).
//! * Strict: if the job at the front does not fit, nothing behind it starts (no one sneaks ahead of a waiting job).
//! * Smart: when a job does not fit, pause only the running jobs that will not finish soon.

use crate::job::{Job, JobId, Mode, State};

/// Pending jobs in the order they will be started.
pub fn order(jobs: &[Job]) -> Vec<&Job> {
    let mut p: Vec<&Job> = jobs.iter().filter(|j| j.state == State::Pending).collect();
    p.sort_by(|a, b| {
        b.lane
            .cmp(&a.lane)
            .then(b.top.cmp(&a.top))
            .then(a.submitted_at.cmp(&b.submitted_at))
            .then(a.id.cmp(&b.id))
    });
    p
}

pub fn used_shards(jobs: &[Job]) -> u32 {
    jobs.iter().filter(|j| j.state.holds_shards()).map(|j| j.shards).sum()
}

pub fn free_shards(jobs: &[Job], total: u32) -> u32 {
    total.saturating_sub(used_shards(jobs))
}

/// Jobs to start now, in order. `held` are pending jobs that must not start yet (e.g. waiting for the user's
/// answer); they still block everything behind them.
pub fn to_start(jobs: &[Job], total: u32, held: &[JobId]) -> Vec<JobId> {
    let mut free = free_shards(jobs, total);
    let mut out = vec![];
    for j in order(jobs) {
        if held.contains(&j.id) || j.shards > free {
            break;
        }
        free -= j.shards;
        out.push(j.id);
    }
    out
}

/// What it takes to make room for `need` shards.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Plan {
    /// Running jobs to pause now (stop + requeue).
    pub pause: Vec<JobId>,
    /// Running jobs we let finish.
    pub wait_for: Vec<JobId>,
    /// Estimated seconds until the job can start, if known.
    pub eta_s: Option<u64>,
    /// Shards free right now.
    pub free_now: u32,
}

/// Plan for a job needing `need` shards under `mode`. `smart_s`: jobs finishing within this many seconds are
/// waited for, longer (or unknown) ones are paused. `grace_s`: time a paused job gets to save and stop.
pub fn plan(jobs: &[Job], total: u32, need: u32, mode: Mode, now: u64, smart_s: u64, grace_s: u64) -> Plan {
    let free_now = free_shards(jobs, total);
    let mut p = Plan {
        free_now,
        ..Default::default()
    };
    if need <= free_now {
        p.eta_s = Some(0);
        return p;
    }
    let running: Vec<&Job> = jobs.iter().filter(|j| j.state.holds_shards()).collect();
    let eta = |j: &Job| j.eta_left(now);

    // Candidates to pause, longest (or unknown) first.
    let mut cand: Vec<&Job> = running
        .iter()
        .copied()
        .filter(|j| j.pausable())
        .filter(|j| match mode {
            Mode::Pause => true,
            Mode::Smart | Mode::Ask => eta(j).is_none_or(|e| e > smart_s),
            Mode::Wait => false,
        })
        .collect();
    cand.sort_by(|a, b| {
        eta(b)
            .unwrap_or(u64::MAX)
            .cmp(&eta(a).unwrap_or(u64::MAX))
            .then(b.shards.cmp(&a.shards))
    });

    let mut free = free_now;
    for j in cand {
        if free >= need {
            break;
        }
        free += j.shards;
        p.pause.push(j.id);
    }
    if free >= need {
        p.eta_s = Some(if p.pause.is_empty() { 0 } else { grace_s });
        p.wait_for = vec![];
        return p;
    }

    // Still short: wait for the remaining running jobs, soonest first.
    let mut rest: Vec<&Job> = running.into_iter().filter(|j| !p.pause.contains(&j.id)).collect();
    rest.sort_by_key(|j| eta(j).unwrap_or(u64::MAX));
    let mut t: Option<u64> = Some(0);
    for j in rest {
        if free >= need {
            break;
        }
        free += j.shards;
        p.wait_for.push(j.id);
        t = match (t, eta(j)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            _ => None,
        };
    }
    p.eta_s = if free >= need { t } else { None };
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Lane, Progress};

    fn job(id: JobId, lane: Lane, shards: u32, state: State, submitted: u64) -> Job {
        Job {
            id,
            name: format!("j{id}"),
            cmd: vec!["true".into()],
            cwd: "/".into(),
            env: Default::default(),
            shards,
            mem_gb: None,
            lane,
            mode: Mode::Smart,
            detached: true,
            no_pause: false,
            owner: "t".into(),
            state,
            submitted_at: submitted,
            started_at: None,
            ended_at: None,
            top: false,
            requeues: 0,
            pending_since: submitted,
            pausing_since: None,
            paused_for: None,
            max_wait_s: 0,
            progress: Progress::default(),
            exit_code: None,
            energy_j: 0.0,
            pid: None,
            log: None,
            reason: String::new(),
        }
    }
    fn with_eta(mut j: Job, eta: u64, at: u64) -> Job {
        j.progress = Progress {
            message: String::new(),
            eta_s: Some(eta),
            updated_at: at,
        };
        j
    }

    #[test]
    fn interactive_before_batch_then_fifo() {
        let jobs = vec![
            job(1, Lane::Batch, 10, State::Pending, 1),
            job(2, Lane::Interactive, 10, State::Pending, 5),
            job(3, Lane::Batch, 10, State::Pending, 2),
        ];
        let ids: Vec<_> = order(&jobs).iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![2, 1, 3]);
    }

    #[test]
    fn top_moves_ahead_within_lane_only() {
        let mut jobs = vec![
            job(1, Lane::Batch, 10, State::Pending, 1),
            job(2, Lane::Batch, 10, State::Pending, 2),
            job(3, Lane::Interactive, 10, State::Pending, 3),
        ];
        jobs[1].top = true;
        let ids: Vec<_> = order(&jobs).iter().map(|j| j.id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    #[test]
    fn paused_job_keeps_its_place() {
        // job 1 was submitted first, paused and requeued: it goes back ahead of job 2
        let mut a = job(1, Lane::Batch, 60, State::Pending, 1);
        a.requeues = 1;
        a.pending_since = 100;
        let jobs = vec![a, job(2, Lane::Batch, 60, State::Pending, 50)];
        assert_eq!(order(&jobs)[0].id, 1);
    }

    #[test]
    fn starts_what_fits_strictly_in_order() {
        let jobs = vec![
            job(1, Lane::Batch, 60, State::Running, 0),
            job(2, Lane::Batch, 30, State::Pending, 1),
            job(3, Lane::Batch, 40, State::Pending, 2), // does not fit after 2 → blocks 4
            job(4, Lane::Batch, 5, State::Pending, 3),
        ];
        assert_eq!(to_start(&jobs, 110, &[]), vec![2]);
    }

    #[test]
    fn held_job_blocks_those_behind_it() {
        let jobs = vec![
            job(1, Lane::Interactive, 10, State::Pending, 0),
            job(2, Lane::Batch, 10, State::Pending, 1),
        ];
        assert!(to_start(&jobs, 110, &[1]).is_empty());
    }

    #[test]
    fn fits_now_needs_no_plan() {
        let jobs = vec![job(1, Lane::Batch, 60, State::Running, 0)];
        let p = plan(&jobs, 110, 50, Mode::Smart, 10, 600, 90);
        assert!(p.pause.is_empty() && p.wait_for.is_empty());
        assert_eq!(p.eta_s, Some(0));
    }

    #[test]
    fn smart_waits_for_short_and_pauses_long() {
        // the scenario from the design: ComfyUI clip (5 min left) + training (2 h left), writer needs everything
        let now = 1000;
        let comfy = with_eta(job(1, Lane::Batch, 60, State::Running, 0), 300, now);
        let train = with_eta(job(2, Lane::Batch, 24, State::Running, 0), 7200, now);
        let jobs = vec![comfy, train];
        let p = plan(&jobs, 110, 110, Mode::Smart, now, 600, 90);
        assert_eq!(p.pause, vec![2]);
        assert_eq!(p.wait_for, vec![1]);
        assert_eq!(p.eta_s, Some(300));
    }

    #[test]
    fn pause_mode_pauses_both() {
        let now = 1000;
        let jobs = vec![
            with_eta(job(1, Lane::Batch, 60, State::Running, 0), 300, now),
            with_eta(job(2, Lane::Batch, 24, State::Running, 0), 7200, now),
        ];
        let p = plan(&jobs, 110, 110, Mode::Pause, now, 600, 90);
        assert_eq!(p.pause.len(), 2);
        assert!(p.wait_for.is_empty());
        assert_eq!(p.eta_s, Some(90));
    }

    #[test]
    fn wait_mode_pauses_nothing() {
        let now = 1000;
        let jobs = vec![with_eta(job(1, Lane::Batch, 60, State::Running, 0), 7200, now)];
        let p = plan(&jobs, 110, 110, Mode::Wait, now, 600, 90);
        assert!(p.pause.is_empty());
        assert_eq!(p.wait_for, vec![1]);
        assert_eq!(p.eta_s, Some(7200));
    }

    #[test]
    fn unknown_eta_counts_as_long_in_smart() {
        let jobs = vec![job(1, Lane::Batch, 60, State::Running, 0)];
        let p = plan(&jobs, 110, 110, Mode::Smart, 10, 600, 90);
        assert_eq!(p.pause, vec![1]);
    }

    #[test]
    fn never_pauses_foreground_or_no_pause_jobs() {
        let mut fg = job(1, Lane::Interactive, 60, State::Running, 0);
        fg.detached = false;
        let mut np = job(2, Lane::Batch, 40, State::Running, 0);
        np.no_pause = true;
        let p = plan(&[fg, np], 110, 110, Mode::Pause, 10, 600, 90);
        assert!(p.pause.is_empty());
        assert_eq!(p.wait_for.len(), 2);
        assert_eq!(p.eta_s, None); // nobody reported an ETA
    }

    #[test]
    fn eta_ages_since_report() {
        let j = with_eta(job(1, Lane::Batch, 1, State::Running, 0), 300, 1000);
        assert_eq!(j.eta_left(1100), Some(200));
        assert_eq!(j.eta_left(2000), Some(0));
    }
}
