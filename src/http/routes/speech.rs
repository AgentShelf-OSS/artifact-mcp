//! Optional, authenticated speech synthesis for selected artifacts.

mod catalog;
mod clients;
mod routes;
mod validation;

#[allow(unused_imports)]
pub(crate) use catalog::{
    CatalogVoice, ExtraPocketVoice, VoiceTags, parse_extra_pocket_voices, parse_pocket_catalog,
    pocket_catalog,
};
#[allow(unused_imports)]
pub(crate) use clients::parse_worker_concurrency;
pub(crate) use routes::router;
