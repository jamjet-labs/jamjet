use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::time::Duration;

/// Timeout configuration for a workflow execution.
///
/// Approval deadlines are not here: they are policy (`approval_timeout_secs`
/// and `escalate_after_secs` on a tenant, workflow or node policy). The
/// `approval_timeout` key this struct used to declare was never read, and
/// registration refuses a non-null one.
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
        DurationRepr::Struct { secs, nanos } => duration(secs, nanos),
    }
}

/// `Duration::new`, refusing `nanos` of a second or more: carried into
/// `secs`, it overflowed near `u64::MAX` and `Duration::new` panicked, inside
/// the IR parse of `POST /workflows`.
fn duration<E: serde::de::Error>(secs: u64, nanos: u32) -> Result<Duration, E> {
    if nanos >= 1_000_000_000 {
        return Err(E::custom(format!(
            "nanos must be below 1000000000, got {nanos}"
        )));
    }
    Ok(Duration::new(secs, nanos))
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
        Some(OptDurationRepr::Struct { secs, nanos }) => duration(secs, nanos).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `nanos` of a second or more is refused, not carried: near `u64::MAX`
    /// seconds the carry overflowed and `Duration::new` panicked.
    #[test]
    fn nanos_of_a_second_or_more_are_refused() {
        for (secs, nanos) in [
            (u64::MAX, 1_000_000_000u32),
            (1, 1_000_000_000),
            (1, u32::MAX),
        ] {
            let parsed: Result<TimeoutConfig, _> = serde_json::from_value(serde_json::json!({
                "node_timeout": { "secs": secs, "nanos": nanos }
            }));
            assert!(parsed.is_err(), "{secs} {nanos}");
        }
        let parsed: TimeoutConfig = serde_json::from_value(serde_json::json!({
            "node_timeout": { "secs": u64::MAX, "nanos": 999_999_999 }
        }))
        .unwrap();
        assert_eq!(
            parsed.node_timeout,
            Some(Duration::new(u64::MAX, 999_999_999))
        );
    }
}
