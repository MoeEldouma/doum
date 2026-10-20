"""Drive a shell in a pseudo-terminal through the demo and write an asciicast v2 file (asciinema format)."""
import fcntl, json, os, pty, select, struct, sys, termios, threading, time

COLS, ROWS = 104, 26
out_path, state = sys.argv[1], sys.argv[2]
here = os.path.dirname(os.path.abspath(__file__))
env = dict(os.environ, TERM="xterm-256color", PS1="\\[\\e[1;32m\\]$\\[\\e[0m\\] ", DOUM_CONFIG=f"{here}/doum.toml",
           DOUM_STATE_DIR=state, PATH=f"{here}/bin:" + os.environ["PATH"], HISTFILE="/dev/null")
pid, fd = pty.fork()
if pid == 0:
    os.chdir(here)
    os.execvpe("bash", ["bash", "--norc", "--noprofile", "-i"], env)
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
events, t0, done = [], time.time(), False

def reader():
    while not done:
        r, _, _ = select.select([fd], [], [], 0.1)
        if r:
            try:
                data = os.read(fd, 65536)
            except OSError:
                return
            events.append([round(time.time() - t0, 4), "o", data.decode("utf-8", "replace")])

threading.Thread(target=reader, daemon=True).start()

def typ(cmd, after=1.2):
    for ch in cmd:
        os.write(fd, ch.encode()); time.sleep(0.035)
    time.sleep(0.4); os.write(fd, b"\r"); time.sleep(after)

def key(k, after=0.8):
    os.write(fd, k.encode()); time.sleep(after)

time.sleep(0.8); key("clear\r", 0.5); events.clear(); t0 = time.time()
key("", 0.3); os.write(fd, b"\r"); time.sleep(0.6)
typ("# one GPU, three jobs: a ComfyUI render, a training run, and an LLM someone is waiting for", 0.8)
typ("doum run -d -s 60 -n comfy-render -- ./render.sh", 0.8)
typ("doum run -d -s 24 -n tts-train -- ./train.sh", 1.5)
typ("doum status", 2.5)
typ("doum run -d -i -s 110 -n llm-writer -- ./writer.sh", 2.5)
typ("doum top", 29)
key("q", 1.0)
typ("doum logs 2", 3.0)
done = True
with open(out_path, "w") as f:
    f.write(json.dumps({"version": 2, "width": COLS, "height": ROWS, "timestamp": int(t0),
                        "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"}, "title": "doum"}) + "\n")
    for e in events:
        f.write(json.dumps(e) + "\n")
os.kill(pid, 9)
print(len(events), "events,", round(events[-1][0], 1), "s")
