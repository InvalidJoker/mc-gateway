//! mc-gateway: adds a line to the MOTD of every Minecraft server on a hosting
//! node, and is otherwise invisible.
//!
//! A firewall rule hands new connections on a port range to the gateway. For a
//! status ping it asks the server, replaces a MOTD line and passes the answer
//! back; every other connection is piped through untouched. Servers see the
//! player's address, and when the gateway is not running, connections go
//! straight to the servers.

pub mod chat;
pub mod config;
pub mod intercept;
pub mod motd;
pub mod netsetup;
pub mod observe;
pub mod protocol;
pub mod server;
pub mod transparent;
