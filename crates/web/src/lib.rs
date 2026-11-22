//! Web UI and HTTP/WebSocket API, built into the scanner binary (Phase 3).
//!
//! See PLAN.md 3.5a (API surface, auth and roles) and 3.5b (live listening and
//! the uplink budget). Nothing is implemented yet; this crate exists so the
//! workspace layout matches the plan from the start.

/// Roles recognised by the API. Each route declares the minimum role it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Anonymous,
    Admin,
}
