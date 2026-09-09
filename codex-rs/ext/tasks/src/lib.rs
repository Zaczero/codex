//! Extension crate for the work ledger: the `ledger` tool, its World State section, and the
//! native completion enforcement.

mod completion;
mod control;
mod details;
mod extension;
pub mod ledger;
mod presentation;
mod spec;
pub mod storage;
mod tool;
mod world_state;

pub use control::parallelism;
pub use control::read;
pub use extension::TasksExtensionConfig;
pub use extension::install;
pub use presentation::open_tasks;
pub use spec::LEDGER_TOOL_NAME;
