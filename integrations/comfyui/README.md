# ComfyUI gate plugin

Every ComfyUI prompt (from the UI or any script) waits for its turn in the Doum queue.

Install: `ln -s /path/to/doum/integrations/comfyui/doum_gate /path/to/ComfyUI/custom_nodes/doum_gate`, restart ComfyUI.
Settings (environment of the ComfyUI process): `DOUM_COMFY_SHARDS` (60), `DOUM_COMFY_IDLE_S` (90), `DOUM_BIN`, `DOUM_API`.

- First prompt: ComfyUI takes a lease (a background Doum job) and holds it while it has work; released after idle.
- Progress + ETA are reported to Doum for smart decisions.
- Paused by doum: the running prompt is interrupted, models are unloaded, then (handshake) the lease is handed back,
  so the next job never overlaps; the interrupted prompt is queued again at the front.
- Doum down or missing: ComfyUI runs exactly as before.

Known limit: if another prompt was already waiting, it may run before the re-queued one.
