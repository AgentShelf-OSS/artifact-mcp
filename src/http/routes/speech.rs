//! Optional, authenticated speech synthesis for selected artifacts.

mod catalog;
mod clients;
mod routes;
mod validation;

pub(crate) use catalog::parse_extra_pocket_voices;
pub(crate) use clients::parse_worker_concurrency;
pub(crate) use routes::router;
