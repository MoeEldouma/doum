# Changelog

All notable changes to Doum are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and Doum uses [Semantic Versioning](https://semver.org/).

## Unreleased

### Fixed
- ComfyUI: a paused prompt is put back into ComfyUI's own queue, at the front, before its run ends, so it always
  runs next. Re-queueing over HTTP a moment later let the next prompt in line run first. (Falls back to HTTP on
  ComfyUI versions with a different queue.) Tested in CI.

## v0.2.0 (2026-10-21)

### Added
- Slurm backend (`backend = "slurm"`): Doum decides, Slurm runs. Background jobs are submitted with `sbatch
  --gres=shard:N` to the lane's partition and show in `squeue` as `doum-<id>-<name>`; pause and cancel go through
  `scancel` (SIGTERM, SIGKILL after the grace period; a job Slurm hasn't started is cancelled); a restarted daemon picks
  Slurm jobs up by their Slurm id. Plain Slurm jobs that are not Doum's count against the GPU and show in `doum status`,
  the panel, `doum top` and `/metrics` (`doum_external_shards`, `doum_backend`).
- `doum doctor`: checks the daemon, state directory, GPU and driver, whether jobs can be frozen in place, the Slurm
  partitions and shards, and alerts.
- Re-planning: estimates well past due count as unknown, and the job at the front of the queue is re-planned every 15 s,
  so a job that ran past its estimate is paused instead of being waited on forever.

- Correctness checks (docs/correctness.md): property tests (proptest) for the scheduling rules and a deterministic
  simulator (1,000,629 events across typical load and 15x overload: 0 over-allocations, 0 deadlocks, every job
  finished).

### Fixed
- The Claude hook checks each part of a compound command: `echo doum; ollama run x` used to pass because the command
  mentioned `doum`.
- The memory check uses the GPU's own free memory when the card reports it (discrete GPUs), not system RAM.
- Agent limits (`[mcp]` max shards, max jobs, mode) are enforced by the daemon too, for every job submitted as owner
  `agent`.
- Exit records are removed once a job has finished, and logs when Doum forgets the job (7 days after it finished).
- Two deadlocks found by the simulator: smart mode could pause an interactive job for a batch job, which then sat ahead
  of it while held for it; and a job's "paused for" mark outlived the job it was for, so two jobs could end up held for
  each other. Nothing pauses a higher lane now, and the mark is cleared when that job starts.

### Changed
- Smart mode only pauses jobs in a lower lane (interactive work over batch work); jobs in the same lane no longer
  pause each other (in the simulator they did, about 20 times per job). Same-lane pausing needs `-m pause` or
  "Pause now".
- `deploy/slurm`: the backfill scheduler (Slurm 23.11 without an accounting database only starts jobs in the backfill
  pass) and no time limit on the interactive partition.

## v0.1.0 (2026-10-21)

First public release. Linux only; tested on one NVIDIA GB10 (128 GB unified memory).

### Added
- Web panel: a live timeline of every job's runs on parallel tracks (pauses shown as gaps), an animated shard bar
  (stable colour per job, striped while pausing, dashed outline for what the next job is waiting for), progress bars
  from each job's ETA, and new jobs sliding in. Jobs keep the start and end of each paused run (`runs`).
- An alert when a job cannot be started at all.
- Docs: phone alerts guide (docs/phone-alerts.md), panel and phone demo media (docs/media/).
- Native GPU queue in a single daemon (`doum daemon`, `127.0.0.1:8420`): jobs reserve memory shards (~1 GB each,
  110 by default) and start only when their shards are free.
- Two lanes: interactive before batch; strict order (nothing starts past a waiting job at the front); `doum up` to
  move a waiting job to the front of its lane.
- Modes for a busy GPU: `wait`, `pause`, `smart` (wait for jobs finishing within `smart_wait_s`, pause longer ones)
  and `ask` (desktop question with Smart / Wait / Pause now, recommendation applied after `ask_timeout_s`).
- Pause as stop + requeue: SIGTERM to the job's process group, SIGKILL after `pause_grace_s`, automatic resume in
  the job's original place; paused jobs are held until the job they made room for has started.
- Check-back: interactive and foreground jobs waiting longer than `max_wait_s` trigger a pause-now / keep-waiting /
  cancel question.
- Foreground (`doum run`) and detached (`doum run -d`) jobs; logs for detached jobs (`doum logs`).
- Progress reports from inside jobs (`doum progress "..." --eta 2h`, `--no-pause`), used for ETAs and smart decisions.
- Pre-start checks: free-memory wait based on `MemAvailable` (unified-memory aware), unloading an idle ComfyUI's
  models, user `pre_start` / `post_end` hooks.
- Detection of GPU processes outside the queue via `nvidia-smi`, with one alert per process.
- Notifications: desktop (with answer buttons), ntfy phone push, JSON webhooks.
- GPU energy attributed to jobs by shards; 24-hour in-memory history.
- `doum status` (with `--all` and `--json`), `doum top` terminal UI, `doum panel` web panel with charts and controls.
- `doum mcp`: MCP server for AI agents with seven tools and agent limits (`[mcp]`).
- `doum claude-hook`: Claude Code PreToolUse hook that blocks GPU commands outside the queue, with a `# doum:allow`
  escape; `gpu-share` skill for Claude Code.
- ComfyUI gate plugin (`integrations/comfyui/doum_gate`): leases, progress reports, idle release, stop handshake,
  re-queue of the interrupted prompt, orphaned-lease cleanup.
- llama-swap integration example: model servers started on demand as interactive Doum jobs.
- Prometheus metrics at `GET /metrics`.
- State persisted to disk; detached jobs keep running through a daemon restart, including
  `systemctl --user restart doum` (the unit uses `KillMode=process`), and are picked up again.
- Cancel stops a running background job with SIGTERM, then SIGKILL after `pause_grace_s`.
- Local-only API: `listen` must be a loopback address, requests with any other `Host` header are refused (DNS
  rebinding), changes need the token (header, or the panel's cookie with an origin check). The API never returns a
  job's environment; the state directory is mode 0700 and `state.json` mode 0600.
- `[claude_hook] hint`: your own text added to the hook's message.
- systemd user units, `install.sh` (checksum-verified), release builds for x86_64 and aarch64 Linux.
- Experimental single-node Slurm configuration and install scripts in `deploy/slurm` (not used by the CLI yet).
- Documentation: getting started, configuration reference, integrations, how it works.
