//! Routing: hostname to target, target to backend, and the health state that
//! keeps dead backends out of the answer.

pub mod health;
pub mod matcher;
pub mod registry;

pub use matcher::{Matcher, RouteMatch};
pub use registry::{Backend, Registry, Route, SelectError, SessionGuard, StatusSnapshot, Target};
