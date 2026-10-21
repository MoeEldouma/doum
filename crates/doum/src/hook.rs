//! `doum claude-hook`: a Claude Code PreToolUse hook. It reads the tool call on stdin and blocks shell commands
//! that would start GPU work outside the queue, telling Claude to use doum instead. Everything else passes.
//! Escape hatch for a deliberate exception: put `# doum:allow` in the command.

use regex::RegexBuilder;
use serde_json::{json, Value};
use std::io::Read;

use crate::config::ClaudeHookCfg;

/// Split a shell command into the parts that run separately (`;`, `&&`, `||`, `|`, `&`, newlines), keeping quoted
/// text together, so `doum run -- sh -c 'a; b'` stays one part and `echo doum; ollama run x` is two.
fn segments(command: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let (mut sq, mut dq) = (false, false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !sq => {
                cur.push(c);
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            '\'' if !dq => {
                sq = !sq;
                cur.push(c);
            }
            '"' if !sq => {
                dq = !dq;
                cur.push(c);
            }
            ';' | '&' | '|' | '\n' if !sq && !dq => {
                while matches!(chars.peek(), Some('&') | Some('|')) {
                    chars.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// Where `doum` itself is invoked in a part (as a word: `doum` or a path ending in `/doum`), if it is.
fn doum_at(seg: &str) -> Option<usize> {
    let mut pos = 0;
    for w in seg.split_whitespace() {
        let at = seg[pos..].find(w).map(|i| i + pos)?;
        pos = at + w.len();
        if w == "doum" || w.ends_with("/doum") {
            return Some(at);
        }
    }
    None
}

/// Why a command must go through doum, or None if it may run as is. Each part of a compound command is checked on
/// its own: a part that starts GPU work passes only if doum runs it (`doum run ... -- <gpu command>`).
pub fn check(cfg: &ClaudeHookCfg, command: &str) -> Option<String> {
    if command.contains("doum:allow") {
        return None;
    }
    let patterns: Vec<regex::Regex> = cfg
        .gpu_patterns
        .iter()
        .filter_map(|p| RegexBuilder::new(p).case_insensitive(true).build().ok())
        .collect();
    for seg in segments(command) {
        for re in &patterns {
            let Some(m) = re.find(&seg) else { continue };
            if doum_at(&seg).is_some_and(|d| d < m.start()) {
                continue; // `doum run ... -- <this>`: queued
            }
            return Some(format!(
                "`{}` starts GPU work outside the GPU queue (doum). On this machine all GPU work goes through doum so jobs \
                 don't crash each other: use the doum MCP tools (gpu_status, then gpu_run) or prefix the command with \
                 `doum run --shards <N> --` (add --detach for long jobs).{} If this really must bypass the queue, \
                 ask the user and add `# doum:allow`.",
                m.as_str(),
                if cfg.hint.is_empty() {
                    String::new()
                } else {
                    format!(" {}", cfg.hint.trim())
                }
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
    fn a_mention_of_doum_does_not_let_other_parts_through() {
        assert!(blocked("echo doum; ollama run qwen3"));
        assert!(blocked("doum status && python3 train_lora.py"));
        assert!(blocked("ollama serve | doum progress x"));
        assert!(blocked("cat doum.toml\nvllm serve Qwen/Qwen3-8B"));
        assert!(blocked("echo 'doum' && torchrun x.py"));
        assert!(blocked("ollama run x # mentions doum"));
    }

    #[test]
    fn quoted_commands_stay_whole() {
        assert!(!blocked("doum run -s 8 -- sh -c 'cd x; python3 train_lora.py'"));
        assert!(!blocked("doum run -s 8 -- bash -lc \"ollama run qwen3 && echo done\""));
        assert!(!blocked("cd proj && doum run -d -s 24 -- python3 train_lora.py"));
        assert!(!blocked("nohup doum run -d -s 60 -- ollama serve &"));
    }

    #[test]
    fn allows_doum_and_explicit_exceptions() {
        assert!(!blocked("doum run -s 24 -- python3 train_lora.py"));
        assert!(!blocked("~/.local/bin/doum run -d -s 60 -- ollama serve"));
        assert!(!blocked("python3 render_test.py  # doum:allow"));
    }
}
