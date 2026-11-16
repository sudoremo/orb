//! Background services for orb
//!
//! This module contains long-running background services that operate
//! independently of the main TUI event loop.

pub mod health;
pub mod idle;
pub mod network;
pub mod scheduler;

pub use health::{AccountHealth, HealthBook, HealthState};
pub use idle::IdleService;
pub use scheduler::{SyncEvent, SyncScheduler};
