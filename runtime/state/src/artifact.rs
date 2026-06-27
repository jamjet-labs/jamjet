//! Content-addressed artifact store helpers.
//!
//! Large `serde_json::Value` payloads (model outputs, large state blobs) that
//! exceed a configurable byte threshold are "spilled" to the artifact store
//! and replaced inline by a small `ArtifactRef` sentinel value. Readers call
//! `resolve_value` to restore the original.
//!
//! The sentinel shape is:
//! ```json
//! { "$artifact": { "hash": "...", "size": 16384, "media_type": "application/json" } }
//! ```
//!
//! The sentinel key `$artifact` is reserved — it must never appear as a real
//! workflow state key. In practice this is a trivially safe constraint because
//! workflow state keys come from user-defined YAML node names.

use crate::backend::{BackendResult, StateBackend, StateBackendError};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Sentinel key used to distinguish an ArtifactRef from real data.
pub const ARTIFACT_SENTINEL_KEY: &str = "$artifact";

/// Default spill threshold: 8 KiB. Values whose serialized JSON byte length
/// exceeds this are written to the artifact store and replaced by a sentinel.
/// Override at runtime via the `JAMJET_ARTIFACT_THRESHOLD_BYTES` env var.
pub const DEFAULT_SPILL_THRESHOLD: usize = 8 * 1024;

/// A reference to a content-addressed artifact stored out-of-band.
///
/// Replaces the inline `serde_json::Value` in events and state when the value
/// exceeds the spill threshold. `hash` is the SHA-256 hex of the stored bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    /// SHA-256 hex of the stored bytes (64 lowercase hex chars).
    pub hash: String,
    /// Byte length of the stored content.
    pub size: u64,
    /// Optional MIME type (e.g. `"application/json"`, `"text/plain"`).
    pub media_type: Option<String>,
}

impl ArtifactRef {
    /// Serialize this ref into the sentinel JSON value that replaces the
    /// original payload inline.
    pub fn to_sentinel(&self) -> Value {
        json!({
            ARTIFACT_SENTINEL_KEY: {
                "hash": self.hash,
                "size": self.size,
                "media_type": self.media_type,
            }
        })
    }

    /// Parse an `ArtifactRef` from a sentinel value, or return `None` if the
    /// value is not a sentinel.
    pub fn from_sentinel(v: &Value) -> Option<ArtifactRef> {
        let inner = v.as_object()?.get(ARTIFACT_SENTINEL_KEY)?;
        let hash = inner.get("hash")?.as_str()?.to_string();
        let size = inner.get("size")?.as_u64()?;
        let media_type = inner
            .get("media_type")
            .and_then(|m| m.as_str())
            .map(|s| s.to_string());
        Some(ArtifactRef {
            hash,
            size,
            media_type,
        })
    }
}

/// If `value` serializes to more than `threshold` bytes, return `Some(bytes)`
/// (the raw JSON bytes to put in the artifact store). Otherwise return `None`
/// (the value is small enough to stay inline).
///
/// The caller is expected to:
/// 1. Call `spill_bytes` to check if spilling is needed and get the bytes.
/// 2. Call `backend.put_artifact(&bytes, media_type)` to get an `ArtifactRef`.
/// 3. Replace the original value with `artifact_ref.to_sentinel()`.
pub fn spill_bytes(value: &Value, threshold: usize) -> Option<Vec<u8>> {
    let bytes = serde_json::to_vec(value).ok()?;
    if bytes.len() > threshold {
        Some(bytes)
    } else {
        None
    }
}

/// Resolve a value: if it is a `{"$artifact": {...}}` sentinel, fetch the
/// stored bytes from the backend and deserialize back to the original `Value`.
/// Otherwise return it unchanged.
///
/// One-level walk: if the top-level value is not a sentinel but is an object
/// or array, resolve any sentinel values one level deep. This is sufficient for
/// the current spill model where the entire node output is spilled as one ref.
///
/// A missing artifact (dangling ref) returns a `StateBackendError::NotFound`
/// rather than panicking.
pub async fn resolve_value(value: &Value, backend: &dyn StateBackend) -> BackendResult<Value> {
    // Top-level sentinel?
    if let Some(artifact_ref) = ArtifactRef::from_sentinel(value) {
        return resolve_ref(&artifact_ref, backend).await;
    }

    // Walk one level into objects.
    if let Some(obj) = value.as_object() {
        let mut out = serde_json::Map::with_capacity(obj.len());
        for (k, v) in obj {
            if let Some(artifact_ref) = ArtifactRef::from_sentinel(v) {
                out.insert(k.clone(), resolve_ref(&artifact_ref, backend).await?);
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
        return Ok(Value::Object(out));
    }

    // Walk one level into arrays.
    if let Some(arr) = value.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            if let Some(artifact_ref) = ArtifactRef::from_sentinel(v) {
                out.push(resolve_ref(&artifact_ref, backend).await?);
            } else {
                out.push(v.clone());
            }
        }
        return Ok(Value::Array(out));
    }

    Ok(value.clone())
}

async fn resolve_ref(
    artifact_ref: &ArtifactRef,
    backend: &dyn StateBackend,
) -> BackendResult<Value> {
    match backend.get_artifact(&artifact_ref.hash).await? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(StateBackendError::Serialization),
        None => Err(StateBackendError::NotFound(format!(
            "artifact {} not found (dangling ref)",
            artifact_ref.hash
        ))),
    }
}
