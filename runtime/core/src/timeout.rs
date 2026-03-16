use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::Duration;

/// Timeout configuration for a workflow execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Maximum time a single node may run before being killed and failed.
    #[serde(
        default,
        serialize_with = "ser_opt_duration",
        deserialize_with = "de_opt_duration"
    )]
    pub node_timeout: Option<Duration>,
    /// Maximum total time a workflow execution may run.
    #[serde(
        default,
        serialize_with = "ser_opt_duration",
        deserialize_with = "de_opt_duration"
    )]
    pub workflow_timeout: Option<Duration>,
    /// How often a worker must renew its lease heartbeat.
    /// If a worker misses this, the lease is reclaimed and the node is re-queued.
    #[serde(
        default = "default_heartbeat",
        serialize_with = "ser_duration",
        deserialize_with = "de_duration"
    )]
    pub heartbeat_interval: Duration,
    /// Maximum time a human_approval node waits before routing to fallback or failing.
    #[serde(
        default,
        serialize_with = "ser_opt_duration",
        deserialize_with = "de_opt_duration"
    )]
    pub approval_timeout: Option<Duration>,
}

fn default_heartbeat() -> Duration {
    Duration::from_secs(30)
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            node_timeout: Some(Duration::from_secs(300)), // 5 min
            workflow_timeout: None,
            heartbeat_interval: default_heartbeat(),
            approval_timeout: None,
        }
    }
}

// Serialize Duration as seconds (integer).
fn ser_duration<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_u64(d.as_secs())
}

fn ser_opt_duration<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(d) => s.serialize_some(&d.as_secs()),
        None => s.serialize_none(),
    }
}

// Deserialize Duration from either integer (seconds) or Duration struct.
fn de_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum DurationRepr {
        Secs(u64),
        Struct { secs: u64, nanos: u32 },
    }
    match DurationRepr::deserialize(d)? {
        DurationRepr::Secs(s) => Ok(Duration::from_secs(s)),
        DurationRepr::Struct { secs, nanos } => Ok(Duration::new(secs, nanos)),
    }
}

fn de_opt_duration<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OptDurationRepr {
        Null,
        Secs(u64),
        Struct { secs: u64, nanos: u32 },
    }
    match Option::<OptDurationRepr>::deserialize(d)? {
        None | Some(OptDurationRepr::Null) => Ok(None),
        Some(OptDurationRepr::Secs(s)) => Ok(Some(Duration::from_secs(s))),
        Some(OptDurationRepr::Struct { secs, nanos }) => Ok(Some(Duration::new(secs, nanos))),
    }
}
