# Using doum with Claude Code (or any MCP client)

1. MCP tools for every session:
   `claude mcp add --scope user doum -- doum mcp`
   Tools: gpu_status, gpu_run, gpu_job, gpu_wait, gpu_pause, gpu_cancel, gpu_answer (limits in `[mcp]` config).
2. Skill: copy `skills/gpu-share` to `~/.claude/skills/`.
3. Optional backstop hook in `~/.claude/settings.json` (blocks shell commands that start GPU work outside the queue;
   patterns in `[claude_hook]` config, escape hatch `# doum:allow`):
   ```json
   { "hooks": { "PreToolUse": [ { "matcher": "Bash",
       "hooks": [ { "type": "command", "command": "doum claude-hook", "timeout": 10 } ] } ] } }
   ```
4. One line in `~/.claude/CLAUDE.md`: "GPU work goes through doum: use the gpu-share skill."
