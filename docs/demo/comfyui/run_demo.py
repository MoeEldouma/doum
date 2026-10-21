"""Live demo of Doum pausing ComfyUI: three Flux renders A, B, C are queued; while A renders, an urgent job needs
most of the GPU, Doum pauses ComfyUI, A is put back at the front of ComfyUI's queue, the urgent job runs, then A, B, C
finish in order. Screenshots of the Doum panel and ComfyUI are taken along the way."""
import json, os, subprocess, sys, time, urllib.request
from playwright.sync_api import sync_playwright

OUT = os.path.dirname(os.path.abspath(__file__))
COMFY = "http://127.0.0.1:8188"
DOUM = os.path.expanduser("~/.local/bin/doum")
CLIENT = "doum-demo"
log_lines = []


def log(msg):
    line = f"{time.strftime('%H:%M:%S')}  {msg}"
    print(line, flush=True)
    log_lines.append(line)


def get(path):
    return json.load(urllib.request.urlopen(COMFY + path, timeout=10))


def workflow(name, seed):
    return {
        "1": {"class_type": "CheckpointLoaderSimple", "inputs": {"ckpt_name": "flux1-schnell-fp8.safetensors"}},
        "2": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["1", 1], "text": {
            "A": "a doum palm by the Nile at golden hour, photographic",
            "B": "a quiet workstation with a glowing GPU, cinematic light",
            "C": "a desert caravan resting in the shade of palms, painterly"}[name]}},
        "3": {"class_type": "CLIPTextEncode", "inputs": {"clip": ["1", 1], "text": ""}},
        "4": {"class_type": "EmptySD3LatentImage", "inputs": {"width": 1024, "height": 1024, "batch_size": 1}},
        "5": {"class_type": "KSampler", "inputs": {"model": ["1", 0], "positive": ["2", 0], "negative": ["3", 0],
              "latent_image": ["4", 0], "seed": seed, "steps": 28, "cfg": 1.0, "sampler_name": "euler",
              "scheduler": "simple", "denoise": 1.0}},
        "6": {"class_type": "VAEDecode", "inputs": {"samples": ["5", 0], "vae": ["1", 2]}},
        "7": {"class_type": "SaveImage", "inputs": {"images": ["6", 0], "filename_prefix": f"doum_demo/{name}"}},
    }


def queue_prompt(name, seed):
    body = json.dumps({"prompt": workflow(name, seed), "client_id": CLIENT}).encode()
    r = json.load(urllib.request.urlopen(urllib.request.Request(COMFY + "/prompt", data=body, headers={"content-type": "application/json"}), timeout=10))
    return r["prompt_id"]


def queue_names(ids):
    q = get("/queue")
    def label(item):
        pid = item[1]
        prompt = item[2]
        prefix = prompt.get("7", {}).get("inputs", {}).get("filename_prefix", "")
        return prefix.split("/")[-1] + ("" if pid in ids.values() else "'")
    return [label(i) for i in q["queue_running"]], [label(i) for i in sorted(q["queue_pending"], key=lambda x: x[0])]


def shot(page, url, path, wait=2500, setup=None):
    page.goto(url, wait_until="domcontentloaded")
    page.wait_for_timeout(wait)
    if setup:
        setup(page)
    page.screenshot(path=os.path.join(OUT, path))


def snapshot(ids, title):
    running, pending = queue_names(ids)
    return {"title": title, "running": running, "pending": pending, "t": time.strftime("%H:%M:%S")}


def doum_job(name):
    s = json.loads(subprocess.run([DOUM, "status", "--json"], capture_output=True, text=True).stdout)
    js = [j for j in s["jobs"] if j["name"] == name]
    return max(js, key=lambda j: j["id"]) if js else None


def main():
    token = open(os.path.expanduser("~/.local/state/doum/token")).read().strip()
    with sync_playwright() as p:
        b = p.chromium.launch()
        panel = b.new_page(viewport={"width": 1440, "height": 1000})
        panel.goto(f"http://127.0.0.1:8420/login?token={token}")
        comfy = b.new_page(viewport={"width": 1600, "height": 950})

        ids = {}
        for n, seed in [("A", 11), ("B", 22), ("C", 33)]:
            ids[n] = queue_prompt(n, seed)
        log(f"queued A, B, C in ComfyUI: {ids}")

        # wait until A is rendering (past the first steps)
        t0 = time.time()
        while time.time() - t0 < 600:
            running, pending = queue_names(ids)
            if running == ["A"]:
                break
            time.sleep(1)
        time.sleep(12)
        running, pending = queue_names(ids)
        log(f"1. rendering: {running}, waiting: {pending}")
        cards = [snapshot(ids, "1. A is rendering; B and C wait")]
        shot(panel, "http://127.0.0.1:8420/", "1-doum-panel-rendering.png")
        shot(comfy, COMFY, "1-comfyui-rendering.png", 5000)

        log("2. an urgent job needs 80 of 110 shards: doum run -i -m pause -s 80 -n urgent-llm -- sleep 25")
        subprocess.Popen([DOUM, "run", "-d", "-i", "-m", "pause", "-s", "80", "-n", "urgent-llm", "--", "sleep", "25"],
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        # the moment the urgent job is running, ComfyUI has been paused and A is back in line first
        t0 = time.time()
        while time.time() - t0 < 120:
            j = doum_job("urgent-llm")
            if j and j["state"] == "running":
                break
            time.sleep(0.5)
        running, pending = queue_names(ids)
        log(f"3. urgent job running; ComfyUI paused. ComfyUI running: {running} (waiting at Doum's gate), queue: {pending}")
        cards.append(snapshot(ids, "2. Paused: the urgent job has the GPU; A (re-queued as A') is first in line, waiting at Doum's gate"))
        time.sleep(2)
        shot(panel, "http://127.0.0.1:8420/", "2-doum-panel-paused.png")

        # the urgent job runs, then ComfyUI resumes
        t0 = time.time()
        seen = None
        while time.time() - t0 < 900:
            running, pending = queue_names(ids)
            state = (tuple(running), tuple(pending))
            if state != seen:
                log(f"   running: {running}, queue: {pending}")
                seen = state
            if not running and not pending:
                break
            time.sleep(2)
        time.sleep(3)
        shot(panel, "http://127.0.0.1:8420/", "3-doum-panel-done.png")

        # the order ComfyUI actually ran things, from its history
        hist = get("/history?max_items=20")
        rows = []
        for pid, h in hist.items():
            prefix = h["prompt"][2].get("7", {}).get("inputs", {}).get("filename_prefix", "")
            if not prefix.startswith("doum_demo/"):
                continue
            start = next((m[1]["timestamp"] for m in h["status"]["messages"] if m[0] == "execution_start"), 0)
            status = h["status"].get("status_str")
            interrupted = any(m[0] == "execution_interrupted" for m in h["status"]["messages"])
            rows.append((start, prefix.split("/")[-1] + ("" if pid in ids.values() else "'"), "interrupted" if interrupted else status))
        rows.sort()
        log("4. ComfyUI ran them in this order: " + "  ->  ".join(f"{n} ({s})" for _, n, s in rows))
        json.dump({"order": [(n, s) for _, n, s in rows], "cards": cards, "log": log_lines,
                   "starts": [(time.strftime("%H:%M:%S", time.localtime(t / 1000)), n, s) for t, n, s in rows]},
                  open(os.path.join(OUT, "result.json"), "w"), indent=1)
        b.close()


if __name__ == "__main__":
    main()
