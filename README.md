# doum

**Share one GPU between ComfyUI, local LLMs, training and AI agents — without out-of-memory crashes.**
A smart queue for a single AI workstation: memory shards, wait-or-pause, automatic resume, notifications, and
(coming) MCP tools so AI agents take turns too.

> Named after the **doum palm** (دوم) of the Nile in Sudan: a tough, branching palm that survives the heat and gives
> shade to every traveller who stops under it. One GPU, shade for every job.

> **Status: pre-alpha, under active development.** Built and tested on an NVIDIA GB10 (DGX Spark-class, 128 GB unified
> memory). Not ready for general use yet; see the roadmap.

## The problem

You run ComfyUI renders, a local LLM, a fine-tune and maybe an agent or two on one GPU. Start two heavy things at once
and something crashes, or on unified-memory machines the whole box can lock up or power off at a model-load peak.
ComfyUI queues its own work, Ollama queues its own work, but nothing queues *across* them.

## What doum does

- **One queue for every GPU app.** Jobs ask for *shards* of GPU memory (~1 GB each). doum starts a job only when it
  fits; small jobs share the GPU, big ones get it alone.
- **Two lanes.** `--interactive` (someone is waiting: a chat, a writer) is always scheduled before batch work.
- **Wait, pause or smart.** When a job does not fit, doum can wait, pause what is in the way, or be *smart*: let jobs
  that finish soon complete and pause the long ones. Paused jobs get a signal to save a checkpoint, then **resume on
  their own** when the GPU is free, ahead of newer work.
- **Asks you when it matters.** With `mode = ask`, a desktop pop-up offers **Smart / Wait / Pause now**, with time
  estimates from each job's own progress reports; no answer → the recommendation. If something you are waiting for has
  waited too long (default 20 min), it checks back with you.
- **Notices what bypasses it.** GPU processes outside the queue are reported (and alerted once).
- **Pre-start safety hooks.** Free-memory check before every job (unified-memory aware), unload of an idle ComfyUI's
  cached models, and your own gate (e.g. a thermal cool-down) as a hook.
- **Survives restarts.** Background jobs run in their own process group; the daemon can restart without killing them.

## Quick look

```console
$ doum run -d -s 60 -n comfy-render -- python render_queue.py      # background, pausable
$ doum run -d -s 24 -n tts-train    -- python train.py             # reports `doum progress "epoch 3/10" --eta 2h`
$ doum run -i -s 110 -n writer      -- ollama-writer.sh            # needs the whole GPU, someone is waiting
doum: pausing job(s) [2] to make room
doum: expected start in ~5 min
...
doum: GPU ready, starting

$ doum status
GPU  [██████████████████████░░░░░░░░░░░░░░░░░░]  60/110 shards used · 94 GB memory free

RUNNING
     1  comfy-render                   60 shards  batch     2m  ~5 min left  clip 2/5

WAITING (in start order)
     3  writer                        110 shards  interactive  waiting for 110 shards (50 free)
     2  tts-train                      24 shards  batch        paused; resumes when the GPU is free
```

## Design

```
 CLI · panel · desktop/phone alerts · MCP (agents)
                    │
              doum daemon  ── smart decisions, ETAs, check-back, notifications
                    │
     backend: native (built in)  |  slurm (optional, for Slurm users)
                    │
   ComfyUI (gate plugin) · llama-swap / Ollama / llama.cpp · training · scripts
                    │
          pre-start hooks (memory, your thermal gate)  →  GPU
```

The scheduling rules are pure functions with unit tests (`crates/doum/src/sched.rs`).

## Roadmap

- [x] Native queue: shards, lanes, strict order, smart / wait / pause / ask, requeue + resume, check-back,
      unmanaged-process detection, notifications (desktop, ntfy, webhooks)
- [x] systemd user service
- [x] llama-swap integration: local LLMs started on demand as doum jobs, unloaded when idle
      (tested on a GB10: gpt-oss-120b loads + answers in 37 s, 37 tok/s warm; memory fully returned on unload)
- [x] `doum top` (terminal UI) and a web panel with history charts and energy per job
- [x] MCP server for AI agents (agent-safe limits) + Claude Code skill and hook
- [ ] ComfyUI gate plugin (each render waits its turn; queue saved and restored on pause)
- [ ] Slurm backend
- [ ] Prometheus metrics, docs site, packages, v0.1.0

## License

Apache-2.0
