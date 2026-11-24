//! The daemon's configuration: how to reach SpiceDB and Postgres, and how hard to run. What is projected (the
//! resource type, the permission, the tenant separator) is NOT here: it lives in `rebac.config`, the one table the SQL
//! and the daemon both read, so the two cannot disagree.

use serde::Deserialize;
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    /// Each lookup is at least as fresh as the write that queued it (its zedtoken); fully consistent without one.
    AtLeastAsFresh,
    FullyConsistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Relay the outbox and keep `rebac.readable`.
    Live,
    /// Write only `rebac.readable_shadow`, relay nothing, re-send nothing: for proving a projection before it is used.
    Shadow,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub database_url_env: String,
    pub spicedb_endpoint: String,
    /// The NAME of the environment variable holding the SpiceDB token, never the token.
    pub spicedb_token_env: String,
    pub consistency: Consistency,
    pub mode: Mode,
    pub listen: String,
    pub deadline_ms: u64,
    pub read_deadline_ms: u64,
    pub heartbeat_ms: u64,
    pub relay_batch: i32,
    pub relay_per_tenant: i32,
    pub relay_window: i32,
    pub relay_idle_ms: u64,
    pub relay_lock_timeout_ms: u64,
    pub relay_statement_timeout_ms: u64,
    pub refresh_concurrency: usize,
    pub refresh_lease_ms: i32,
    pub refresh_idle_ms: u64,
    pub sweep_interval_ms: i64,
    pub sweep_settle_ms: i64,
    pub watch: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            database_url_env: "DATABASE_URL".into(),
            spicedb_endpoint: "http://127.0.0.1:50051".into(),
            spicedb_token_env: "SPICEDB_TOKEN".into(),
            consistency: Consistency::AtLeastAsFresh,
            mode: Mode::Live,
            listen: "127.0.0.1:7341".into(),
            deadline_ms: 5_000,
            read_deadline_ms: 3_000,
            heartbeat_ms: 1_000,
            relay_batch: 200,
            relay_per_tenant: 50,
            relay_window: 5_000,
            relay_idle_ms: 200,
            relay_lock_timeout_ms: 15_000,
            relay_statement_timeout_ms: 60_000,
            refresh_concurrency: 4,
            refresh_lease_ms: 60_000,
            refresh_idle_ms: 250,
            sweep_interval_ms: 15 * 60_000,
            sweep_settle_ms: 5 * 60_000,
            watch: true,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        match path {
            None => Ok(Self::default()),
            Some(p) => Ok(toml::from_str(&std::fs::read_to_string(p)?)?),
        }
    }
    pub fn deadline(&self) -> Duration {
        Duration::from_millis(self.deadline_ms)
    }
    pub fn read_deadline(&self) -> Duration {
        Duration::from_millis(self.read_deadline_ms)
    }
    pub fn database_url(&self) -> anyhow::Result<String> {
        std::env::var(&self.database_url_env).map_err(|_| {
            anyhow::anyhow!(
                "set {} to the Postgres connection string",
                self.database_url_env
            )
        })
    }
    pub fn spicedb_token(&self) -> anyhow::Result<String> {
        std::env::var(&self.spicedb_token_env).map_err(|_| {
            anyhow::anyhow!(
                "set {} to the SpiceDB preshared key",
                self.spicedb_token_env
            )
        })
    }
}
