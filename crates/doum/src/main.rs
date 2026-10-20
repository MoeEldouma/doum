//! doum: share one GPU between ComfyUI, local LLMs, training and AI agents.
//! Named after the doum palm of the Nile: one tree, shade for every traveller.

mod client;
mod config;
mod daemon;
mod exec;
mod job;
mod notify;
mod sched;
mod store;
mod sys;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

use client::Client;
use config::{Config, Paths};
use daemon::{fmt_dur, StatusView, SubmitResp};
use job::{now, Job, Lane, Mode, State};

#[derive(Parser)]
#[command(name = "doum", version, about = "Share one GPU between ComfyUI, local LLMs, training and AI agents.", long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command on the GPU when it has room (waits its turn; asks you if the GPU is busy).
    Run {
        /// GPU memory shards to reserve (~1 GB each). Use the total (e.g. 110) for the whole GPU.
        #[arg(short, long, default_value_t = 8)]
        shards: u32,
        /// Memory the job really uses (GB), for the free-memory check. Defaults from --shards.
        #[arg(long)]
        mem: Option<f64>,
        #[arg(short, long)]
        name: Option<String>,
        /// Someone is waiting for this: schedule before batch work.
        #[arg(short, long)]
        interactive: bool,
        /// When the GPU is busy: smart | wait | pause | ask (default from config).
        #[arg(short, long, value_parser = parse_mode)]
        mode: Option<Mode>,
        /// Run in the background (daemon-managed, pausable). Without it, the command runs here, in the foreground.
        #[arg(short, long)]
        detach: bool,
        /// Never pause this job.
        #[arg(long)]
        no_pause: bool,
        /// Ask me after this long still waiting (e.g. 20m; 0 = never).
        #[arg(long, value_parser = humantime::parse_duration)]
        max_wait: Option<Duration>,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
    /// What is on the GPU, what is waiting, and why.
    Status {
        #[arg(long)]
        json: bool,
        /// Include finished jobs.
        #[arg(short, long)]
        all: bool,
    },
    /// Pause a background job (it saves, stops and resumes later on its own).
    Pause { id: u64 },
    /// Cancel a job (waiting or running).
    Cancel { id: u64 },
    /// Move a waiting job to the front of its lane.
    Top { id: u64 },
    /// Answer a question (e.g. `doum answer 12 smart`).
    Answer { question: u64, choice: String },
    /// From inside a job: report progress, e.g. `doum progress "clip 2/5" --eta 5m`.
    Progress {
        message: String,
        #[arg(long, value_parser = humantime::parse_duration)]
        eta: Option<Duration>,
        /// From now on, do not pause this job.
        #[arg(long)]
        no_pause: bool,
    },
    /// Show a job's log.
    Logs {
        id: u64,
        #[arg(short, long)]
        follow: bool,
    },
    /// Run the daemon (normally via `systemctl --user start doum`).
    Daemon,
    #[command(hide = true, name = "_exec")]
    Exec {
        #[arg(long)]
        id: u64,
        #[arg(long)]
        attempt: u32,
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 0.0)]
        mem_gb: f64,
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
}

fn parse_mode(s: &str) -> Result<Mode, String> {
    match s {
        "smart" => Ok(Mode::Smart),
        "wait" => Ok(Mode::Wait),
        "pause" => Ok(Mode::Pause),
        "ask" => Ok(Mode::Ask),
        _ => Err("use smart, wait, pause or ask".into()),
    }
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = real_main(cli) {
        eprintln!("doum: {e:#}");
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<()> {
    let paths = Paths::get()?;
    let cfg = Config::load(&paths.config)?;
    match cli.cmd {
        Cmd::Daemon => {
            tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "doum=info".into()))
                .init();
            tokio::runtime::Runtime::new()?.block_on(daemon::serve(cfg, paths))
        }
        Cmd::Exec {
            id,
            attempt,
            name,
            mem_gb,
            command,
        } => {
            let code = exec::run(
                &cfg,
                &paths,
                &exec::ExecArgs {
                    id,
                    attempt,
                    name: &name,
                    mem_gb,
                    cmd: &command,
                },
            );
            std::process::exit(code);
        }
        Cmd::Run {
            shards,
            mem,
            name,
            interactive,
            mode,
            detach,
            no_pause,
            max_wait,
            command,
        } => {
            let code = run(
                &cfg,
                &paths,
                shards,
                mem,
                name,
                interactive,
                mode,
                detach,
                no_pause,
                max_wait,
                command,
            )?;
            std::process::exit(code);
        }
        Cmd::Status { json, all } => {
            let s: StatusView = Client::new(&cfg, &paths)?.get("/api/status")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s)?);
            } else {
                print_status(&s, all);
            }
            Ok(())
        }
        Cmd::Pause { id } => simple(
            &cfg,
            &paths,
            &format!("/api/jobs/{id}/pause"),
            json!({}),
            &format!("job {id}: pausing (it resumes on its own)"),
        ),
        Cmd::Cancel { id } => simple(
            &cfg,
            &paths,
            &format!("/api/jobs/{id}/cancel"),
            json!({}),
            &format!("job {id}: cancelled"),
        ),
        Cmd::Top { id } => simple(
            &cfg,
            &paths,
            &format!("/api/jobs/{id}/top"),
            json!({}),
            &format!("job {id}: moved to the front"),
        ),
        Cmd::Answer { question, choice } => simple(
            &cfg,
            &paths,
            &format!("/api/decisions/{question}"),
            json!({ "choice": choice }),
            "answered",
        ),
        Cmd::Progress { message, eta, no_pause } => {
            let Ok(id) = std::env::var("DOUM_JOB_ID") else {
                bail!("`doum progress` works inside a job started by doum (DOUM_JOB_ID is not set)")
            };
            let body =
                json!({ "message": message, "eta_s": eta.map(|d| d.as_secs()), "no_pause": if no_pause { Some(true) } else { None } });
            let _: serde_json::Value = Client::new(&cfg, &paths)?.post(&format!("/api/jobs/{id}/progress"), &body)?;
            Ok(())
        }
        Cmd::Logs { id, follow } => {
            let j: Job = Client::new(&cfg, &paths)?.get(&format!("/api/jobs/{id}"))?;
            let Some(log) = j.log else {
                bail!("job {id} has no log (foreground jobs print to their own terminal)")
            };
            let mut c = std::process::Command::new("tail");
            if follow {
                c.arg("-f");
            } else {
                c.args(["-n", "200"]);
            }
            c.arg(log).status()?;
            Ok(())
        }
    }
}

fn simple(cfg: &Config, paths: &Paths, path: &str, body: serde_json::Value, ok: &str) -> Result<()> {
    let _: serde_json::Value = Client::new(cfg, paths)?.post(path, &body)?;
    println!("{ok}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run(
    cfg: &Config,
    paths: &Paths,
    shards: u32,
    mem: Option<f64>,
    name: Option<String>,
    interactive: bool,
    mode: Option<Mode>,
    detach: bool,
    no_pause: bool,
    max_wait: Option<Duration>,
    command: Vec<String>,
) -> Result<i32> {
    let c = Client::new(cfg, paths)?;
    let env: BTreeMap<String, String> = std::env::vars().filter(|(k, _)| !k.starts_with("DOUM_")).collect();
    let name = name.unwrap_or_else(|| {
        std::path::Path::new(&command[0])
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let body = json!({
        "name": name, "cmd": command, "cwd": std::env::current_dir()?.display().to_string(), "env": env,
        "shards": shards, "mem_gb": mem, "lane": if interactive { Lane::Interactive } else { Lane::Batch },
        "mode": mode, "detached": detach, "no_pause": no_pause, "max_wait_s": max_wait.map(|d| d.as_secs()),
        "owner": std::env::var("USER").unwrap_or_default(), "client_pid": if detach { None } else { Some(std::process::id() as i32) },
    });
    let r: SubmitResp = c.post("/api/jobs", &body)?;
    let id = r.job.id;
    eprintln!("doum: job {id} '{}' queued ({} shards)", r.job.name, r.job.shards);
    if let Some(d) = &r.decision {
        eprintln!(
            "doum: the GPU is busy — {}\ndoum: answer with `doum answer {} <{}>` (no answer → {} in {})",
            d.text,
            d.id,
            d.options.join("|"),
            d.recommended,
            fmt_dur(d.deadline.saturating_sub(now()))
        );
    }
    if let Some(p) = &r.plan {
        if !p.pause.is_empty() {
            eprintln!("doum: pausing job(s) {:?} to make room", p.pause);
        }
        if let Some(eta) = p.eta_s.filter(|e| *e > 0) {
            eprintln!("doum: expected start in ~{}", fmt_dur(eta));
        }
    }
    if detach {
        println!("{id}");
        return Ok(0);
    }

    // Foreground: wait for our turn (Ctrl-C cancels), then run the command right here.
    let cancel_on_ctrlc = {
        let token_path = paths.token_file();
        let base = format!("http://{}", cfg.listen);
        move || {
            let tok = std::fs::read_to_string(token_path).unwrap_or_default();
            let _ = reqwest::blocking::Client::new()
                .post(format!("{base}/api/jobs/{id}/cancel"))
                .header("x-doum-token", tok.trim())
                .send();
        }
    };
    let mut last_reason = String::new();
    loop {
        let j: Job = c.get(&format!("/api/jobs/{id}"))?;
        match j.state {
            State::Running => break,
            State::Pending => {
                if j.reason != last_reason {
                    eprintln!("doum: {}", j.reason);
                    last_reason = j.reason.clone();
                }
            }
            s if s.is_terminal() => {
                eprintln!("doum: job {id} {s:?}: {}", j.reason);
                return Ok(1);
            }
            _ => {}
        }
        if ctrlc_pressed() {
            cancel_on_ctrlc();
            eprintln!("doum: cancelled");
            return Ok(130);
        }
        std::thread::sleep(Duration::from_millis(700));
    }
    eprintln!("doum: GPU ready, starting");
    let j: Job = c.get(&format!("/api/jobs/{id}"))?;
    let mem_gb = mem.unwrap_or_else(|| (shards as f64 * cfg.gb_per_shard).min(cfg.total_shards as f64 * cfg.gb_per_shard * 0.6));
    std::env::set_var("DOUM_JOB_ID", id.to_string());
    let code = exec::run(
        cfg,
        paths,
        &exec::ExecArgs {
            id,
            attempt: j.requeues,
            name: &j.name,
            mem_gb,
            cmd: &j.cmd,
        },
    );
    let _: serde_json::Value = c.post(&format!("/api/jobs/{id}/done"), &json!({ "code": code }))?;
    Ok(code)
}

/// SIGINT while waiting in the queue: noticed between polls so we can cancel the job cleanly.
fn ctrlc_pressed() -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Once;
    static PRESSED: AtomicBool = AtomicBool::new(false);
    static INIT: Once = Once::new();
    extern "C" fn on_int(_: i32) {
        PRESSED.store(true, Ordering::SeqCst);
    }
    INIT.call_once(|| unsafe {
        let _ = nix::sys::signal::signal(nix::sys::signal::Signal::SIGINT, nix::sys::signal::SigHandler::Handler(on_int));
        let _ = nix::sys::signal::signal(nix::sys::signal::Signal::SIGTERM, nix::sys::signal::SigHandler::Handler(on_int));
    });
    PRESSED.load(Ordering::SeqCst)
}

fn print_status(s: &StatusView, all: bool) {
    let bar_w = 40usize;
    let used = (s.used_shards as usize * bar_w) / s.total_shards.max(1) as usize;
    println!(
        "GPU  [{}{}]  {}/{} shards used · {:.0} GB memory free",
        "█".repeat(used),
        "░".repeat(bar_w - used),
        s.used_shards,
        s.total_shards,
        s.mem_available_gb
    );
    let show = |j: &&Job| all || !j.state.is_terminal();
    let mut running: Vec<&Job> = s.jobs.iter().filter(|j| j.state.holds_shards()).collect();
    running.sort_by_key(|j| j.started_at);
    println!("\nRUNNING");
    if running.is_empty() {
        println!("  (nothing)");
    }
    for j in running {
        let eta = j.eta_left(s.now).map(|e| format!("~{} left", fmt_dur(e))).unwrap_or_default();
        let ran = j.started_at.map(|t| fmt_dur(s.now.saturating_sub(t))).unwrap_or_default();
        println!(
            "  {:>4}  {:<28} {:>3} shards  {:<11} {:>8}  {}  {}{}",
            j.id,
            trunc(&j.name, 28),
            j.shards,
            format!("{:?}", j.lane).to_lowercase(),
            ran,
            eta,
            j.progress.message,
            if j.state == State::Pausing { "  [pausing]" } else { "" }
        );
    }
    println!("\nWAITING (in start order)");
    if s.queue.is_empty() {
        println!("  (nothing)");
    }
    for id in &s.queue {
        if let Some(j) = s.jobs.iter().find(|j| j.id == *id) {
            println!(
                "  {:>4}  {:<28} {:>3} shards  {:<11} {}",
                j.id,
                trunc(&j.name, 28),
                j.shards,
                format!("{:?}", j.lane).to_lowercase(),
                j.reason
            );
        }
    }
    if !s.decisions.is_empty() {
        println!("\nQUESTIONS FOR YOU");
        for d in &s.decisions {
            println!(
                "  #{}  {}\n       answer: doum answer {} <{}>  (default {} in {})",
                d.id,
                d.text.replace('\n', "\n       "),
                d.id,
                d.options.join("|"),
                d.recommended,
                fmt_dur(d.deadline.saturating_sub(s.now))
            );
        }
    }
    if !s.unmanaged.is_empty() {
        println!("\nOUTSIDE THE QUEUE (using the GPU without doum)");
        for p in &s.unmanaged {
            println!("  pid {:<8} {:<24} ~{:.0} GB", p.pid, p.name, p.mem_gb);
        }
    }
    if all {
        println!("\nFINISHED");
        for j in s.jobs.iter().filter(|j| j.state.is_terminal()).filter(show) {
            println!(
                "  {:>4}  {:<28} {:<9} {}",
                j.id,
                trunc(&j.name, 28),
                format!("{:?}", j.state).to_lowercase(),
                j.reason
            );
        }
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    }
}
