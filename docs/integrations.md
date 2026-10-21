# Integrations

- [ComfyUI](#comfyui): every prompt waits its turn
- [llama-swap](#llama-swap): local LLMs start on demand as Doum jobs
- [Claude Code](#claude-code): MCP tools for agents, a PreToolUse hook, a skill
- [Prometheus and Grafana](#prometheus-and-grafana)
- [Notifications](#notifications): desktop, ntfy, webhooks

The files referred to below are in the repository and in every release archive.

## ComfyUI

`integrations/comfyui/doum_gate` is a ComfyUI custom node package (it adds no nodes). With it, every ComfyUI prompt,
from the UI or from any script posting to `/prompt`, waits for the GPU in the Doum queue.

### Install

```sh
ln -s /path/to/doum/integrations/comfyui/doum_gate /path/to/ComfyUI/custom_nodes/doum_gate
```

Then restart ComfyUI. Its console shows a line starting with `[doum]`:

- `gate active: prompts wait for 60 shards in the doum GPU queue; GPU released after 90s idle`, or
- `not installed (~/.local/bin/doum missing): ComfyUI runs without the queue` if it cannot find the binary (set
  `DOUM_BIN`).

### Settings

Environment variables of the ComfyUI process:

| Variable | Default | Meaning |
|---|---|---|
| `DOUM_COMFY_SHARDS` | `60` | Shards ComfyUI asks for while it has work. |
| `DOUM_COMFY_IDLE_S` | `90` | Seconds with an empty queue before ComfyUI unloads its models and releases the GPU. |
| `DOUM_BIN` | `~/.local/bin/doum` | The Doum binary. If it does not exist the plugin stays inactive. |
| `DOUM_API` | `http://127.0.0.1:8420` | The daemon's address. |
| `DOUM_STATE_DIR` | `~/.local/state/doum` | Doum's state directory, if you moved it. The plugin reads the token from it and keeps its handshake flag files in its `comfyui/` folder. |
| `DOUM_TOKEN_FILE` | `$DOUM_STATE_DIR/token` | The API token file, if it is somewhere else. |
| `DOUM_COMFY_URL` | `http://127.0.0.1:8188` | ComfyUI's own address (used to wait for startup and to re-queue prompts). |

### What it does

- **Lease.** Before a prompt runs, the plugin makes sure ComfyUI holds a *lease*: a detached Doum job named
  `comfyui` of `DOUM_COMFY_SHARDS` shards. The lease is a small holder process that only sleeps; it is submitted with
  `--no-hooks --mem 0` (ComfyUI manages its own memory, so Doum's pre-start checks and your hooks do not run for it)
  and `--covers-pid <ComfyUI's pid>`, so ComfyUI's own GPU use is not reported as outside the queue. If the GPU is
  busy, the prompt waits (and in `ask` mode you get the usual question). Later prompts reuse the lease.
- **Progress.** While prompts run, the plugin reports `rendering (N more queued)` and an ETA based on the average of
  the last five prompt durations, so smart decisions know how long ComfyUI still needs.
- **Idle release.** After `DOUM_COMFY_IDLE_S` seconds with an empty queue, ComfyUI unloads its models and cancels the
  lease, which frees the shards. While idle with a lease it reports `idle, releasing the GPU in Ns`.
- **Stop handshake.** When Doum pauses the lease, the plugin interrupts the running prompt, unloads the models, and
  only then writes a flag file that lets the holder process exit. So the next job does not start until ComfyUI has
  actually let go of the memory. (The holder waits for the flag for up to 120 s; Doum's `pause_grace_s` still applies
  and kills it after 90 s by default.) If the lease is paused while ComfyUI is idle, it simply releases it.
- **Re-queue.** The interrupted prompt is sent back to ComfyUI's queue at the front. The lease goes back to the Doum
  queue as a paused job, and ComfyUI carries on when it gets the GPU again.
- **Orphaned lease cleanup.** If ComfyUI exits or crashes while holding a lease, the daemon notices that the covered
  process is gone and cancels the lease.
- **Doum not running.** Prompts run without the queue, exactly as without the plugin.

### Known limits

- A prompt that ComfyUI had already taken off its queue when the pause happened (it is then waiting in the gate for
  the GPU) runs before the re-queued prompt.
- The interrupted prompt starts again from the beginning; ComfyUI has no way to resume a half-finished prompt.
- The lease covers all of ComfyUI at a fixed size. A workflow that needs more memory than `DOUM_COMFY_SHARDS` is not
  stopped by Doum.

## llama-swap

[llama-swap](https://github.com/mostlygeek/llama-swap) is a proxy that starts a model server when a request for that
model arrives and stops it after it has been idle. With Doum, each model server is started through `doum run`, so a
model load waits for the GPU like any other job.

The example config is `integrations/llama-swap/config.example.yaml`. The important part is the model's `cmd`:

```yaml
models:
  "qwen3.6-35b":
    cmd: doum run --interactive --shards 30 --mem 28 --name llm-qwen3.6-35b -- ${ollama}
    proxy: http://127.0.0.1:${PORT}
    checkEndpoint: /v1/models
    useModelName: "qwen3.6:35b-a3b"
    ttl: 300
```

How a model becomes a Doum job:

1. A request for `qwen3.6-35b` reaches llama-swap. It runs the `cmd`.
2. `doum run --interactive` submits a **foreground** job in the interactive lane, so it starts before batch work. With
   your `default_mode`, it may pause batch jobs, ask you, or wait.
3. When the job starts, Doum runs the pre-start checks (memory, your hooks) and then the model server (here
   `ollama serve` on llama-swap's port). llama-swap waits for `checkEndpoint` to answer, up to `healthCheckTimeout`
   (1500 s in the example, long enough to cover a wait in the queue up to the 20-minute check-back plus the load).
4. After `ttl` seconds without requests, llama-swap stops the server. `doum run` passes the stop signal on, the server
   exits, and the job ends, freeing its shards. If llama-swap stops it while it is still waiting in the queue, the job
   is cancelled.

Notes:

- Model jobs are foreground jobs, so Doum never pauses them; they leave when idle.
- `--shards` is the GPU budget for the model; `--mem` is what it really uses, for the free-memory check.
- The example runs one Ollama server per model through the `ollama` macro; llama.cpp's `llama-server` or any other
  OpenAI-compatible server works the same way.
- `deploy/systemd/llama-swap.service` runs llama-swap as a user service on `127.0.0.1:8410` with its config at
  `~/.config/llama-swap/config.yaml`. Installing llama-swap itself is up to you.
- `tests/llm_cycle.sh <model-id>` runs a full cycle against a real llama-swap (request, queue, load, answer, unload,
  shards freed). It uses the GPU, so run it only when you mean to.

## Claude Code

Three pieces, each optional. They also work with other MCP clients (the MCP server) and are described in
`claude/README.md`.

### MCP server

```sh
claude mcp add --scope user doum -- doum mcp
```

`doum mcp` speaks MCP over stdio and talks to the daemon. Tools:

| Tool | What it does |
|---|---|
| `gpu_status` | Running jobs with progress and time left, the waiting queue in start order, open questions, GPU processes outside the queue. |
| `gpu_run` | Queue a command as a detached job (`name`, `command`, `shards`, optional `mem_gb`, `cwd`, `interactive`). The command runs with `bash -lc`. Returns the job id. |
| `gpu_job` | One job's state, reason, progress, exit code and the last 25 lines of its log. |
| `gpu_wait` | Wait until a job starts (or finishes, with `until_finished`), up to `timeout_s` (default 120, max 600). |
| `gpu_pause` | Pause an agent job. |
| `gpu_cancel` | Cancel an agent job. |
| `gpu_answer` | Answer an open question, only if `[mcp] allow_answer = true`. |

Agent limits (config `[mcp]`, see [Configuration](configuration.md#mcp)): at most `max_shards` (60) per job, at most
`max_jobs` (2) unfinished agent jobs, agent jobs use mode `wait` so they never pause your work, agents can pause and
cancel only jobs started through MCP (owner `agent`), and they cannot answer your questions. The daemon enforces the shard,
job and mode limits too, for every job submitted as owner `agent`, however it reaches the API. They are guard rails,
not a sandbox: agents run as you, so an agent with a shell can still run `doum run` directly (the hook below nudges
it to).

### PreToolUse hook

`doum claude-hook` reads a Claude Code tool call on stdin. For a Bash command that matches one of the
`[claude_hook] gpu_patterns` (by default: `ollama run|serve`, `llama-server|cli|bench`, `vllm serve`,
`sglang.launch_server`, `basert bench|serve|...`, `torchrun`, `accelerate launch`, and `python ...` running a script
whose name contains train, finetune, infer, inference, generate or render), it returns a "deny" decision with a
reason that tells Claude to use the Doum MCP tools or `doum run`. Everything else passes.

Add it to `~/.claude/settings.json`:

```json
{ "hooks": { "PreToolUse": [ { "matcher": "Bash",
    "hooks": [ { "type": "command", "command": "doum claude-hook", "timeout": 10 } ] } ] } }
```

- Each part of a compound command (split on `;`, `&&`, `||`, `|`, `&` and newlines, with quoted text kept whole) is
  checked on its own. A part that starts GPU work passes only if Doum runs it, so `doum run ... -- python train.py`
  is allowed, while `echo doum; ollama run qwen3` is blocked.
- Escape hatch for a deliberate exception: put `# doum:allow` in the command. The skill tells Claude to ask you first.
- It is a backstop for honest mistakes, not a security boundary: a command that hides GPU work well enough (in a
  script file, for example) is not caught, and `# doum:allow` lets anything through.
- To add your own advice to the message (for example where local LLMs are served), set `[claude_hook] hint` in the
  config (see [Configuration](configuration.md#claude_hook)).

### Skill

`claude/skills/gpu-share/SKILL.md` teaches Claude how to size jobs in shards, when to use foreground, detached and
interactive, how to report progress, and the rules (never start GPU work directly, do not touch other people's jobs).
Copy it to `~/.claude/skills/gpu-share/`, and add one line to `~/.claude/CLAUDE.md`:
"GPU work goes through doum: use the gpu-share skill." It mentions the llama-swap address from the example config
(`http://127.0.0.1:8410/v1`); change it if yours differs.

## Prometheus and Grafana

The daemon serves Prometheus text format at `GET http://127.0.0.1:8420/metrics`. It needs no token. Since the daemon
listens only on loopback, Prometheus has to run on the same machine (a Prometheus container needs host networking),
and the scrape target must be `127.0.0.1:8420`, `localhost:8420` or `[::1]:8420`: the daemon answers 403 to any other
`Host` header, so a target such as `host.docker.internal:8420` does not work.

```yaml
# prometheus.yml
scrape_configs:
  - job_name: doum
    scrape_interval: 15s
    static_configs:
      - targets: ["127.0.0.1:8420"]
```

Metrics:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `doum_up` | gauge | | 1 while the daemon answers. |
| `doum_shards_total` | gauge | | The shard budget (`total_shards`). |
| `doum_shards_used` | gauge | | Shards held by running (and pausing) jobs. |
| `doum_memory_available_gb` | gauge | | `MemAvailable` in GB (GPU headroom on unified memory; system RAM otherwise). |
| `doum_jobs` | gauge | `state` | Jobs the daemon knows about, by state: pending, running, pausing, done, failed, cancelled. Finished jobs are kept for 7 days. |
| `doum_job_shards` | gauge | `id`, `name`, `lane`, `state` | Shards each unfinished job holds (running) or asks for (waiting). |
| `doum_job_wait_seconds` | gauge | `id`, `name`, `lane` | How long each waiting job has been waiting. Resets when it is requeued or after a check-back. |
| `doum_job_energy_joules_total` | counter | `id`, `name`, `lane` | GPU energy attributed to each job. |
| `doum_job_requeues_total` | counter | `id`, `name`, `lane` | Times each job was paused and requeued. |
| `doum_decisions_open` | gauge | | Questions waiting for an answer. |
| `doum_unmanaged_gpu_processes` | gauge | | GPU processes outside the queue. |
| `doum_gpu_temperature_celsius` | gauge | | GPU temperature (only when `nvidia-smi` reports it). |
| `doum_gpu_power_watts` | gauge | | GPU power draw (only when `nvidia-smi` reports it). |
| `doum_soc_temperature_celsius` | gauge | | Hottest thermal zone in `/sys/class/thermal` (the SoC on a GB10). |

Per-job series carry the job id, so each job is its own series; on a busy machine that adds up over time.

A few useful queries for a Grafana dashboard:

```promql
doum_shards_used / doum_shards_total                     # GPU budget in use
doum_jobs{state="pending"}                               # queue length
max(doum_job_wait_seconds)                               # longest wait right now
sum by (name) (doum_job_energy_joules_total) / 3600      # energy per job name, Wh
increase(doum_job_requeues_total[1d])                    # pauses in the last day
```

The daemon also has a JSON history endpoint used by the panel and `doum top`: `GET /api/metrics?minutes=N` (default
60) returns one sample every 10 s for up to the last 24 hours (shards used, free memory, GPU and SoC temperature, GPU
power, running and waiting counts). It is kept in memory and starts empty after a daemon restart.

## Notifications

Configured in `[notify]` (see [Configuration](configuration.md#notify)). Every alert and question goes to all enabled
channels. Sending happens in the background; a broken channel never holds up the queue.

What triggers a message:

| Event | Level |
|---|---|
| A job is being paused (by you or to make room) | info |
| A paused job resumed | info |
| A background (detached) job finished | info |
| A background job failed (with its exit code and `doum logs <id>`) | warn |
| A GPU process outside the queue appeared (once per process) | warn |
| A question: the GPU is busy (`ask`), or a check-back | warn |

### Desktop

Through D-Bus (`notify-rust`). Questions are shown as critical notifications with one button per option (the
recommended one marked), and stay up until the question's deadline. Clicking a button answers the question. The
daemon needs access to your D-Bus session bus, which a systemd user service normally has.

### ntfy

Set `ntfy_url` to a topic URL, for example `https://ntfy.sh/doum-3f9c1a7e52b04d6e`, and subscribe to that topic in
the ntfy app. Doum POSTs the message text with the headers `Title`, `Priority` (`default` for info, `high` for warn)
and `Tags` (`question` or `gpu`).

On a public ntfy server, anyone who knows or guesses the topic name can read your messages (job names, what is
running). Use a long random topic, or your own ntfy server with access control. A phone cannot reach the daemon on
localhost, so a question on the phone includes the command to answer it at the machine
(`doum answer <id> <choice>`); no answer means the recommendation is applied.

### Webhooks

Each URL in `webhooks` gets a JSON POST:

```json
{
  "source": "doum",
  "title": "doum: 'tts-train' failed",
  "body": "exit code 1; see its log: doum logs 2",
  "level": "Warn",
  "decision": null
}
```

`level` is `"Info"` or `"Warn"`. For a question, `body` ends with the answer command and `decision` holds the
question:

```json
{
  "source": "doum",
  "title": "doum: the GPU is busy",
  "body": "'writer' needs 110 shards, 26 free. ...\n(no answer by the deadline → smart)\nAnswer: doum answer 4 <smart|wait|pause>",
  "level": "Warn",
  "decision": {
    "id": 4,
    "job_id": 3,
    "kind": "fit",
    "text": "'writer' needs 110 shards, 26 free. ...",
    "options": ["smart", "wait", "pause"],
    "recommended": "smart",
    "created_at": 1792540800,
    "deadline": 1792540860,
    "answer": null
  }
}
```

`kind` is `"fit"` (the GPU is busy) or `"check-back"` (options `pause-now`, `keep-waiting`, `cancel`). Times are Unix
seconds. A webhook cannot answer by itself; to answer from a script, POST `{"choice": "..."}` to
`/api/decisions/<id>` with the token (see [How it works](how-it-works.md#http-api)).
