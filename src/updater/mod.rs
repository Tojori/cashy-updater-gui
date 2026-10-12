//! Anbindung an das installierte `cachy-update` (arch-update).

pub mod progress;
pub mod runner;
pub mod service;
pub mod session;
pub mod state_files;
pub mod status;
pub mod system;

pub use runner::{CancelToken, CommandRunner, SystemRunner};
pub use service::{
    Backend, CheckReport, NewsItem, ServiceConfig, StatusSnapshot, ToolInfo, UpdaterService,
    UpgradeOptions,
};
pub use session::{SessionEvent, SessionSpec, UpgradeSession};
pub use state_files::{PendingUpdate, UpdateSource};
pub use status::{Outcome, OutcomeKind, Prompt, Step, StepStatus, UpdatePhase, UpgradeTracker};
