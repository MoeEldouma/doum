//! Configuration: `~/.config/doum/doum.toml` (every key optional; defaults below).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::job::Mode;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// GPU memory budget in shards. On a 128 GB unified-memory GB10, 110 leaves room for the OS and desktop.
    pub total_shards: u32,
    /// Roughly how many GB one shard is (used for the free-memory check before a job starts).
    pub gb_per_shard: f64,
    /// Local API + panel.
    pub listen: String,
    /// Default mode when a job does not fit: smart | wait | pause | ask.
    pub default_mode: Mode,
    /// Smart: jobs finishing within this many seconds are waited for; longer ones are paused.
    pub smart_wait_s: u64,
    /// Ask the user after a job has waited this long (0 = never). The "20-minute check-back".
    pub max_wait_s: u64,
    /// How long an "ask" question stays open before the recommendation is applied.
    pub ask_timeout_s: u64,
    /// Time a paused job gets to save a checkpoint after SIGTERM, before SIGKILL.
    pub pause_grace_s: u64,
    /// GPU processes outside the queue smaller than this (GB) are not reported (driver contexts, idle servers).
    pub unmanaged_min_gb: f64,
    /// A job standing in for an app (a lease, like ComfyUI's) that has reported progress before and then goes quiet
    /// this long is treated as stuck: its shards are released and you are told. 0 = never.
    pub stale_lease_s: u64,
    /// Who runs background jobs: `native` (Doum itself) or `slurm` (Doum decides, Slurm runs them).
    pub backend: Backend,
    pub slurm: SlurmCfg,
    pub memory: MemoryCfg,
    pub hooks: Hooks,
    pub notify: NotifyCfg,
    pub comfyui: ComfyCfg,
    pub mcp: McpCfg,
    pub claude_hook: ClaudeHookCfg,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Native,
    Slurm,
}

/// The Slurm backend: partitions per lane, the GRES name for shards, and the Slurm commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SlurmCfg {
    pub interactive_partition: String,
    pub batch_partition: String,
    /// GRES name of the GPU shards in gres.conf.
    pub gres: String,
    /// Extra `sbatch` arguments for every job, e.g. `["--qos=normal"]`.
    pub extra_args: Vec<String>,
    pub sbatch: String,
    pub squeue: String,
    pub scancel: String,
    pub sinfo: String,
}

impl Default for SlurmCfg {
    fn default() -> Self {
        Self {
            interactive_partition: "interactive".into(),
            batch_partition: "batch".into(),
            gres: "shard".into(),
            extra_args: vec![],
            sbatch: "sbatch".into(),
            squeue: "squeue".into(),
            scancel: "scancel".into(),
            sinfo: "sinfo".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryCfg {
    /// Before a job starts, wait until MemAvailable >= shards * gb_per_shard + headroom_gb.
    pub check: bool,
    pub headroom_gb: f64,
}

/// Commands run inside the job, around the user's command. `{name}` and `{id}` are substituted.
/// `pre_start` must exit 0 before the job's command runs (it may block, e.g. a thermal cool-down gate).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Hooks {
    pub pre_start: Vec<String>,
    pub post_end: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyCfg {
    pub desktop: bool,
    /// ntfy topic URL for phone push, e.g. "https://ntfy.sh/<private-topic>". Empty = off.
    pub ntfy_url: String,
    /// Extra webhooks (JSON POST of every alert).
    pub webhooks: Vec<String>,
    /// Answer questions from the phone: ntfy buttons send the answer to a second private topic that Doum listens to.
    pub phone_answers: bool,
    /// That topic. Empty: the alerts topic with `-answers` added.
    pub ntfy_answers_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComfyCfg {
    /// ComfyUI base URL. Used to free cached models when ComfyUI is idle and a job needs the memory.
    pub url: String,
    pub free_when_idle: bool,
}

/// Limits for AI agents using the MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpCfg {
    /// Largest job an agent may submit, in shards.
    pub max_shards: u32,
    /// How many unfinished jobs agents may have at once.
    pub max_jobs: u32,
    /// What an agent's job does when the GPU is busy. `wait` never pauses anyone else's work.
    pub mode: Mode,
    /// Let agents pause / cancel jobs they did not start.
    pub allow_control_others: bool,
    /// Let agents answer the user's GPU questions.
    pub allow_answer: bool,
}

impl Default for McpCfg {
    fn default() -> Self {
        Self {
            max_shards: 60,
            max_jobs: 2,
            mode: Mode::Wait,
            allow_control_others: false,
            allow_answer: false,
        }
    }
}

/// `doum claude-hook`: a Claude Code PreToolUse hook that stops shell commands which would start GPU work
/// outside the queue. Patterns are case-insensitive regular expressions. (ComfyUI is covered by its doum_gate plugin
/// instead: every prompt waits its turn, however it was posted.)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClaudeHookCfg {
    pub gpu_patterns: Vec<String>,
    /// Added to the hook's message, e.g. where local LLMs are served
    /// ("Local LLMs load on demand at http://127.0.0.1:8410/v1 (llama-swap)."). Empty = nothing added.
    pub hint: String,
}

impl Default for ClaudeHookCfg {
    fn default() -> Self {
        Self {
            gpu_patterns: [
                r"\bollama\s+(run|serve)\b",
                r"\bllama-(server|cli|bench)\b",
                r"\bvllm\s+serve\b",
                r"\bsglang\.launch_server\b",
                r"\bbasert\s+(bench|serve|chat|run|complete)\b",
                r"\btorchrun\b",
                r"\baccelerate\s+launch\b",
                r"\bpython[0-9.]*\s+[^|;&]*\b[\w-]*(train|finetune|fine_tune|infer|inference|generate|render)[\w-]*\.py\b",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            hint: String::new(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            total_shards: 110,
            gb_per_shard: 1.0,
            listen: "127.0.0.1:8420".into(),
            default_mode: Mode::Ask,
            smart_wait_s: 600,
            max_wait_s: 1200,
            ask_timeout_s: 60,
            pause_grace_s: 90,
            unmanaged_min_gb: 1.0,
            stale_lease_s: 300,
            backend: Backend::Native,
            slurm: SlurmCfg::default(),
            memory: MemoryCfg::default(),
            hooks: Hooks::default(),
            notify: NotifyCfg::default(),
            comfyui: ComfyCfg::default(),
            mcp: McpCfg::default(),
            claude_hook: ClaudeHookCfg::default(),
        }
    }
}
impl Default for MemoryCfg {
    fn default() -> Self {
        Self {
            check: true,
            headroom_gb: 6.0,
        }
    }
}
impl Default for NotifyCfg {
    fn default() -> Self {
        Self {
            desktop: true,
            ntfy_url: String::new(),
            webhooks: vec![],
            phone_answers: true,
            ntfy_answers_url: String::new(),
        }
    }
}
impl Default for ComfyCfg {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8188".into(),
            free_when_idle: true,
        }
    }
}

/// Where doum keeps things. Overridable with DOUM_CONFIG / DOUM_STATE_DIR (tests use temp dirs).
pub struct Paths {
    pub config: PathBuf,
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn get() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "doum").context("no home directory")?;
        let config = std::env::var_os("DOUM_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| dirs.config_dir().join("doum.toml"));
        let state_dir = std::env::var_os("DOUM_STATE_DIR").map(PathBuf::from).unwrap_or_else(|| {
            dirs.state_dir()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| dirs.data_local_dir().to_path_buf())
        });
        std::fs::create_dir_all(&state_dir)?;
        // jobs' environments (which can hold API keys), logs and the API token live here: owner only
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o700))?;
        }
        std::fs::create_dir_all(state_dir.join("logs"))?;
        std::fs::create_dir_all(state_dir.join("exit"))?;
        Ok(Self { config, state_dir })
    }
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    pub fn token_file(&self) -> PathBuf {
        self.state_dir.join("token")
    }
    pub fn exit_file(&self, id: u64, attempt: u32) -> PathBuf {
        self.state_dir.join("exit").join(format!("{id}.{attempt}"))
    }
    pub fn log_file(&self, id: u64, name: &str) -> PathBuf {
        let safe: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' })
            .collect();
        self.state_dir.join("logs").join(format!("{id}-{safe}.log"))
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.total_shards, 110);
        assert_eq!(c.max_wait_s, 1200);
    }

    #[test]
    fn partial_file_overrides_only_given_keys() {
        let c: Config = toml::from_str("total_shards = 64\n[memory]\nheadroom_gb = 2.0\n").unwrap();
        assert_eq!(c.total_shards, 64);
        assert!(c.memory.check);
        assert_eq!(c.memory.headroom_gb, 2.0);
        assert_eq!(c.smart_wait_s, 600);
    }
}
