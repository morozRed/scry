//! Scry's engine: reads the machine into a `Snapshot` and turns it into view models.

pub mod config;
pub mod control;
pub mod daemon;
pub mod daemon_log;
pub mod doctor;
pub mod engine;
pub mod fixture;
pub mod label;
pub mod launch_agent;
pub mod model;
pub mod paths;
pub mod project;
pub mod store;
pub mod sys;
pub mod view;

pub use model::{ProcId, Process, Snapshot};
