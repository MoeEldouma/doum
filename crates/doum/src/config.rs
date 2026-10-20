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
    pub memory: MemoryCfg,
    pub hooks: Hooks,
    pub notify: NotifyCfg,
    pub comfyui: ComfyCfg,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ComfyCfg {
    /// ComfyUI base URL. Used to free cached models when ComfyUI is idle and a job needs the memory.
    pub url: String,
    pub free_when_idle: bool,
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
            memory: MemoryCfg::default(),
            hooks: Hooks::default(),
            notify: NotifyCfg::default(),
            comfyui: ComfyCfg::default(),
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
