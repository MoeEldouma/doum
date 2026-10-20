"""doum gate for ComfyUI: every prompt waits for its turn in the doum GPU queue.

- Before a prompt runs, ComfyUI holds a doum "lease" (a background doum job of DOUM_COMFY_SHARDS shards, default 60).
  If the GPU is busy, the prompt waits (doum may ask the user: smart / wait / pause).
- While prompts run, progress and an ETA are reported to doum, so its smart decisions know how long ComfyUI needs.
- If doum pauses the lease (something urgent needs the GPU), the running prompt is interrupted, models are unloaded,
  and that prompt is put back at the FRONT of ComfyUI's queue; the rest of the queue stays as it is. When the lease is
  granted again, ComfyUI carries on.
- After DOUM_COMFY_IDLE_S seconds (default 90) with an empty queue, models are unloaded and the lease is released.
- If doum is not installed or not running, ComfyUI works exactly as before (never blocked by doum).

Install: link or copy this folder into ComfyUI/custom_nodes/ and restart ComfyUI. Adds no nodes.
"""
import json
import os
import subprocess
import threading
import time
import urllib.request

DOUM = os.environ.get("DOUM_BIN") or os.path.expanduser("~/.local/bin/doum")
API = os.environ.get("DOUM_API", "http://127.0.0.1:8420")
STATE_DIR = os.path.expanduser(os.environ.get("DOUM_STATE_DIR", "~/.local/state/doum"))
TOKEN_FILE = os.path.expanduser(os.environ.get("DOUM_TOKEN_FILE", os.path.join(STATE_DIR, "token")))
SHARDS = int(os.environ.get("DOUM_COMFY_SHARDS", "60"))
IDLE_S = int(os.environ.get("DOUM_COMFY_IDLE_S", "90"))
COMFY_URL = os.environ.get("DOUM_COMFY_URL", "http://127.0.0.1:8188")
TAG = "[doum]"
# Handshake: when doum pauses or ends the lease, its holder process waits for this file, written once ComfyUI has
# really stopped rendering and unloaded its models, so the GPU is only handed over when it is actually free.
FLAG_DIR = os.path.join(STATE_DIR, "comfyui")
STOPPED = os.path.join(FLAG_DIR, f"{os.getpid()}.stopped")

_lock = threading.Lock()
_state = {
    "lease": None,  # doum job id
    "executing": False,
    "lost": False,  # lease taken away while a prompt was running
    "last_activity": time.time(),
    "started": 0.0,
    "durations": [],
    "prompt": "",
    "disabled": False,
}


def _log(msg):
    print(f"{TAG} {msg}", flush=True)


def _token():
    try:
        with open(TOKEN_FILE) as f:
            return f.read().strip()
    except OSError:
        return ""


def _api(method, path, body=None, timeout=5):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(API + path, data=data, method=method, headers={"content-type": "application/json", "x-doum-token": _token()})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.loads(r.read() or b"null")


def _job(jid):
    try:
        return _api("GET", f"/api/jobs/{jid}")
    except Exception:
        return None


def _doum_up():
    try:
        _api("GET", "/api/status", timeout=2)
        return True
    except Exception:
        return False


def _signal_stopped():
    try:
        os.makedirs(FLAG_DIR, exist_ok=True)
        open(STOPPED, "w").close()
    except OSError:
        pass


def _submit():
    try:
        os.remove(STOPPED)
    except OSError:
        pass
    holder = (
        'trap \'for i in $(seq 1 1200); do [ -e "$COMFY_DOUM_STOPPED" ] && break; sleep 0.1; done; '
        'rm -f "$COMFY_DOUM_STOPPED"; exit 0\' TERM INT; while :; do sleep 3600 & wait $!; done'
    )
    os.environ["COMFY_DOUM_STOPPED"] = STOPPED
    cmd = [
        DOUM, "run", "--detach", "--shards", str(SHARDS), "--mem", "0", "--no-hooks",
        "--name", "comfyui", "--covers-pid", str(os.getpid()),
        "--", "bash", "-c", holder,
    ]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=30)
    if out.returncode != 0:
        raise RuntimeError(out.stderr.strip() or "doum run failed")
    return int(out.stdout.strip().splitlines()[-1])


def _acquire(what):
    """Block until ComfyUI holds a running lease. Returns False if doum is unavailable (then run unmanaged)."""
    if _state["disabled"]:
        return False
    if not _doum_up():
        _log("doum daemon not reachable: running this prompt without the queue")
        return False
    with _lock:
        j = _job(_state["lease"]) if _state["lease"] else None
        if j is None or j.get("state") in ("done", "failed", "cancelled"):
            try:
                _state["lease"] = _submit()
                _log(f"asked the GPU queue for {SHARDS} shards (job {_state['lease']}) for {what}")
            except Exception as e:
                _log(f"could not queue ({e!r}): running this prompt without the queue")
                return False
        jid = _state["lease"]
    last_note = 0
    while True:
        j = _job(jid)
        if j is None:
            _log("lost contact with doum: running this prompt without the queue")
            return False
        st = j.get("state")
        if st == "running":
            if last_note:
                _log(f"GPU granted (job {jid})")
            return True
        if st in ("done", "failed", "cancelled"):
            with _lock:
                _state["lease"] = None
            return _acquire(what)
        if time.time() - last_note > 30:
            _log(f"waiting for the GPU: {j.get('reason', st)}")
            last_note = time.time()
        time.sleep(1)


def _unload():
    try:
        import comfy.model_management as mm

        mm.unload_all_models()
        mm.soft_empty_cache(True)
    except Exception as e:
        _log(f"unload failed: {e!r}")


def _release(why):
    with _lock:
        jid, _state["lease"] = _state["lease"], None
    if jid is None:
        return
    _unload()
    _signal_stopped()
    try:
        _api("POST", f"/api/jobs/{jid}/cancel", {})
    except Exception:
        pass
    _log(f"released the GPU ({why})")


def _remaining():
    try:
        import server

        return server.PromptServer.instance.prompt_queue.get_tasks_remaining()
    except Exception:
        return 0


def _watch():
    """Every 2 s: report progress, notice a lost lease, release when idle."""
    while True:
        time.sleep(2)
        try:
            jid = _state["lease"]
            if jid is None:
                continue
            if _state["executing"]:
                j = _job(jid)
                if j is not None and j.get("state") != "running":
                    if not _state["lost"]:
                        _state["lost"] = True
                        _log("doum paused ComfyUI to make room: stopping this prompt; it will be queued again first")
                        import comfy.model_management as mm

                        mm.interrupt_current_processing(True)
                    continue
                durs = _state["durations"][-5:]
                avg = sum(durs) / len(durs) if durs else None
                left = _remaining()
                elapsed = time.time() - _state["started"]
                eta = int(max(0.0, avg - elapsed) + avg * max(0, left - 1)) if avg else None
                msg = f"rendering ({max(0, left - 1)} more queued)"
                _api("POST", f"/api/jobs/{jid}/progress", {"message": msg, "eta_s": eta})
            else:
                j = _job(jid)
                if j is not None and j.get("state") != "running" and _remaining() == 0:
                    _release("paused while idle")  # unload, confirm, and don't keep a requeued lease with nothing to do
                elif time.time() - _state["last_activity"] > IDLE_S and _remaining() == 0:
                    _release(f"idle {IDLE_S}s")
                elif j is not None and j.get("state") == "running":
                    left = max(0, int(IDLE_S - (time.time() - _state["last_activity"])))
                    _api("POST", f"/api/jobs/{jid}/progress", {"message": f"idle, releasing the GPU in {left}s", "eta_s": left})
        except Exception:
            pass


def _requeue_front(prompt, extra_data):
    def go():
        time.sleep(1)
        body = {"prompt": prompt, "front": True}
        if extra_data:
            body["extra_data"] = extra_data
            if extra_data.get("client_id"):
                body["client_id"] = extra_data["client_id"]
        try:
            req = urllib.request.Request(COMFY_URL + "/prompt", data=json.dumps(body).encode(), headers={"content-type": "application/json"})
            urllib.request.urlopen(req, timeout=10).read()
            _log("interrupted prompt queued again at the front")
        except Exception as e:
            _log(f"could not re-queue the interrupted prompt: {e!r}")

    threading.Thread(target=go, daemon=True).start()


def _install():
    import execution

    inner = execution.PromptExecutor.execute
    if getattr(inner, "_doum_gate", False):
        return

    def execute(self, prompt, prompt_id, *args, **kwargs):
        what = f"prompt {str(prompt_id)[:8]}"
        managed = _acquire(what)
        _state.update(executing=True, lost=False, started=time.time(), prompt=what)
        try:
            return inner(self, prompt, prompt_id, *args, **kwargs)
        finally:
            dur = time.time() - _state["started"]
            lost = _state["lost"]
            _state.update(executing=False, lost=False, last_activity=time.time())
            if managed and not lost:
                _state["durations"].append(dur)
            if lost:
                _unload()
                _signal_stopped()  # now the lease holder may exit and doum can hand the GPU over
                extra = args[0] if args else kwargs.get("extra_data", {})
                _requeue_front(prompt, extra or {})

    execute._doum_gate = True
    execution.PromptExecutor.execute = execute
    threading.Thread(target=_watch, daemon=True).start()
    _log(f"gate active: prompts wait for {SHARDS} shards in the doum GPU queue; GPU released after {IDLE_S}s idle")


def _install_when_ready():
    # Wait until the ComfyUI server answers: by then every custom node has loaded, so this wrapper is the outermost.
    for _ in range(600):
        try:
            urllib.request.urlopen(COMFY_URL + "/system_stats", timeout=2).read()
            break
        except Exception:
            time.sleep(1)
    try:
        _install()
    except Exception as e:
        _log(f"NOT ACTIVE: {e!r}")


if os.path.exists(DOUM):
    threading.Thread(target=_install_when_ready, daemon=True).start()
else:
    print(f"{TAG} not installed ({DOUM} missing): ComfyUI runs without the queue", flush=True)

NODE_CLASS_MAPPINGS = {}
NODE_DISPLAY_NAME_MAPPINGS = {}
