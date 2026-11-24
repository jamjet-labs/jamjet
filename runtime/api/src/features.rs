//! What this engine can do that an older one could not, as `GET /health`
//! reports it. Clients check it before sending something an older engine
//! would ignore or refuse.

/// Sorted, unique, `[a-z0-9_]+`. A name is added only with a guard arm in
/// `tests/health.rs` that exercises it.
pub const FEATURES: &[&str] = &[
    "claim_by_execution",
    "ops_tick",
    "python_fn_by_execution",
    "run_key",
    "worker_model_steps",
];
