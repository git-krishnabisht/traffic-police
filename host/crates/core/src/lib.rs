//! Event model, session store, derived views and body decoders for traffic-police.
//!
//! Nothing here touches a terminal or a socket: backends turn their input into
//! [`event::SessionEvent`]s, the [`store::SessionStore`] applies them, and the UI reads the
//! store (ARCHITECTURE.md §5).

pub mod backend;
pub mod decode;
pub mod diff;
pub mod event;
pub mod export;
pub mod filter;
pub mod fmt;
pub mod import;
pub mod jq;
pub mod model;
pub mod normalize;
pub mod phases;
pub mod project;
pub mod rows;
pub mod rules;
pub mod session;
pub mod store;
pub mod values;

pub use event::SessionEvent;
pub use fmt::Ts;
pub use store::SessionStore;
