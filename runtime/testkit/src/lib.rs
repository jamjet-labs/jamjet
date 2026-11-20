//! Test helpers for the JamJet engine. Not published.
//!
//! [`intercept::Intercept`] wraps any [`jamjet_state::StateBackend`] and
//! hands every call to a test's [`intercept::Hooks`] before and after it
//! reaches the wrapped backend: a test can fail one call, or hold a call
//! until another component has reached the same point.

pub mod intercept;

pub use intercept::{Hooks, Intercept, NoHooks, Op};
