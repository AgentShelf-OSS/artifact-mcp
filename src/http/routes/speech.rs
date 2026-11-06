//! Optional, authenticated speech synthesis for selected artifacts.

mod catalog;
mod clients;
mod routes;
mod validation;

pub(crate) use routes::router;
