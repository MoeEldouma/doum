//! `doum claude-hook`: a Claude Code PreToolUse hook. It reads the tool call on stdin and blocks shell commands
//! that would start GPU work outside the queue, telling Claude to use doum instead. Everything else passes.
//! Escape hatch for a deliberate exception: put `# doum:allow` in the command.

use regex::RegexBuilder;
use serde_json::{json, Value};
use std::io::Read;

use crate::config::ClaudeHookCfg;

/// Why a command must go through doum, or None if it may run as is.
pub fn check(cfg: &ClaudeHookCfg, command: &str) -> Option<String> {
    if command.contains("doum:allow") || command.split_whitespace().any(|w| w == "doum" || w.ends_with("/doum")) {
        return None;
    }
    for p in &cfg.gpu_patterns {
        let Ok(re) = RegexBuilder::new(p).case_insensitive(true).build() else { continue };
        if let Some(m) = re.find(command) {
            return Some(format!(
                "`{}` starts GPU work outside the GPU queue (doum). On this machine all GPU work goes through doum so jobs \
                 don't crash each other: use the doum MCP tools (gpu_status, then gpu_run) or prefix the command with \
                 `doum run --shards <N> --` (add --detach for long jobs). Local LLMs are served on demand at \
                 http://127.0.0.1:8410/v1 (llama-swap), no need to start a server. If this really must bypass the queue, \
                 ask the user and add `# doum:allow`.",
                m.as_str()
            ));
        }
    }
    None
}

/// Hook entry point: exit 0 with no output = allow; a JSON "deny" decision = block with a reason Claude reads.
pub fn run(cfg: &ClaudeHookCfg) -> i32 {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return 0; // never break Claude on a hook problem
    }
    let v: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    if v.get("tool_name").and_then(|t| t.as_str()) != Some("Bash") {
        return 0;
    }
    let cmd = v.pointer("/tool_input/command").and_then(|c| c.as_str()).unwrap_or("");
    if let Some(reason) = check(cfg, cmd) {
        let out = json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason
            }
        });
        println!("{out}");
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked(cmd: &str) -> bool {
        check(&ClaudeHookCfg::default(), cmd).is_some()
    }

    #[test]
    fn blocks_direct_gpu_work() {
        assert!(blocked("ollama run qwen3"));
        assert!(blocked("OLLAMA_HOST=x ollama serve &"));
        assert!(blocked("./build/bin/llama-server -m model.gguf"));
        assert!(blocked("vllm serve Qwen/Qwen3-8B"));
        assert!(blocked("basert bench basecompute/Qwen3-8B"));
        assert!(blocked("cd x && python3 train_lora.py --epochs 3"));
        assert!(blocked("python scripts/render_episode.py"));
        assert!(blocked("torchrun --nproc 1 finetune.py"));
        assert!(blocked("curl -X POST http://127.0.0.1:8188/prompt -d @wf.json"));
    }

    #[test]
    fn allows_everything_else() {
        assert!(!blocked("ollama list"));
        assert!(!blocked("ollama pull gpt-oss:120b"));
        assert!(!blocked("python3 -m pytest tests"));
        assert!(!blocked("python manage.py migrate"));
        assert!(!blocked("cargo build --release"));
        assert!(!blocked("curl http://127.0.0.1:8188/queue"));
        assert!(!blocked("ls ~/models"));
    }

    #[test]
    fn allows_doum_and_explicit_exceptions() {
        assert!(!blocked("doum run -s 24 -- python3 train_lora.py"));
        assert!(!blocked("~/.local/bin/doum run -d -s 60 -- ollama serve"));
        assert!(!blocked("python3 render_test.py  # doum:allow"));
    }
}
