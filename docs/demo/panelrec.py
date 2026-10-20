"""Record the doum web panel during a scripted multi-job scenario (stand-in CPU jobs) as PNG frames."""
import os, subprocess, sys, threading, time
from playwright.sync_api import sync_playwright

here = os.path.dirname(os.path.abspath(__file__))
state, frames, port = sys.argv[1], sys.argv[2], 8497
env = dict(os.environ, DOUM_CONFIG=f"{here}/panel.toml", DOUM_STATE_DIR=state, PATH=f"{here}/bin:" + os.environ["PATH"])
os.makedirs(frames, exist_ok=True)

def doum(*a):
    subprocess.run(["doum", *a], cwd=here, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

def scenario():
    doum("run", "-d", "-s", "40", "-n", "comfy-render", "--", "./work.sh", "8", "7", "clip")
    time.sleep(2); doum("run", "-d", "-s", "20", "-n", "sdxl-batch", "--", "./work.sh", "6", "7", "image")
    time.sleep(2); doum("run", "-d", "-s", "24", "-n", "tts-train", "--", "./train2.sh")
    time.sleep(8); doum("run", "-d", "-s", "8", "-n", "agent-eval", "--", "./work.sh", "5", "5", "case")
    time.sleep(14); doum("run", "-d", "-i", "-m", "ask", "-s", "110", "-n", "llm-writer", "--", "./work.sh", "5", "5", "section")
    time.sleep(70); doum("run", "-d", "-s", "40", "-n", "comfy-render-2", "--", "./work.sh", "6", "6", "clip")
    time.sleep(3); doum("run", "-d", "-s", "16", "-n", "upscale", "--", "./work.sh", "5", "6", "frame")

token = open(os.path.join(state, "token")).read().strip()
with sync_playwright() as p:
    b = p.chromium.launch()
    pg = b.new_page(viewport={"width": 1400, "height": 1000}, device_scale_factor=1)
    pg.goto(f"http://127.0.0.1:{port}/login?token={token}")
    pg.wait_for_timeout(1500)
    threading.Thread(target=scenario, daemon=True).start()
    t0, k, clicked = time.time(), 0, False
    while time.time() - t0 < 150:
        pg.screenshot(path=f"{frames}/f{k:04d}.png"); k += 1
        if not clicked and pg.locator("#questions button.rec").count():
            if time.time() - getattr(pg, "_qseen", time.time()) > 4:
                pg.locator("#questions button.rec").first.hover(); pg.wait_for_timeout(500)
                pg.locator("#questions button.rec").first.click(); clicked = True
            pg._qseen = getattr(pg, "_qseen", time.time())
        time.sleep(max(0, t0 + k * 0.75 - time.time()))
    b.close()
print(k, "frames")
