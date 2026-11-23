//! Test helpers for the JamJet engine. Not published.
//!
//! - [`intercept::Intercept`] wraps any [`jamjet_state::StateBackend`] and
//!   hands every call to a test's [`intercept::Hooks`] before and after it
//!   reaches the wrapped backend: a test can fail one call, or hold a call
//!   until another component has reached the same point.
//! - [`conformance`] holds one behaviour suite for every backend: a fixture
//!   per backend, the cases, and [`conformance_suite!`], which runs every
//!   case on one backend.
//! - [`pg::TestDatabase`] gives a test a Postgres database of its own.
//! - [`invariants::check`] reads a store's database and names every rule
//!   of the engine's log, queue and audit chain it breaks.

pub mod conformance;
pub mod intercept;
pub mod invariants;
pub mod pg;

pub use intercept::{Hooks, Intercept, NoHooks, Op};
pub use pg::skip;
