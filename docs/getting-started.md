# Getting started

This guide takes you from install to running, pausing and resuming your own GPU jobs.

## Requirements

- Linux (x86_64 or aarch64). Doum reads `/proc` for memory and process information.
- An NVIDIA GPU with `nvidia-smi` on the `PATH` is optional but recommended. Without it, the queue still works (it
  only counts shards), but Doum cannot see GPU processes outside the queue and has no temperature, power or energy
  numbers.
- For desktop pop-ups: a desktop session with a notification daemon (D-Bus). Answer buttons need a notification
  daemon that supports actions (GNOME's does).
- `xdg-open` for `doum panel`, `tail` for `doum logs`.

Doum was built on an NVIDIA GB10 with 128 GB of unified memory. On a discrete-memory GPU, set `total_shards` to your
VRAM in GB and read the note on the memory check in [Configuration](configuration.md#memory).

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/MoeEldouma/doum/main/install.sh | sh
```

The script:

1. picks the latest GitHub release (or `DOUM_VERSION`, e.g. `DOUM_VERSION=v0.1.0`),
2. downloads `doum-<version>-<arch>-linux.tar.gz` and its `.sha256`, and refuses to install on a mismatch,
3. installs the binary as `~/.local/bin/doum`,
4. unless `DOUM_NO_SERVICE=1` is set (or `systemctl` is missing), installs `~/.config/systemd/user/doum.service` and
   runs `systemctl --user enable --now doum.service`.

The release archive also contains `deploy/`, `integrations/`, `claude/` and `docs/`, which you need for the
integrations.

### From source

```sh
git clone https://github.com/MoeEldouma/doum && cd doum
cargo build --release
install -m 0755 target/release/doum ~/.local/bin/doum
```

Keep the binary at `~/.local/bin/doum`: the systemd unit runs `%h/.local/bin/doum daemon`, and the ComfyUI plugin
looks for `~/.local/bin/doum` unless you set `DOUM_BIN`.

## The daemon

Everything goes through one daemon. It listens on `127.0.0.1:8420`, keeps its state in `~/.local/state/doum/`, and
reads its config from `~/.config/doum/doum.toml` once, when it starts. No config file is needed: every key has a
default (see [Configuration](configuration.md)).

### As a systemd user service (recommended)

```sh
mkdir -p ~/.config/systemd/user
cp deploy/systemd/doum.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now doum
systemctl --user status doum
journalctl --user -u doum -f        # the daemon's own log
```

`RUST_LOG=doum=info` in the unit sets the daemon's log level (`doum=debug` for more). Desktop notifications go over
the D-Bus session bus, which user services normally reach without extra settings, so the unit sets no display.

Config changes take effect only after `systemctl --user restart doum`. Restarting is safe while jobs run: the unit
uses `KillMode=process`, so systemd stops only the daemon, background jobs keep running, and the restarted daemon
picks them up again. (Foreground jobs run in their own `doum run` process and are not affected either.)

To start the service at boot even when you are not logged in: `loginctl enable-linger $USER`.

### In a terminal

```sh
doum daemon
```

Stop it with Ctrl-C.

### Check it

```sh
$ doum status
GPU  [░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░]  0/110 shards used · 112 GB memory free

RUNNING
  (nothing)

WAITING (in start order)
  (nothing)
```

If the daemon is not running, every command says so and tells you how to start it.

## Your first job

```sh
doum run -s 16 -- python generate.py --prompt "a doum palm at sunset"
```

- `-s 16` reserves 16 shards (about 16 GB). Ask for what the job really uses plus a little margin. The default is 8;
  values above `total_shards` are lowered to `total_shards`.
- Everything after `--` is the command. It runs as you, in your current directory, with your environment.
- Without `-d`, this is a **foreground** job: `doum run` waits in the queue, then runs the command right here, in
  your terminal, and exits with the command's exit code. Ctrl-C while waiting cancels the job.

All `doum run` options:

```
Usage: doum run [OPTIONS] <COMMAND>...

Options:
  -s, --shards <SHARDS>           GPU memory shards to reserve (~1 GB each). Use the total (e.g. 110) for the whole GPU [default: 8]
      --mem <MEM>                 Memory the job really uses (GB), for the free-memory check. Defaults from --shards
  -n, --name <NAME>
  -i, --interactive               Someone is waiting for this: schedule before batch work
  -m, --mode <MODE>               When the GPU is busy: smart | wait | pause | ask (default from config)
  -d, --detach                    Run in the background (daemon-managed, pausable). Without it, the command runs here, in the foreground
      --no-pause                  Never pause this job
      --no-hooks                  Skip the pre-start checks and hooks (the app runs its own)
      --covers-pid <COVERS_PIDS>  pid of a process this job stands for (e.g. a server holding a lease); repeatable
      --max-wait <MAX_WAIT>       Ask me after this long still waiting (e.g. 20m; 0 = never)
  -h, --help                      Print help
```

Notes on the less obvious ones:

- `--name` defaults to the file name of the command (`python` for `python generate.py`), so naming jobs pays off.
- `--mem` is only used by the free-memory check. If you leave it out, Doum uses `shards × gb_per_shard`, capped at
  60% of the whole budget.
- `--no-hooks` skips the free-memory check, the ComfyUI unload and your `pre_start` / `post_end` hooks.
- `--covers-pid` tells Doum that this job stands for another process (a server that holds the GPU on the job's
  behalf). That process is not reported as "outside the queue", and when every covered process has exited, the job is
  cancelled so its shards are not held forever. The ComfyUI plugin uses this.
- `--max-wait` overrides `max_wait_s` for this job (`20m`, `1h`, `90s`; `0` turns the check-back off).

## Foreground or detached

| | Foreground (`doum run ...`) | Detached (`doum run -d ...`) |
|---|---|---|
| Who runs the command | the `doum run` process, in your terminal | the daemon, in its own session and process group |
| Output | your terminal | a log file: `doum logs <id>` |
| Can be paused | never | yes, unless `--no-pause` |
| `doum run` returns | when the command ends, with its exit code | at once, printing the job id on stdout |
| If `doum run` dies | the job is cancelled (if waiting) or marked failed (if running) | not affected |
| Check-back after `max_wait_s` | yes | only in the interactive lane |
| Finished / failed alerts | no | yes |

Use foreground for things you watch (a quick test, a chat server that llama-swap starts). Use detached for long jobs
(training, render queues, batch generation), and make them pausable.

A detached job gets the environment of the shell that ran `doum run` (except variables starting with `DOUM_`), plus:
`DOUM_JOB_ID`, `DOUM_API`, `DOUM_TOKEN`, `DOUM_CONFIG` and `DOUM_STATE_DIR`, so `doum progress` inside the job finds
the right daemon. A foreground job gets `DOUM_JOB_ID`.

## The job lifecycle

```
            submit
              │
              ▼
   ┌──────► pending ──── cancel ───► cancelled
   │          │
   │          │ shards free, its turn
   │          ▼
   │       running ──── pre-start checks, then your command
   │          │   └────── exits 0 ──► done
   │          │   └────── exits ≠0 ─► failed   (a refused pre_start hook exits 97)
   │          │   └────── cancel ───► cancelled
   │          │ pause (detached only)
   │          ▼
   └────── pausing ──── SIGTERM, up to pause_grace_s, then SIGKILL
```

- **pending**: waiting for shards, for its turn, or for your answer. `doum status` says which, in plain words.
- **running**: holds its shards. The pre-start checks run first and count as running (the job already holds its
  shards while it waits for free memory).
- **pausing**: SIGTERM was sent; when the process has exited the job goes back to pending with `requeues + 1`.
- **done / failed / cancelled**: finished. Finished jobs stay visible (`doum status --all`) for 7 days.

## Modes: what happens when the GPU is busy

When a new job does not fit in the free shards, its mode decides what happens. The default is `default_mode`
(`ask` unless you change it); `-m` overrides it per job.

All four examples below start from the same situation: `total_shards = 110`, a detached render (60 shards, reported
`--eta 5m`) and a detached training run (24 shards, reported `--eta 2h`) are running, so 26 shards are free. You start
a writer that needs all 110.

### wait

Queue and pause nothing.

```sh
doum run -i -m wait -s 110 -n writer -- ./writer.sh
```

The writer waits until both the render (about 5 min) and the training (about 2 h) have finished. Nothing is
interrupted.

### pause

Pause whatever is in the way, now.

```sh
doum run -i -m pause -s 110 -n writer -- ./writer.sh
```

Both background jobs get SIGTERM. The writer starts once they have exited (at most `pause_grace_s`, 90 s by default).
Both go back in the queue and start again on their own when the writer has finished and their shards are free.

### smart

Let jobs that will finish within `smart_wait_s` (10 min by default) complete; pause the longer ones. A job that never
reported an ETA counts as long.

```sh
doum run -i -m smart -s 110 -n writer -- ./writer.sh
```

The training (2 h left) is paused, the render (5 min left) finishes, and the writer starts in about 5 minutes.

### ask

Ask you. Doum opens a question with the three choices above and time estimates for each:

- a desktop pop-up with **Smart / Wait / Pause now** buttons (Smart is marked "recommended"),
- a phone push and webhooks, if configured,
- the question in `doum status`, `doum top` and the panel,
- and in the `doum run` output, with the command to answer it:

```sh
doum answer 4 pause        # the question number is printed; question and job numbers share one counter
```

If nobody answers within `ask_timeout_s` (60 s), the recommendation (smart) is applied. While the question is open
the job is held, and it also holds up the jobs behind it.

The plan (who to pause, who to wait for) is made once: when the job is submitted, or when the question is answered.
If a job you let finish runs longer than it said, Doum does not re-plan by itself; the check-back below catches that.

### Check-back

An interactive-lane job, or any foreground job, that is still waiting after `max_wait_s` (20 min by default, or its
`--max-wait`) gets a second question: **pause-now**, **keep-waiting** or **cancel**. It stays open for at least 5
minutes; no answer means keep-waiting. If the job is still waiting another `max_wait_s` later, Doum asks again. Batch
jobs waiting in line are normal and never get this question.

## Writing a pausable job

A detached job can be paused at any time (unless it opted out). Pausing sends SIGTERM to the job's process group,
gives it `pause_grace_s` seconds (90 s) to save and exit, then sends SIGKILL. Later Doum runs the **same command again**
from the start: same directory, same environment, same `DOUM_JOB_ID`. The exit code of a paused attempt is ignored.

So a pausable job needs to:

1. save a checkpoint on SIGTERM, or often enough that losing the last stretch is acceptable,
2. exit within the grace period,
3. on start, look for its checkpoint and continue from it,
4. ideally, report progress with an ETA, so `smart` mode and the estimates can use it.

A minimal Python training loop:

```python
import os, signal, subprocess, sys, time, torch

CKPT = "checkpoint.pt"
EPOCHS = 10
stop = False

def on_sigterm(signum, frame):
    global stop
    stop = True            # finish the current step, then save and exit

signal.signal(signal.SIGTERM, on_sigterm)

model, opt = build_model()
start = 0
if os.path.exists(CKPT):
    state = torch.load(CKPT)
    model.load_state_dict(state["model"]); opt.load_state_dict(state["opt"])
    start = state["epoch"]

def save(epoch):
    torch.save({"model": model.state_dict(), "opt": opt.state_dict(), "epoch": epoch}, CKPT + ".tmp")
    os.replace(CKPT + ".tmp", CKPT)          # never leave a half-written checkpoint

for epoch in range(start, EPOCHS):
    t0 = time.time()
    for batch in loader:
        train_step(model, opt, batch)
        if stop:
            save(epoch)                       # resume this epoch from its start next time
            sys.exit(0)
    save(epoch + 1)
    left = (EPOCHS - epoch - 1) * (time.time() - t0)
    subprocess.run(["doum", "progress", f"epoch {epoch + 1}/{EPOCHS}", "--eta", f"{int(left)}s"])
```

Run it detached:

```sh
doum run -d -s 24 -n tts-train -- python train.py
```

Things to watch:

- The check between steps must come around more often than the grace period. If one step can take longer than 90 s,
  raise `pause_grace_s` or save on a timer.
- SIGTERM goes to every process in the command's process group. A shell script that runs `python train.py` passes it
  on to Python too; make sure the script itself does not exit before Python has saved (for example, `exec python ...`
  in the script).
- If the job truly cannot be interrupted, run it with `--no-pause`, or call `doum progress "..." --no-pause` from
  inside it once it reaches a point where it must not stop.

### `doum progress`

```
Usage: doum progress [OPTIONS] <MESSAGE>

Options:
      --eta <ETA>
      --no-pause   From now on, do not pause this job
```

It works only inside a job started by Doum (it reads `DOUM_JOB_ID`). `--eta` takes durations such as `90s`, `5m`,
`2h`, `1h 30m`. The ETA is counted down from the moment of the report, so a job that reports once an epoch still has a
sensible "time left" between reports. The message and time left show in `doum status`, `doum top`, the panel and in
`ask` questions.

## Managing jobs

```sh
doum status            # running, waiting (in start order), open questions, GPU use outside the queue
doum status --all      # also finished jobs (kept for 7 days)
doum status --json     # the daemon's full status, for scripts
doum logs 7            # last 200 lines of a detached job's log
doum logs -f 7         # follow it
doum pause 7           # pause a running detached job; it resumes on its own
doum cancel 7          # cancel a waiting or running job (running: SIGTERM)
doum up 7              # move a waiting job to the front of its lane
doum answer 12 smart   # answer question 12
```

- `doum logs` works only for detached jobs; logs are in `~/.local/state/doum/logs/<id>-<name>.log`. Lines starting
  with `[doum]` come from Doum itself (the pre-start checks, the start and the exit code).
- `doum pause` on a foreground job is refused ("foreground jobs cannot be paused (cancel instead)").
- `doum cancel` on a running background job sends SIGTERM, and SIGKILL if it is still running after
  `pause_grace_s` (90 s). The job is marked cancelled when it has exited. A running foreground job gets SIGTERM only
  (it is in your terminal, where Ctrl-C works too).
- `doum up` only works on waiting jobs and never moves a batch job ahead of the interactive lane.
- Answers: a "the GPU is busy" question takes `smart`, `wait` or `pause`; a check-back takes `pause-now`,
  `keep-waiting` or `cancel`.

### `doum top`

A live terminal view: the shard gauge, jobs (running, then waiting in start order), open questions, the last 30
minutes of shards used and SoC temperature, and processes outside the queue.

Keys: `↑`/`↓` (or `k`/`j`) select a job, `p` pause, `c` cancel (then `y` to confirm), `t` move up, `1` / `2` / `3`
answer the first open question with its first, second or third option, `q` or `Esc` quit.

### `doum panel`

Opens the web panel at `http://127.0.0.1:8420/` in your browser through a one-time login link, which sets a cookie
that unlocks the controls (pause, cancel, move up, answer). The panel shows questions, running and waiting jobs with
energy used, last-hour charts (shards, free memory, GPU and SoC temperature, GPU power), processes outside the queue
and finished jobs. Opening `http://127.0.0.1:8420/` directly shows the same information, but the buttons stay locked
until you have used `doum panel` once in that browser.

## Next

- [Configuration](configuration.md): change the shard budget, the default mode, timings, hooks, notifications.
- [Integrations](integrations.md): ComfyUI, llama-swap, Claude Code, Prometheus.
- [How it works](how-it-works.md): the exact scheduling rules and the security model.
