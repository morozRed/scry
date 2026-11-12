//! Scry's engine: reads the machine into a `Snapshot` and turns it into view models.

pub mod attribution;
pub mod catalog;
pub mod claude_hooks;
pub mod config;
pub mod control;
pub mod daemon;
pub mod daemon_log;
pub mod doctor;
pub mod engine;
pub mod fixture;
pub mod git;
pub mod home;
pub mod keep;
pub mod label;
pub mod launch_agent;
pub mod live;
pub mod model;
pub mod paths;
pub mod project;
pub mod protected;
pub mod reap;
pub mod recorder;
pub mod session;
pub mod size;
pub mod spool;
pub mod store;
pub mod sys;
pub mod view;
pub mod worktree;

pub use model::{ProcId, Process, Snapshot};
