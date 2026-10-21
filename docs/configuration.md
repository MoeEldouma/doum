# Configuration

Doum reads one TOML file: `~/.config/doum/doum.toml` (more precisely `$XDG_CONFIG_HOME/doum/doum.toml`), or the path
in `DOUM_CONFIG`. The file is optional and every key in it is optional: a missing key keeps its default. An empty file
and no file at all give the same result.

- The daemon reads the file once, at start. After a change, restart it: `systemctl --user restart doum`. Running
  background jobs keep running through the restart (see [restarts](how-it-works.md#restarts)).
- The CLI and the MCP server read the same file too (for `listen` and, in the MCP server, the `[mcp]` limits).
- Unknown keys are ignored without a warning, so check the spelling. A file that is not valid TOML, or a value of the
  wrong type, makes every Doum command fail with a parse error that names the file.
- A list setting (for example `claude_hook.gpu_patterns`) replaces the default list; it is not merged with it.

A commented example with every key is in [`config.example.toml`](config.example.toml).

## Top-level keys

| Key | Type | Default | Meaning |
|---|---|---|---|
| `total_shards` | integer | `110` | The GPU memory budget, in shards. Jobs asking for more are lowered to this. 110 suits a 128 GB GB10 (about 110 GB of the roughly 121 GB unified pool, leaving room for the OS and desktop). On a discrete GPU, use your VRAM in GB minus a little. |
| `gb_per_shard` | float | `1.0` | Roughly how many GB one shard is. Used only to turn shards into GB for the free-memory check. |
| `listen` | string | `"127.0.0.1:8420"` | Address of the daemon's API and web panel, as `IP:port`. The CLI, `doum mcp` and jobs connect to it. It must be a loopback address (`127.0.0.1:<port>` or `[::1]:<port>`); the daemon refuses to start otherwise. A host name such as `localhost:8420` is not accepted. See [security](how-it-works.md#security-model). |
| `default_mode` | string | `"ask"` | What a job does when it does not fit and was given no `--mode`: `smart`, `wait`, `pause` or `ask`. |
| `smart_wait_s` | integer (s) | `600` | Smart mode: running jobs that will finish within this many seconds (by their own `doum progress --eta`) are waited for; longer ones, and ones that never reported an ETA, are paused. |
| `max_wait_s` | integer (s) | `1200` | Check-back: ask the user after an interactive or foreground job has waited this long. `0` turns it off. Per job: `doum run --max-wait`. |
| `ask_timeout_s` | integer (s) | `60` | How long an `ask` question stays open before the recommendation (smart) is applied. A check-back question stays open for this or 300 s, whichever is longer. |
| `pause_grace_s` | integer (s) | `90` | After SIGTERM, how long a paused job gets to save and exit before SIGKILL. |
| `unmanaged_min_gb` | float | `1.0` | GPU processes outside the queue that use less than this (GB) are not reported (driver contexts, idle servers). |

## `[memory]`

The free-memory check runs inside the job just before its command starts (it is one of the pre-start checks, skipped
with `--no-hooks`).

| Key | Type | Default | Meaning |
|---|---|---|---|
| `check` | bool | `true` | Wait until there is free memory for the job plus `headroom_gb`: the GPU's own free memory when the card reports it, otherwise `MemAvailable` (from `/proc/meminfo`). |
| `headroom_gb` | float | `6.0` | Extra GB that must stay free on top of the job's memory. |

The job's memory is `--mem` if given, otherwise `shards × gb_per_shard`, capped at 60% of
`total_shards × gb_per_shard`. A job with `--mem 0` skips the check. While waiting, the job already holds its
shards and writes `[doum] WAIT for memory: ...` to its log once a minute. There is no timeout: it waits until the
memory is there or the job is cancelled.

Which free memory counts depends on the machine. A card with its own memory reports it to `nvidia-smi`
(`memory.free`), and the check uses that. On unified-memory machines (GB10, DGX Spark, Jetson-style SoCs) `nvidia-smi`
reports `[N/A]`, and `MemAvailable` is the real headroom for GPU work, so the check uses that. The discrete-GPU path
is untested so far; reports are welcome.

## `[hooks]`

Your own commands, run inside the job (for detached jobs: in the job's directory and environment, output in its log),
around the job's command. Each is a list: the program, then its arguments. No shell is involved unless you call one.
In every argument, `{name}` is replaced by the job's name and `{id}` by its id.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `pre_start` | list of strings | `[]` | Runs after the memory check, before the command. It may block as long as it likes (a thermal cool-down gate, for example). If it exits non-zero, or cannot be started, the command does not run and the job fails with exit code 97. |
| `post_end` | list of strings | `[]` | Runs after the command has ended (also after a refused `pre_start`). Its exit code is ignored. |

Jobs started with `--no-hooks` (for example ComfyUI leases) skip both hooks and the memory check.

Example, a cool-down gate script that blocks until the GPU is cool enough:

```toml
[hooks]
pre_start = ["/home/me/bin/gpu-cooldown-gate", "--job", "doum:{name}"]
post_end  = ["/home/me/bin/gpu-cooldown-gate", "--finished"]
```

## `[notify]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `desktop` | bool | `true` | Desktop notifications through D-Bus. Questions get answer buttons and stay up until their deadline. |
| `ntfy_url` | string | `""` (off) | An ntfy topic URL for phone push, e.g. `"https://ntfy.sh/<long-random-topic>"`. Anyone who knows a topic name on a public ntfy server can read it, so make it long and random. |
| `webhooks` | list of strings | `[]` | URLs that get a JSON POST for every alert and question. |

What is sent and the webhook payload are described in [Integrations: notifications](integrations.md#notifications).

## `[comfyui]`

| Key | Type | Default | Meaning |
|---|---|---|---|
| `url` | string | `"http://127.0.0.1:8188"` | ComfyUI's base URL. Empty disables the unload below. |
| `free_when_idle` | bool | `true` | During a job's memory check, if there is not enough free memory and ComfyUI's queue is empty (nothing running, nothing pending), ask ComfyUI to unload its cached models (`POST /free`). Tried once per job start. |

This is separate from the ComfyUI gate plugin, which has its own settings (environment variables, see
[Integrations](integrations.md#comfyui)).

## `[mcp]`

Limits for AI agents using `doum mcp`. They are enforced by the MCP server, not by the daemon.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `max_shards` | integer | `60` | Largest job an agent may submit. |
| `max_jobs` | integer | `2` | How many unfinished (waiting or running) agent jobs may exist at once, counted over all agents. |
| `mode` | string | `"wait"` | The mode for agent jobs when the GPU is busy. `wait` never pauses anyone else's work. |
| `allow_control_others` | bool | `false` | Let agents pause and cancel jobs they did not start. |
| `allow_answer` | bool | `false` | Let agents answer the user's GPU questions. |

## `[claude_hook]`

Patterns for `doum claude-hook`, the Claude Code PreToolUse hook.

| Key | Type | Default | Meaning |
|---|---|---|---|
| `gpu_patterns` | list of strings | see below | Case-insensitive regular expressions (Rust `regex` syntax). A Bash command that matches one of them is blocked unless it goes through Doum. Patterns that do not compile are skipped. Setting this replaces the whole default list. |
| `hint` | string | `""` | Extra text added to the hook's message when it blocks a command, for example where local LLMs are served on your machine. Empty adds nothing. |

The defaults:

```toml
[claude_hook]
gpu_patterns = [
  '\bollama\s+(run|serve)\b',
  '\bllama-(server|cli|bench)\b',
  '\bvllm\s+serve\b',
  '\bsglang\.launch_server\b',
  '\bbasert\s+(bench|serve|chat|run|complete)\b',
  '\btorchrun\b',
  '\baccelerate\s+launch\b',
  '\bpython[0-9.]*\s+[^|;&]*\b[\w-]*(train|finetune|fine_tune|infer|inference|generate|render)[\w-]*\.py\b',
]
```

## Environment variables

Read by the `doum` binary:

| Variable | Meaning |
|---|---|
| `DOUM_CONFIG` | Path of the config file, instead of `~/.config/doum/doum.toml`. |
| `DOUM_STATE_DIR` | State directory, instead of `~/.local/state/doum` (`$XDG_STATE_HOME/doum`). Holds `state.json`, `token`, `logs/` and `exit/`. Created if missing, and set to mode 0700 by every Doum command. |
| `DOUM_API` | Base URL of the daemon for the CLI and MCP server (e.g. `http://127.0.0.1:8420`), instead of `http://<listen>`. |
| `DOUM_TOKEN` | API token for the CLI and MCP server, instead of reading `<state dir>/token`. |
| `DOUM_JOB_ID` | Set by Doum inside every job. `doum progress` requires it. |
| `RUST_LOG` | Log filter for `doum daemon` (default `doum=info`). |

Doum sets `DOUM_JOB_ID`, `DOUM_API`, `DOUM_TOKEN`, `DOUM_CONFIG` and `DOUM_STATE_DIR` in every detached job, and strips
any `DOUM_*` variables from the environment it captures when you run `doum run`.

Read by the ComfyUI plugin (in ComfyUI's environment): `DOUM_BIN`, `DOUM_API`, `DOUM_STATE_DIR`, `DOUM_TOKEN_FILE`,
`DOUM_COMFY_SHARDS`, `DOUM_COMFY_IDLE_S`, `DOUM_COMFY_URL`. See [Integrations](integrations.md#comfyui).

## Files

| Path | What |
|---|---|
| `~/.config/doum/doum.toml` | This config. |
| `~/.local/state/doum/` | The state directory, mode 0700 (set on every start). |
| `~/.local/state/doum/state.json` | Jobs and open questions, including each job's environment (needed to restart paused jobs; never returned by the API). Mode 0600, written atomically after every change. Finished jobs older than 7 days are dropped. |
| `~/.local/state/doum/token` | The API token (48 hex characters, mode 0600), created by the daemon on first start. |
| `~/.local/state/doum/logs/<id>-<name>.log` | Output of detached jobs. Removed when Doum forgets the job (7 days after it finished). |
| `~/.local/state/doum/exit/<id>.<attempt>` | Exit code of each attempt, written by the job runner; `<id>.<attempt>.pgid` holds the command's process group. Removed once the job has finished and its exit code is recorded. |
| `~/.local/state/doum/comfyui/` | Stop-handshake flag files of the ComfyUI plugin (under `DOUM_STATE_DIR` if ComfyUI's environment sets it). |

## Complete example

```toml
# ~/.config/doum/doum.toml
total_shards = 110
gb_per_shard = 1.0
listen = "127.0.0.1:8420"
default_mode = "smart"       # no pop-ups: wait for short jobs, pause long ones
smart_wait_s = 900           # "short" means within 15 minutes
max_wait_s = 1800            # check back after 30 minutes
ask_timeout_s = 60
pause_grace_s = 120          # my training saves slowly
unmanaged_min_gb = 2.0

[memory]
check = true
headroom_gb = 8

[hooks]
pre_start = ["/home/me/bin/gpu-cooldown-gate", "--job", "doum:{name}"]
post_end = []

[notify]
desktop = true
ntfy_url = "https://ntfy.sh/doum-3f9c1a7e52b04d6e"
webhooks = ["http://127.0.0.1:9000/hooks/doum"]

[comfyui]
url = "http://127.0.0.1:8188"
free_when_idle = true

[mcp]
max_shards = 40
max_jobs = 1
mode = "wait"
allow_control_others = false
allow_answer = false

[claude_hook]
hint = "Local LLMs load on demand at http://127.0.0.1:8410/v1 (llama-swap); no need to start a server."
```
