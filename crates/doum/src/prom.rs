//! Prometheus text format for `GET /metrics`, so doum can sit on a Grafana dashboard next to everything else.
//! Read-only, like `/api/status`; the daemon only listens on localhost.

use std::fmt::Write;

use crate::daemon::StatusView;
use crate::job::State;

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', " ")
}

fn lower<T: std::fmt::Debug>(v: T) -> String {
    format!("{v:?}").to_lowercase()
}

pub fn render(v: &StatusView) -> String {
    let mut o = String::new();
    let mut metric = |name: &str, kind: &str, help: &str, rows: Vec<(String, f64)>| {
        let _ = writeln!(o, "# HELP {name} {help}\n# TYPE {name} {kind}");
        for (labels, val) in rows {
            let _ = writeln!(o, "{name}{labels} {val}");
        }
    };
    let one = |x: f64| vec![(String::new(), x)];

    metric("doum_up", "gauge", "1 while the daemon is answering.", one(1.0));
    metric(
        "doum_shards_total",
        "gauge",
        "GPU memory shards (about 1 GB each).",
        one(v.total_shards as f64),
    );
    metric(
        "doum_shards_used",
        "gauge",
        "Shards held by running jobs.",
        one(v.used_shards as f64),
    );
    metric(
        "doum_memory_available_gb",
        "gauge",
        "Memory available to new GPU work.",
        one(v.mem_available_gb),
    );

    let states = [
        State::Pending,
        State::Running,
        State::Pausing,
        State::Done,
        State::Failed,
        State::Cancelled,
    ];
    metric(
        "doum_jobs",
        "gauge",
        "Jobs the daemon knows about, by state.",
        states
            .iter()
            .map(|s| {
                let n = v.jobs.iter().filter(|j| j.state == *s).count();
                (format!("{{state=\"{}\"}}", lower(s)), n as f64)
            })
            .collect(),
    );
    let live: Vec<_> = v.jobs.iter().filter(|j| !j.state.is_terminal()).collect();
    let jl = |j: &crate::job::Job| format!("id=\"{}\",name=\"{}\",lane=\"{}\"", j.id, esc(&j.name), lower(j.lane));
    metric(
        "doum_job_shards",
        "gauge",
        "Shards each live job holds (running) or asks for (waiting).",
        live.iter()
            .map(|j| (format!("{{{},state=\"{}\"}}", jl(j), lower(j.state)), j.shards as f64))
            .collect(),
    );
    metric(
        "doum_job_wait_seconds",
        "gauge",
        "How long each waiting job has been waiting.",
        live.iter()
            .filter(|j| j.state == State::Pending)
            .map(|j| (format!("{{{}}}", jl(j)), v.now.saturating_sub(j.pending_since) as f64))
            .collect(),
    );
    metric(
        "doum_job_energy_joules_total",
        "counter",
        "GPU energy attributed to each job (power split by shards).",
        v.jobs
            .iter()
            .map(|j| (format!("{{{}}}", jl(j)), (j.energy_j * 10.0).round() / 10.0))
            .collect(),
    );
    metric(
        "doum_job_requeues_total",
        "counter",
        "Times each job was paused and put back in the queue.",
        v.jobs.iter().map(|j| (format!("{{{}}}", jl(j)), j.requeues as f64)).collect(),
    );
    metric(
        "doum_decisions_open",
        "gauge",
        "Questions waiting for an answer (mode = ask).",
        one(v.decisions.len() as f64),
    );
    metric(
        "doum_unmanaged_gpu_processes",
        "gauge",
        "GPU processes running outside the queue.",
        one(v.unmanaged.len() as f64),
    );
    if let Some(s) = &v.latest {
        if let Some(c) = s.gpu_c {
            metric("doum_gpu_temperature_celsius", "gauge", "GPU temperature.", one(c));
        }
        if let Some(w) = s.gpu_w {
            metric("doum_gpu_power_watts", "gauge", "GPU power draw.", one(w));
        }
        if let Some(c) = s.soc_c {
            metric(
                "doum_soc_temperature_celsius",
                "gauge",
                "Hottest thermal zone (the SoC on a GB10).",
                one(c),
            );
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::Lane;
    use crate::sched::tests::job;

    #[test]
    fn renders_prometheus_text() {
        let mut a = job(1, Lane::Batch, 60, State::Running, 100);
        a.name = "comfy \"render\"".into();
        a.energy_j = 1234.56;
        let b = job(2, Lane::Interactive, 110, State::Pending, 150);
        let v = StatusView {
            total_shards: 110,
            used_shards: 60,
            mem_available_gb: 40.5,
            jobs: vec![a, b],
            queue: vec![2],
            decisions: vec![],
            unmanaged: vec![],
            latest: None,
            now: 200,
        };
        let t = render(&v);
        assert!(t.contains("doum_shards_used 60\n"));
        assert!(t.contains("doum_jobs{state=\"running\"} 1\n"));
        assert!(t.contains("name=\"comfy \\\"render\\\"\""));
        assert!(t.contains("doum_job_wait_seconds{id=\"2\",name=\"j2\",lane=\"interactive\"} 50\n"));
        assert!(t.contains("doum_job_energy_joules_total{id=\"1\""));
        assert!(t.lines().filter(|l| l.starts_with("# TYPE")).count() >= 10);
    }
}
