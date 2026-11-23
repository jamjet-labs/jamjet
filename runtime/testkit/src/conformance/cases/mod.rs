//! The cases, one module per subject. Each case is a `pub async fn` taking
//! a [`Fixture`](super::Fixture), and is listed in
//! [`for_each_case!`](crate::for_each_case), which is what runs it.
//!
//! Most modules hold what used to be one file of `jamjet-state`'s
//! integration tests, ported so every backend runs them. `concurrency`,
//! `payloads`, `terminal` and `tenancy` are written for the suite: what two
//! engines on one store, large or unusual values, a finished run and a
//! tenant's scope must do on every backend. `run_keys` checks that a run
//! key names one run, in its tenant only. `ticks` checks the one tick
//! lease a store keeps for engines run with their loops off. `faults` stops
//! each of the store's transactions between two statements.

pub mod api_token_hash;
pub mod append_event_if;
pub mod artifacts;
pub mod commit_turn;
pub mod concurrency;
pub mod durability;
pub mod event_compat;
pub mod events_page;
pub mod faults;
pub mod fencing;
pub mod fixture;
pub mod idempotency;
pub mod initiator;
pub mod lease_events;
pub mod lease_semantics;
pub mod memory_backend;
pub mod park;
pub mod payloads;
pub mod projector;
pub mod run_keys;
pub mod running_page;
pub mod scheduler_pass;
pub mod segment;
pub mod support;
pub mod tenancy;
pub mod tenant_isolation;
pub mod terminal;
pub mod ticks;
pub mod workflow_immutability;
