"""Replays the ComfyUI pause race against a queue that behaves like ComfyUI's PromptQueue (heap of
(number, prompt_id, prompt, extra_data, outputs, sensitive), front = negative number, currently_running copy)."""
import copy, heapq, importlib.util, sys, threading, types

class PromptQueue:  # same put/get semantics as ComfyUI's execution.PromptQueue
    def __init__(self):
        self.mutex = threading.RLock(); self.queue = []; self.currently_running = {}; self.task_counter = 0
    def put(self, item):
        with self.mutex: heapq.heappush(self.queue, item)
    def get(self):
        with self.mutex:
            item = heapq.heappop(self.queue); i = self.task_counter
            self.currently_running[i] = copy.deepcopy(item); self.task_counter += 1
            return item, i
    def task_done(self, i):
        with self.mutex: self.currently_running.pop(i)

srv = types.SimpleNamespace(number=0, prompt_queue=PromptQueue())
sys.modules["server"] = types.SimpleNamespace(PromptServer=types.SimpleNamespace(instance=srv))

spec = importlib.util.spec_from_file_location("doum_gate", sys.argv[1])
gate = importlib.util.module_from_spec(spec)
gate._log = lambda m: print("  [gate]", m)
sys.modules["doum_gate"] = gate
spec.loader.exec_module(gate)
gate._log = lambda m: print("  [gate]", m)

def submit(name):
    item = (srv.number, f"id-{name}", {"name": name}, {"client_id": "c"}, ["9"], {})
    srv.number += 1; srv.prompt_queue.put(item)

for n in ["A", "B", "C"]: submit(n)
q = srv.prompt_queue
(a, ia) = q.get()                               # the worker starts A
assert a[2]["name"] == "A"
ok = gate._requeue_in_process(a[1])             # doum pauses ComfyUI: A is interrupted, re-queued before its run ends
q.task_done(ia)                                 # A's run ends
order = [q.get()[0][2]["name"] for _ in range(3)]
print("order after the pause:", order, "| in-process:", ok)
assert ok and order == ["A", "B", "C"], "the interrupted prompt must run first"
print("PASS: the interrupted prompt runs next, then the rest in their original order")
