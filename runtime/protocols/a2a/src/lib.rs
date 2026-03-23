//! A2A protocol integration — re-exports from the published `jamjet-a2a` crate
//! plus the local `ProtocolAdapter` bridge.

pub mod adapter;

// ── Re-export types with backward-compatible aliases ─────────────────────────

pub use jamjet_a2a_types::A2aError;
pub use jamjet_a2a_types::A2aProtocolError;
pub use jamjet_a2a_types::A2aTransportError;
pub use jamjet_a2a_types::Artifact as A2aArtifact;
pub use jamjet_a2a_types::CancelTaskRequest;
pub use jamjet_a2a_types::GetTaskRequest;
pub use jamjet_a2a_types::Message as A2aMessage;
pub use jamjet_a2a_types::Part as A2aPart;
pub use jamjet_a2a_types::PartContent;
pub use jamjet_a2a_types::Role;
pub use jamjet_a2a_types::SendMessageConfiguration;
pub use jamjet_a2a_types::SendMessageRequest;
pub use jamjet_a2a_types::SendMessageResponse;
pub use jamjet_a2a_types::StreamResponse as A2aStreamEvent;
pub use jamjet_a2a_types::Task as A2aTask;
pub use jamjet_a2a_types::TaskArtifactUpdateEvent;
pub use jamjet_a2a_types::TaskState as A2aTaskState;
pub use jamjet_a2a_types::TaskStatus as A2aTaskStatus;
pub use jamjet_a2a_types::TaskStatusUpdateEvent;

/// Backward-compatible alias: the old `SendTaskRequest` maps to the v1.0 `SendMessageRequest`.
pub type SendTaskRequest = jamjet_a2a_types::SendMessageRequest;

// ── Re-export client, server, store, federation from the published crate ─────

pub use jamjet_a2a::client::A2aClient;
pub use jamjet_a2a::federation::{
    build_mtls_client, check_method_scopes, federation_auth_layer, validate_federation_token,
    FederationIdentity, FederationPolicy, FederationToken, TlsConfig,
};
pub use jamjet_a2a::server::{A2aServer, TaskHandler};
pub use jamjet_a2a::store::{InMemoryTaskStore, TaskStore};

// ── Re-export the local adapter ──────────────────────────────────────────────

pub use adapter::A2aAdapter;
