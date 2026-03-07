//! JamJet A2A Client and Server
//!
//! Implements the Agent-to-Agent (A2A) protocol as both client and server.
//!
//! Client: discover remote agents via Agent Cards, submit tasks, stream results.
//! Server: publish Agent Card, accept tasks, manage task lifecycle.

pub mod adapter;
pub mod client;
pub mod server;
pub mod types;

pub use adapter::A2aAdapter;
pub use client::A2aClient;
pub use server::{A2aServer, TaskHandler, TaskStore};
pub use types::*;
