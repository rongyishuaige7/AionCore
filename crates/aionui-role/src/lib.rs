#![warn(clippy::disallowed_types)]

//! Resident roles: an assistant promoted to a standing team member.
//!
//! A role adds three things to the assistant it is built on: a charter (what it
//! owns and what it refuses), a memory that outlives any single team, and a
//! table of team tools it may not call. Everything lives as plain files under
//! `<data_dir>/roles/<assistant_id>/`, so the memory stays readable and
//! editable by the user and by agents without going through the database:
//!
//! ```text
//! roles/<assistant_id>/role.json          charter + permission table
//! roles/<assistant_id>/memory/MEMORY.md   the role's memory
//! ```
//!
//! The team runtime consumes roles through `aionui_team::TeamRolePort`; the
//! composition layer adapts [`RoleService`] to that port.

mod error;
mod service;
mod state;
mod store;

pub mod routes;

pub use error::RoleError;
pub use routes::role_routes;
pub use service::{MAX_CHARTER_CHARS, MAX_MEMORY_BYTES, RoleService};
pub use state::RoleRouterState;
