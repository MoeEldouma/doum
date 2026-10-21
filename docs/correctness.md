# Correctness: how Doum's scheduling is checked

A GPU queue that hands out too much memory, deadlocks, or keeps knocking the same job out is worse than no queue.
Doum's scheduling rules are pure functions (`crates/doum/src/sched.rs`: no I/O, no clock, no daemon state), so they
can be checked far more thoroughly than with a handful of examples. Three layers do that.

## 1. Examples

Hand-written unit tests in `sched.rs` pin down the rules with readable scenarios: interactive before batch, strict
order, smart waits for a render with 5 minutes left but pauses a 2-hour training run, a stale estimate stops counting.

## 2. Properties, on thousands of random cases

`crates/doum/src/proof.rs` uses [proptest](https://github.com/proptest-rs/proptest) to generate random mixes of
jobs (lanes, sizes, states, estimates old and new, pausable or not) and checks that each rule always holds. Each
property runs on 4,000 random cases on every `cargo test`; when one fails, proptest shrinks it to the smallest
example that still fails.

| Property | What it guarantees |
|---|---|
| `order_is_lane_then_top_then_fifo` | Interactive jobs start before batch jobs; within a lane, jobs moved up come first, then first come, first served |
| `to_start_never_overcommits_and_is_strict` | Starting jobs never hands out more shards than exist; what starts is always the front of the queue, and the first job left waiting is either held or genuinely does not fit |
| `plans_are_safe_sufficient_and_minimal` | `wait` never pauses; only pausable jobs are paused; smart never pauses a job about to finish, nor one in the same lane; nothing ever pauses a job in a higher lane; a plan that promises a start time really frees enough; no pause is wasted |
| `estimates_count_down_then_expire` | A job's time left only goes down, and once its estimate is stale it stays stale |

## 3. A simulator, a million events

The simulator in `proof.rs` drives the same functions the daemon uses through random workloads, second by
second: jobs arrive (a third of them interactive, one in ten needing the whole GPU), run, report progress (some
honestly, some 5x too optimistic, some not at all), get paused and resumed, and finish. It re-plans every 15 seconds
like the daemon does, and checks after **every step** that:

- the shards handed out never exceed the total;
- no paused job is ever ahead of the job it is held for (which would be a deadlock);
- the queue always drains: every job finishes (no starvation).

It is deterministic (a seeded generator), so any failure can be replayed exactly.

Results of the full run (`DOUM_SIM_EVENTS=1000000`, about a minute on one machine):

| Scenario | Events | Jobs | GPU load | Over-allocations | Deadlocks | Jobs that never finished | Pauses per job |
|---|---|---|---|---|---|---|---|
| Typical: random arrivals over 8 hours | 500,106 | 132,629 | 78% | 0 | 0 | 0 | 0.26 |
| Heavy overload: 15x more work than the GPU can do | 500,523 | 132,374 | 1,543% | 0 | 0 | 0 | 0.26 |

At typical load, interactive jobs started after a median of **4 seconds** (95th percentile 23 minutes, when a
whole-GPU job had to wait for work that may not be paused). Batch jobs waited a median of 40 minutes, as you'd expect
at 78% load with jobs that need the whole GPU.

## What the simulator found

It earned its keep. Three real bugs in the scheduling rules, all fixed:

1. **A deadlock across lanes.** Smart mode could pause an *interactive* job to make room for a *batch* job. The
   paused interactive job then sat at the front of the queue, held until the batch job started, while blocking it.
   Fix: nothing ever pauses a job in a higher lane.
2. **A deadlock through stale marks.** Job B pauses job A, and B starts; A still carries "paused for B". Later A
   comes to the front, and re-planning pauses B for A. Now each is held for the other. Fix: when a job starts, every
   job that was paused for it is released.
3. **Thrashing.** Smart mode let jobs in the same lane pause each other; with re-planning every 15 seconds, jobs
   were paused and restarted about 20 times each. Fix: smart mode only pauses jobs in a *lower* lane. Pausing within
   a lane happens only when you ask for it (`-m pause`, or "Pause now"). Pauses dropped to 0.26 per job.

## What this does not cover

The proofs cover the scheduling *decisions*. The daemon around them (processes and signals, Slurm, files, the API)
is covered by end-to-end tests on a real machine with stand-in CPU jobs (pause and resume with a checkpoint, a daemon
restart mid-job, cancel, Slurm submission, plain Slurm jobs counted against the GPU), not by the simulator.

## Run it yourself

```sh
cargo test                                                        # examples + properties + a short simulation
DOUM_SIM_EVENTS=1000000 cargo test --release sim_ -- --nocapture  # the full run, with the numbers above
```
