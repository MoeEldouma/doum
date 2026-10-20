//! Talking to the local daemon (CLI, MCP server).

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::time::Duration;

use crate::config::{Config, Paths};

pub struct Client {
    base: String,
    token: String,
    http: reqwest::blocking::Client,
}

impl Client {
    pub fn new(cfg: &Config, paths: &Paths) -> Result<Self> {
        // Inside a job the daemon passes its address and token in the environment.
        let base = std::env::var("DOUM_API").unwrap_or_else(|_| format!("http://{}", cfg.listen));
        let token = match std::env::var("DOUM_TOKEN") {
            Ok(t) => t,
            Err(_) => std::fs::read_to_string(paths.token_file())
                .map(|t| t.trim().to_string())
                .unwrap_or_default(),
        };
        let http = reqwest::blocking::Client::builder().timeout(Duration::from_secs(15)).build()?;
        Ok(Self { base, token, http })
    }

    fn check(r: reqwest::blocking::Response) -> Result<Value> {
        let status = r.status();
        let v: Value = r.json().unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{}", v.get("error").and_then(|e| e.as_str()).unwrap_or(status.as_str()));
        }
        Ok(v)
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let r = self.http.get(format!("{}{path}", self.base)).send().map_err(not_running)?;
        Ok(serde_json::from_value(Self::check(r)?)?)
    }

    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &Value) -> Result<T> {
        let r = self
            .http
            .post(format!("{}{path}", self.base))
            .header("x-doum-token", &self.token)
            .json(body)
            .send()
            .map_err(not_running)?;
        serde_json::from_value(Self::check(r)?).context("unexpected reply from the daemon")
    }
}

fn not_running(e: reqwest::Error) -> anyhow::Error {
    if e.is_connect() {
        anyhow!("the doum daemon is not running (start it: `systemctl --user start doum` or `doum daemon`)")
    } else {
        anyhow!(e)
    }
}
