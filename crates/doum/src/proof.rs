//! Evidence that the scheduling rules hold, beyond the hand-written examples in `sched.rs`:
//!
//! * property tests: thousands of random job mixes per run, each checked against the guarantees;
//! * a deterministic simulator that drives the same pure functions the daemon uses (`order`, `to_start`,
//!   `plan`, `eta_left`) through random workloads (arrivals, finishes, late or missing estimates, pauses,
//!   resumes) and checks the guarantees after every step.
//!
//! `cargo test` runs a short simulation. The full one: `DOUM_SIM_EVENTS=1000000 cargo test --release sim_ -- --nocapture`.

use proptest::prelude::*;

use crate::job::{Job, JobId, Lane, Mode, Progress, State};
use crate::sched::{self, tests::job};

// ---------- random job mixes ----------

#[derive(Debug, Clone)]
struct Mix {
    total: u32,
    jobs: Vec<Job>,
    now: u64,
}

fn arb_mix() -> impl Strategy<Value = Mix> {
    let one = (
        any::<bool>(),                              // interactive
        1u32..=110,                                 // shards
        0u8..3,                                     // 0 pending, 1 running, 2 pausing
        any::<bool>(),                              // top
        0u64..500,                                  // submitted_at
        prop::option::of((0u64..9000, 0u64..2000)), // (eta_s, updated_at)
        any::<bool>(),                              // detached
        any::<bool>(),                              // no_pause
    );
    (1u32..=110, prop::collection::vec(one, 0..14), 0u64..4000).prop_map(|(total, specs, now)| {
        let mut used = 0;
        let mut jobs = vec![];
        for (i, (inter, shards, st, top, sub, eta, detached, no_pause)) in specs.into_iter().enumerate() {
            let shards = shards.min(total);
            let mut state = match st {
                1 => State::Running,
                2 => State::Pausing,
                _ => State::Pending,
            };
            // a real daemon never holds more than the total: running jobs beyond it would not have started
            if state.holds_shards() {
                if used + shards > total {
                    state = State::Pending;
                } else {
                    used += shards;
                }
            }
            let mut j = job(
                i as JobId + 1,
                if inter { Lane::Interactive } else { Lane::Batch },
                shards,
                state,
                sub,
            );
            j.top = top;
            j.detached = detached;
            j.no_pause = no_pause || !detached;
            if let Some((e, at)) = eta {
                j.progress = Progress {
                    message: String::new(),
                    eta_s: Some(e),
                    updated_at: at,
                };
            }
            jobs.push(j);
        }
        Mix { total, jobs, now }
    })
}

fn arb_mode() -> impl Strategy<Value = Mode> {
    prop_oneof![Just(Mode::Smart), Just(Mode::Wait), Just(Mode::Pause), Just(Mode::Ask)]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// Interactive before batch; then jobs moved to the top; then first come, first served.
    #[test]
    fn order_is_lane_then_top_then_fifo(m in arb_mix()) {
        let o = sched::order(&m.jobs);
        prop_assert!(o.iter().all(|j| j.state == State::Pending));
        for w in o.windows(2) {
            let (a, b) = (w[0], w[1]);
            prop_assert!(a.lane >= b.lane);
            if a.lane == b.lane {
                prop_assert!(a.top >= b.top);
                if a.top == b.top {
                    prop_assert!((a.submitted_at, a.id) <= (b.submitted_at, b.id));
                }
            }
        }
    }

    /// Starting jobs never hands out more shards than exist, and it is strict: what starts is a prefix of the
    /// queue, and the first job left behind is either held or genuinely does not fit.
    #[test]
    fn to_start_never_overcommits_and_is_strict(m in arb_mix(), held_pick in prop::collection::vec(any::<bool>(), 14)) {
        let pending: Vec<JobId> = sched::order(&m.jobs).iter().map(|j| j.id).collect();
        let held: Vec<JobId> = pending.iter().zip(held_pick.iter()).filter(|(_, h)| **h).map(|(id, _)| *id).collect();
        let start = sched::to_start(&m.jobs, m.total, &held);
        let shards = |id: JobId| m.jobs.iter().find(|j| j.id == id).unwrap().shards;
        let used = sched::used_shards(&m.jobs);
        prop_assert!(used + start.iter().map(|id| shards(*id)).sum::<u32>() <= m.total);
        prop_assert_eq!(&start[..], &pending[..start.len()]);
        if let Some(next) = pending.get(start.len()) {
            let free = m.total - used - start.iter().map(|id| shards(*id)).sum::<u32>();
            prop_assert!(held.contains(next) || shards(*next) > free);
        }
    }

    /// The plan to make room: wait never pauses; only pausable running jobs are paused; smart never pauses a job
    /// it expects to finish within `smart_s`; a complete plan really frees enough; and no pause is wasted.
    #[test]
    fn plans_are_safe_sufficient_and_minimal(m in arb_mix(), need in 1u32..=110, inter in any::<bool>(), mode in arb_mode(), smart_s in 0u64..1200) {
        let need = need.min(m.total);
        let lane = if inter { Lane::Interactive } else { Lane::Batch };
        let p = sched::plan(&m.jobs, m.total, need, lane, mode, m.now, smart_s, 90);
        let get = |id: JobId| m.jobs.iter().find(|j| j.id == id).unwrap();
        prop_assert_eq!(p.free_now, sched::free_shards(&m.jobs, m.total));
        if need <= p.free_now {
            prop_assert!(p.pause.is_empty() && p.wait_for.is_empty());
            prop_assert_eq!(p.eta_s, Some(0));
        }
        if mode == Mode::Wait {
            prop_assert!(p.pause.is_empty());
        }
        for id in &p.pause {
            let j = get(*id);
            prop_assert!(j.pausable(), "paused a job that may not be paused");
            prop_assert!(j.lane <= lane, "batch work paused an interactive job");
            if mode != Mode::Pause {
                prop_assert!(j.lane < lane, "smart mode paused a job in the same lane");
            }
            if matches!(mode, Mode::Smart | Mode::Ask) {
                prop_assert!(j.eta_left(m.now).is_none_or(|e| e > smart_s), "smart paused a job about to finish");
            }
        }
        prop_assert!(p.pause.iter().all(|id| !p.wait_for.contains(id)));
        let freed: u32 = p.pause.iter().chain(p.wait_for.iter()).map(|id| get(*id).shards).sum();
        if p.eta_s.is_some() {
            prop_assert!(p.free_now + freed >= need, "plan claims a start time but does not free enough");
        }
        if let Some((last, rest)) = p.pause.split_last() {
            let without_last: u32 = rest.iter().map(|id| get(*id).shards).sum();
            prop_assert!(p.free_now + without_last < need, "pause of job {} was not needed", last);
        }
    }

    /// An estimate only counts down, and once it is stale (well past due) it stays stale.
    #[test]
    fn estimates_count_down_then_expire(eta in 0u64..20_000, at in 0u64..1000, steps in prop::collection::vec(0u64..3000, 1..12)) {
        let mut j = job(1, Lane::Batch, 1, State::Running, 0);
        j.progress = Progress { message: String::new(), eta_s: Some(eta), updated_at: at };
        let mut t = at;
        let mut last = j.eta_left(t);
        prop_assert_eq!(last, Some(eta));
        for s in steps {
            t += s;
            let now = j.eta_left(t);
            match (last, now) {
                (Some(a), Some(b)) => prop_assert!(b <= a),
                (None, Some(_)) => prop_assert!(false, "a stale estimate came back"),
                _ => {}
            }
            last = now;
        }
    }
}

// ---------- the simulator ----------

/// xorshift64*: deterministic, so a failing seed can be replayed exactly.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

/// What the simulator knows about a job that the scheduler does not: how long it really runs.
struct Truth {
    work_left: u64,
    /// The job reports an estimate this many times its real remaining time (1.0 = honest).
    lie: f64,
    reports: bool,
}

#[derive(Default, Debug)]
pub struct SimStats {
    pub events: u64,
    pub jobs: u64,
    pub starts: u64,
    pub pauses: u64,
    pub replans: u64,
    /// How long each job waited before its first start, per lane (seconds).
    pub waits_interactive: Vec<u64>,
    pub waits_batch: Vec<u64>,
    /// Shard-seconds of work offered, and the GPU's capacity over the arrival window.
    pub offered: u64,
    pub capacity: u64,
}

impl SimStats {
    fn add(&mut self, s: SimStats) {
        self.events += s.events;
        self.jobs += s.jobs;
        self.starts += s.starts;
        self.pauses += s.pauses;
        self.replans += s.replans;
        self.waits_interactive.extend(s.waits_interactive);
        self.waits_batch.extend(s.waits_batch);
        self.offered += s.offered;
        self.capacity += s.capacity;
    }
}

fn pct(v: &mut [u64], p: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p).round() as usize]
}

const TOTAL: u32 = 110;
const GRACE: u64 = 5;
const SMART_S: u64 = 600;

/// One run: jobs arrive for `arrive_until` seconds (each second with probability `per_mille`/1000), then the
/// queue drains. Panics if a guarantee is broken.
pub fn simulate(seed: u64, arrive_until: u64, per_mille: u64) -> SimStats {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut jobs: Vec<Job> = vec![];
    let mut truth: Vec<Truth> = vec![];
    let mut pausing_left: Vec<(JobId, u64)> = vec![];
    let mut st = SimStats::default();
    let mut t: u64 = 0;
    let deadline = arrive_until + 2_000_000; // generous: every job is finite, so the queue must drain long before this
    st.capacity = TOTAL as u64 * arrive_until;

    loop {
        // arrivals
        if t < arrive_until && rng.below(1000) < per_mille {
            let id = jobs.len() as JobId + 1;
            let interactive = rng.chance(30);
            let shards = match rng.below(10) {
                0 => TOTAL, // whole GPU (a big LLM)
                1..=3 => 40 + rng.below(50) as u32,
                _ => 1 + rng.below(30) as u32,
            };
            let mut j = job(
                id,
                if interactive { Lane::Interactive } else { Lane::Batch },
                shards,
                State::Pending,
                t,
            );
            j.detached = rng.chance(85);
            j.no_pause = !j.detached || rng.chance(10);
            j.mode = match rng.below(10) {
                0 => Mode::Wait,
                1 => Mode::Pause,
                _ => Mode::Smart,
            };
            j.pending_since = t;
            let work = 5 + rng.below(if interactive { 120 } else { 3000 });
            let lie = match rng.below(10) {
                0 => 0.2, // wildly optimistic: says 20% of the real time
                1 => 3.0, // pessimistic
                _ => 1.0,
            };
            truth.push(Truth {
                work_left: work,
                lie,
                reports: rng.chance(80),
            });
            st.offered += work * shards as u64;
            jobs.push(j);
            st.jobs += 1;
            st.events += 1;
            // what the daemon does on submit: plan if it does not fit
            let (lane, mode) = (jobs[id as usize - 1].lane, jobs[id as usize - 1].mode);
            let p = sched::plan(&jobs, TOTAL, shards, lane, mode, t, SMART_S, GRACE);
            if shards > p.free_now && !p.pause.is_empty() {
                pause_for(&mut jobs, &mut pausing_left, &p.pause, id, t, &mut st);
            }
        }

        // running jobs work; pausing jobs stop after the grace period and go back to the queue
        for (i, j) in jobs.iter_mut().enumerate() {
            if j.state == State::Running {
                let tr = &mut truth[i];
                tr.work_left = tr.work_left.saturating_sub(1);
                if tr.reports && (t.is_multiple_of(30) || j.progress.eta_s.is_none()) {
                    j.progress = Progress {
                        message: String::new(),
                        eta_s: Some((tr.work_left as f64 * tr.lie) as u64),
                        updated_at: t,
                    };
                }
                if tr.work_left == 0 {
                    j.state = State::Done;
                    j.ended_at = Some(t);
                    st.events += 1;
                }
            }
        }
        pausing_left.retain_mut(|(id, left)| {
            *left = left.saturating_sub(1);
            if *left > 0 {
                return true;
            }
            let j = &mut jobs[*id as usize - 1];
            j.state = State::Pending; // requeued; keeps its place (submitted_at) and its checkpointed progress
            j.requeues += 1;
            j.pending_since = t;
            j.started_at = None;
            st.events += 1;
            false
        });

        // the daemon's re-plan for the job at the front (every 15 s)
        if t.is_multiple_of(15) && !jobs.iter().any(|j| j.state == State::Pausing) {
            if let Some((id, need, lane, mode)) = sched::order(&jobs).first().map(|j| (j.id, j.shards, j.lane, j.mode)) {
                if matches!(mode, Mode::Smart | Mode::Pause) {
                    let p = sched::plan(&jobs, TOTAL, need, lane, mode, t, SMART_S, GRACE);
                    if need > p.free_now && !p.pause.is_empty() {
                        st.replans += 1;
                        pause_for(&mut jobs, &mut pausing_left, &p.pause, id, t, &mut st);
                    }
                }
            }
        }

        // start what fits; paused jobs stay held until the job they made room for has started
        let waiting: Vec<JobId> = jobs.iter().filter(|j| j.state == State::Pending).map(|j| j.id).collect();
        let held: Vec<JobId> = jobs
            .iter()
            .filter(|j| j.paused_for.is_some_and(|b| waiting.contains(&b)))
            .map(|j| j.id)
            .collect();
        for id in sched::to_start(&jobs, TOTAL, &held) {
            for other in jobs.iter_mut().filter(|o| o.paused_for == Some(id)) {
                other.paused_for = None;
            }
            let j = &mut jobs[id as usize - 1];
            if j.requeues == 0 {
                let waited = t - j.submitted_at;
                if j.lane == Lane::Interactive {
                    st.waits_interactive.push(waited);
                } else {
                    st.waits_batch.push(waited);
                }
            }
            j.state = State::Running;
            j.started_at = Some(t);
            j.paused_for = None;
            j.top = false;
            st.starts += 1;
            st.events += 1;
        }

        // the guarantees, every step
        let queue: Vec<JobId> = sched::order(&jobs).iter().map(|j| j.id).collect();
        for j in jobs.iter().filter(|j| j.state == State::Pending) {
            if let Some(b) = j.paused_for.filter(|b| queue.contains(b)) {
                let (pj, pb) = (queue.iter().position(|x| *x == j.id), queue.iter().position(|x| *x == b));
                assert!(
                    pb < pj,
                    "seed {seed} t {t}: paused job {} is ahead of job {b}, which it is held for (deadlock)",
                    j.id
                );
            }
        }
        let used = sched::used_shards(&jobs);
        assert!(used <= TOTAL, "seed {seed} t {t}: {used} shards handed out, only {TOTAL} exist");
        assert!(jobs.iter().all(|j| j.shards <= TOTAL));

        t += 1;
        if t >= arrive_until && jobs.iter().all(|j| j.state.is_terminal()) {
            return st;
        }
        assert!(
            t < deadline,
            "seed {seed}: the queue did not drain (starvation): {:?}",
            jobs.iter()
                .filter(|j| !j.state.is_terminal())
                .map(|j| (j.id, j.state, j.shards, &j.reason))
                .collect::<Vec<_>>()
        );
    }
}

fn pause_for(jobs: &mut [Job], pausing: &mut Vec<(JobId, u64)>, ids: &[JobId], for_job: JobId, t: u64, st: &mut SimStats) {
    jobs[for_job as usize - 1].top = true;
    for id in ids {
        let j = &mut jobs[*id as usize - 1];
        assert!(j.pausable(), "plan paused job {id}, which may not be paused");
        j.state = State::Pausing;
        j.pausing_since = Some(t);
        j.paused_for = Some(for_job);
        pausing.push((*id, GRACE));
        st.pauses += 1;
        st.events += 1;
    }
}

fn run_until(target: u64, arrive_until: u64, per_mille: u64, first_seed: u64) -> (SimStats, u64) {
    let mut total = SimStats::default();
    let mut seed = first_seed;
    while total.events < target {
        total.add(simulate(seed, arrive_until, per_mille));
        seed += 1;
    }
    (total, seed - first_seed)
}

fn report(name: &str, mut s: SimStats, runs: u64) {
    println!(
        "{name}: {} events over {runs} runs ({} jobs, load {:.0}% of the GPU): {} starts, {} pauses ({:.2} per job, {} from \
         re-planning), 0 over-allocations, 0 deadlocks, every job finished. Wait before first start: interactive median \
         {} s, p95 {} s; batch median {} s, p95 {} s",
        s.events,
        s.jobs,
        100.0 * s.offered as f64 / s.capacity.max(1) as f64,
        s.starts,
        s.pauses,
        s.pauses as f64 / s.jobs.max(1) as f64,
        s.replans,
        pct(&mut s.waits_interactive, 0.5),
        pct(&mut s.waits_interactive, 0.95),
        pct(&mut s.waits_batch, 0.5),
        pct(&mut s.waits_batch, 0.95),
    );
}

fn sim_target() -> u64 {
    std::env::var("DOUM_SIM_EVENTS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000)
}

/// A busy workstation: around 80% of the GPU's capacity asked for, arriving at random.
#[test]
fn sim_typical_load() {
    let (s, runs) = run_until(sim_target() / 2, 8 * 3600, 2, 1);
    report("typical load", s, runs);
}

/// Far more work than the GPU can do: the guarantees must still hold and the queue must still drain.
#[test]
fn sim_heavy_overload() {
    let (s, runs) = run_until(sim_target() / 2, 3600, 40, 1_000_001);
    report("heavy overload", s, runs);
}

#[test]
fn sim_is_deterministic() {
    let a = simulate(42, 1800, 40);
    let b = simulate(42, 1800, 40);
    assert_eq!((a.events, a.starts, a.pauses), (b.events, b.starts, b.pauses));
}
