# How it works

This page describes what the code does, so you can predict what Doum will do. The scheduling rules live in
`crates/doum/src/sched.rs` as pure functions with unit tests; the daemon (`daemon.rs`) runs them once a second.

## Shards

Doum does not measure GPU memory per job. Each job *says* how many shards it needs (`--shards`, about 1 GB each), and
the daemon makes sure the shards of all running jobs never add up to more than `total_shards` (110 by default). A job
that asks for more than the total is lowered to the total. Whether a job really stays within its shards is up to the
job; the [free-memory check](#pre-start-checks) and [unmanaged-process detection](#unmanaged-gpu-processes) are
there to catch the cases where reality and the numbers disagree.

## Scheduling rules

### Order

Waiting jobs are sorted by:

1. **lane**: interactive before batch;
2. **moved up**: jobs moved to the front (`doum up`, or a job that other jobs were paused for) before the rest of
   their lane;
3. **submission time**: first come, first served;
4. **id**, as a tie-break.

A paused job keeps its original submission time, so when it is requeued it goes back to its old place, ahead of work
submitted after it.

### Strict order

Once a second the daemon walks the waiting jobs in that order and starts each one that fits in the free shards. At the
first job that does not fit, it stops: nothing behind that job starts, even if it would fit. A small job never sneaks
past a big one that is waiting at the front, so the big one does not starve.

A job that is *held* also stops the walk. A job is held while:

- a question about it is open (mode `ask`), or
- it was paused to make room for another job, and that job has not started yet (`paused_for`). This stops a paused
  job from taking its shards straight back.

### When a new job does not fit

This is decided when the job is submitted, and only if it needs more shards than are free at that moment. The result
is a *plan*: which running jobs to pause, which to wait for, and an estimated start time.

- Only running **detached** jobs that did not ask for `--no-pause` can be paused. Foreground jobs are never paused.
- **wait**: pause nothing. Wait for running jobs, soonest-finishing first, until enough shards would be free.
- **Lanes come first.** Nothing ever pauses a job in a higher lane: batch work never stops interactive work.
- **pause** (chosen explicitly): every pausable running job in the same lane or a lower one is a candidate.
- **smart**: only pausable jobs in a *lower* lane are candidates (interactive work steps in front of batch work; jobs
  in the same lane are first come, first served and never knock each other out), and only if their time left (from
  the last `doum progress --eta`, counted down since the report) is more than `smart_wait_s` (600 s), or unknown.
- Candidates are paused longest-remaining first (unknown counts as longest; then larger jobs first), only as many as
  needed to free enough shards.
- If pausing every candidate is still not enough, those candidates are paused anyway and the rest of the running jobs
  are waited for, soonest first.
- The estimated start is `pause_grace_s` if pausing alone makes room, otherwise the longest time left among the jobs
  waited for (unknown if any of them never reported an ETA).
- **ask**: the daemon works out the smart, wait and pause plans, and asks you with a time estimate for each (desktop
  pop-up, phone, webhooks, `doum status`, `doum top`, the panel). The job is held until you answer; after
  `ask_timeout_s` (60 s) the recommendation, smart, is applied.

When a plan pauses jobs, the new job is moved to the front of its lane, and the paused jobs are marked as paused for
it (see *held* above). The mark is cleared the moment that job starts, so a paused job can never end up waiting for
a job that is in turn waiting for it. These rules are checked on thousands of random cases and a million-event
simulation; see [Correctness](correctness.md).

### Re-planning when an estimate goes stale

A smart plan is only as good as the estimates it used, so the daemon looks again every 15 seconds at the job at the
front of the queue. Two things change a plan:

- **A stale estimate.** A job's time left is its last `doum progress --eta`, aged by the time since that report. Once a
  job is past its estimate by half the estimate (at least a minute), the estimate no longer counts: the job is treated
  like one that never gave an ETA. A render that said "5 minutes left" is waited for until 7.5 minutes have passed.
- **A longer estimate.** A job that reports a new, longer ETA beyond `smart_wait_s` no longer qualifies to be waited for.

If the front job's mode is `smart` or `pause` and its plan now pauses something, that happens straight away, and the
paused job's reason (and alert) says it *ran past its estimate*. Ask-mode jobs don't act on their own; the
check-back asks you instead. `wait` never pauses anything.

### Check-back

Every second, the daemon looks for waiting jobs that are in the interactive lane or in the foreground, have a non-zero
`max_wait_s` (default 1200 s, or `--max-wait`), have waited at least that long since they were submitted, requeued or
last asked about, and have no open question. For each one it opens a question: **pause-now** (pause whatever is in
the way, as in pause mode), **keep-waiting** (recommended) or **cancel**. The question stays open for
`ask_timeout_s` or 300 s, whichever is longer. The timer restarts when the question is opened, so a job that keeps
waiting is asked again after another `max_wait_s`. Batch jobs waiting in line never get a check-back.

Questions about a job that is no longer waiting are closed as "moot".

## Pausing

Pausing is stop + requeue, not a freeze:

1. The job is marked *pausing* and SIGTERM is sent to its process group. It keeps its shards while it stops.
2. The job runner passes SIGTERM (and SIGINT, SIGHUP, SIGUSR1) on to the command's own process group.
3. If the job has not exited after `pause_grace_s` (90 s), SIGKILL goes to both process groups.
4. Once it has exited, the job is *pending* again with `requeues + 1` and its original place in the queue.
5. When it is its turn again, the same command is started again (same directory, environment and `DOUM_JOB_ID`).

The job is responsible for saving and restoring its own state; see
[Writing a pausable job](getting-started.md#writing-a-pausable-job).

Cancelling a running background job works the same way, without the requeue: SIGTERM first, SIGKILL to both process
groups if the job is still running after `pause_grace_s`, and the job is marked cancelled once it has exited. A
running foreground job gets SIGTERM only.

## Pre-start checks

When a job's turn comes, it is marked *running* (it now holds its shards) and the job runner (`doum _exec`, or the
`doum run` process for a foreground job) does these steps before the command, unless the job was submitted with
`--no-hooks`:

1. **Free memory.** If `[memory] check` is on, wait until the free memory (the GPU's own when the card reports it,
   otherwise `MemAvailable`) is at least the job's memory plus
   `headroom_gb` (6 GB). The job's memory is `--mem`, or `shards × gb_per_shard` capped at 60% of the total budget.
   On a unified-memory machine this is the real GPU headroom, and it catches memory that the shard numbers do not
   know about: page cache, the desktop, a process outside the queue, a model that a previous job has not fully
   released yet. Checked every 5 s, with no timeout.
2. **Idle ComfyUI.** While short of memory, once per start: if `[comfyui] free_when_idle` is on and ComfyUI's queue is
   empty, ask ComfyUI to unload its cached models (`POST /free`), wait 5 s and check again.
3. **Your `pre_start` hook.** It may block (a thermal cool-down gate, for example). A non-zero exit fails the job with
   exit code 97.
4. The command runs in its own process group. Its exit code is written to `exit/<id>.<attempt>` in the state
   directory.
5. **Your `post_end` hook** runs after the command, whatever its result.

The steps and their results are written to the job's log as lines starting with `[doum]`.

## Unmanaged GPU processes

Every 15 s the daemon runs `nvidia-smi --query-compute-apps=pid,process_name,used_gpu_memory`. A process counts as
part of a Doum job if it is in a running job's process group (the runner's or the command's), descends from the job's
process, or descends from one of the job's `--covers-pid` processes. Everything else that uses at least
`unmanaged_min_gb` is listed under "outside the queue" in `doum status`, `doum top`, the panel and the metrics, and
triggers one alert per process (again only if it disappears and comes back).

On unified memory `nvidia-smi` reports no per-process memory, so the process's resident memory (`VmRSS`) is used
instead. Without `nvidia-smi` this feature is off. Doum only reports these processes; it never stops them.

## Energy accounting

Every 10 s the daemon reads the GPU power draw from `nvidia-smi` and splits `power × elapsed time` between the jobs
holding shards, in proportion to their shards. A job with 30 shards next to one with 90 gets a quarter of the
energy. Gaps longer than 120 s (the daemon was stopped, for example) are not counted. This is an attribution, not a
measurement per process. The totals are saved with each job (`energy_j`) and shown in the panel and in
`doum_job_energy_joules_total`.

The same sample feeds the in-memory history (24 hours, one sample per 10 s) behind the panel's and `doum top`'s charts.

## Restarts

Everything the daemon knows (jobs and open questions) is in `state.json` in the state directory, written atomically
(write to a temporary file, then rename) after every change, with mode 0600. Finished jobs are dropped after 7 days.

Detached jobs are started in a new session (`setsid`), so they have their own process group and are not tied to the
daemon process. The job runner writes each attempt's exit code to `exit/<id>.<attempt>`. When the daemon starts
again, it checks each running job's process id: if the process is still there, the job keeps running; if it is gone,
the exit code is read from the file and the job is marked done, failed or (if it was pausing) requeued. A foreground
job ends with its `doum run` process: if that process has gone away, the job is cancelled (still waiting) or marked
failed (it was running).

So restarts keep background jobs running. This includes `systemctl --user stop doum` / `restart doum` and an
automatic restart after a crash: the shipped `deploy/systemd/doum.service` sets `KillMode=process`, so systemd stops
only the daemon and leaves the jobs (which share the service's cgroup) alone. While the daemon is down nothing new
starts, and pauses, cancels and progress reports have to wait until it is back. If you write your own unit, keep
`KillMode=process`; systemd's default (`control-group`) would send SIGTERM to every job as well.

The in-memory history for the charts is not saved and starts empty after a restart.

## Security model

Doum is built for one user on one machine.

- **Loopback only.** The daemon listens on `127.0.0.1:8420` by default, and `listen` must be a loopback address
  (`127.0.0.1` or `::1`); with anything else the daemon refuses to start. Nothing on the network can reach it.
- **Host check.** The daemon answers only requests whose `Host` header is `127.0.0.1:<port>`, `localhost:<port>` or
  `[::1]:<port>`; everything else gets 403. This stops DNS rebinding, where a web page points its own domain at
  127.0.0.1 to read local services from your browser.
- **Token.** On first start the daemon creates a random token (24 bytes from `/dev/urandom`, 48 hex characters) in
  `<state dir>/token` with mode 0600 (the state directory itself is set to mode 0700 on every start). Every request that changes something needs it, as the `x-doum-token` header
  (CLI, MCP server, jobs, the ComfyUI plugin, the desktop notification buttons) or as the panel cookie.
- **Panel login.** `doum panel` opens `http://127.0.0.1:8420/login?token=<token>` in your browser. The daemon checks
  the token and sets it as a cookie (`HttpOnly`, `SameSite=Strict`, one year). For cookie-authenticated requests, the
  `Origin` header, if present, must be the daemon's own address (`http://127.0.0.1:8420`, or `localhost` instead of
  `127.0.0.1`), which blocks other web pages from sending commands.
- **Read-only endpoints are not authenticated.** `GET /`, `GET /api/status`, `GET /api/jobs/<id>`, `GET /api/metrics`
  and `GET /metrics` answer any local process that connects to the port, including other users' processes on a
  shared machine. They show job names, commands, working directories, owners, progress and log file paths. They do
  **not** show a job's environment: the daemon keeps that (it needs it to restart paused jobs) only in `state.json`,
  which only your user can read.
- **What the token allows.** Submitting a job runs any command as the user the daemon runs as. Treat the token like a
  shell login.
- **Agents.** The MCP limits (`[mcp]`) restrict what an agent can do through the MCP tools. They are not a sandbox:
  the MCP server reads the same token, and an agent with shell access can use the CLI.

### HTTP API

| Method and path | Token | What |
|---|---|---|
| `GET /` | no | The web panel. |
| `GET /login?token=...` | the token itself | Sets the panel cookie and redirects to `/`. |
| `GET /api/status` | no | Everything: shard use, free memory, all jobs, the queue in start order, open questions, processes outside the queue, latest sample. |
| `GET /api/jobs/<id>` | no | One job. |
| `GET /api/metrics?minutes=N` | no | History samples (default 60 minutes). |
| `GET /metrics` | no | Prometheus text format. |
| `POST /api/jobs` | yes | Submit a job (what `doum run` and `gpu_run` send). |
| `POST /api/jobs/<id>/pause` | yes | Pause. |
| `POST /api/jobs/<id>/cancel` | yes | Cancel. |
| `POST /api/jobs/<id>/top` | yes | Move to the front of its lane. |
| `POST /api/jobs/<id>/progress` | yes | `{"message": "...", "eta_s": 300, "no_pause": true}` (`eta_s` and `no_pause` optional). |
| `POST /api/jobs/<id>/done` | yes | A foreground client reports its exit code. |
| `POST /api/decisions/<id>` | yes | Answer a question: `{"choice": "smart"}`. |

The API is internal to Doum and may change before 1.0. `doum status --json` prints the status object.

## Slurm (experimental preview)

`deploy/slurm/` holds a single-node Slurm setup that mirrors Doum's model: a generic resource `shard` with a count of
110 on the GPU, an `interactive` partition that is always considered before `batch`, strict priority order,
requeueable batch jobs with a 90 s `KillWait` to save a checkpoint, and cgroup v2 for process tracking only.
`deploy/slurm/install.sh` installs MUNGE and Slurm from the Ubuntu 24.04 packages (Slurm 23.11), writes this config
(backing up any existing one), starts the services and runs a one-shard test job; `uninstall.sh` removes them.

This is a preview for people who already run Slurm. **The Doum CLI and daemon do not submit to Slurm yet**: there is
no setting for it, and jobs submitted with `doum run` always go to the native queue. The install script runs its test
job as the user who called `sudo` (or the login user). The scripts change system packages and services and need root.
Read them before running them.
