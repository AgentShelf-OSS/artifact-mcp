//! Authenticated POST/OPTIONS `/mcp` transport.

mod admission;
mod routes;

pub(crate) use routes::router;
