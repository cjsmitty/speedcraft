//! HTTP control plane. The same server runs as the local Mac UI
//! (`speedcraft ui`) and as the headless Linux agent (`speedcraft agent`).
//! The UI host additionally keeps a registry of remote agents and proxies
//! requests to them, so one browser tab controls every engine.

pub mod agents;
pub mod api;
pub mod settings;
pub mod supervisor;

pub use api::{router, AppState};
pub use supervisor::Supervisor;
