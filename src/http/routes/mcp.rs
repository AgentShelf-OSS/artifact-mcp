//! Authenticated POST/OPTIONS `/mcp` transport.

mod admission;
mod routes;
mod transport;

pub(crate) use routes::router;
