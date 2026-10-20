---
name: gpu-share
description: Use before ANY GPU work on this machine (local LLM inference or serving, training/fine-tuning, image/video/audio generation, ComfyUI renders, benchmarks). Explains how to run it through the doum GPU queue so jobs never collide, how to size it, and how to report progress.
---

# Sharing the GPU with doum

This machine has one GPU shared by several apps and agents. **All GPU work goes through doum**, a queue that starts a
job only when its memory fits, pauses and resumes long jobs when something urgent needs the GPU, and runs safety checks
(cool-down, temperature, free memory) before every job.

## Before you start GPU work
1. Look first: call the MCP tool `gpu_status` (or run `doum status`). Mention to the user if the GPU is busy.
2. Size the job in **shards** (~1 GB each): what the job really uses plus a little margin
   (e.g. a 7B model in 4-bit ≈ 6–8, an SDXL render ≈ 12–16, a 27B model in 4-bit ≈ 20–24).

## Running it
- From an agent: MCP `gpu_run` with `name`, `command`, `shards` (and `mem_gb` if known), then `gpu_wait` / `gpu_job`.
- From a shell: `doum run --shards N -- <command>` (foreground) or `doum run --detach --shards N -- <command>` for long
  jobs (pausable). Add `--interactive` only when the user is waiting for the result right now.
- **Local LLMs need no server start:** call the OpenAI-compatible endpoint `http://127.0.0.1:8410/v1`
  (models `gpt-oss-120b`, `qwen3.6-35b`). They load as doum jobs on demand and unload when idle.
- Long jobs should report progress so the queue can make good decisions:
  `doum progress "epoch 3/10" --eta 2h` (from inside the job), and save a checkpoint on SIGTERM (that's what "pause" sends).

## Rules
- Never start GPU programs directly (ollama run/serve, llama-server, vllm serve, basert bench/serve, torchrun, training
  or render scripts, posting to ComfyUI's /prompt). A hook blocks these and points here.
- Do not pause or cancel jobs you did not start; tell the user instead. Never answer the user's GPU questions for them.
- If something truly must bypass the queue, ask the user first, then add `# doum:allow` to the command.
