//! The cases, one module per subject. Each case is a `pub async fn` taking
//! a [`Fixture`](super::Fixture), and is listed in
//! [`for_each_case!`](crate::for_each_case), which is what runs it.
//!
//! Most modules hold what used to be one file of `jamjet-state`'s
//! integration tests, ported so every backend runs them.

pub mod api_token_hash;
pub mod append_event_if;
pub mod artifacts;
pub mod commit_turn;
pub mod durability;
pub mod event_compat;
pub mod events_page;
pub mod fencing;
pub mod fixture;
pub mod idempotency;
pub mod initiator;
pub mod lease_events;
pub mod lease_semantics;
pub mod memory_backend;
pub mod park;
pub mod projector;
pub mod running_page;
pub mod scheduler_pass;
pub mod segment;
pub mod support;
pub mod tenant_isolation;
pub mod workflow_immutability;
