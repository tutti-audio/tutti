//! The AU format's server side: it differs from [`ServerSide`]'s defaults in
//! nothing. The module is declared where AU is served (`served_au`, defined by
//! `build.rs`), so no item below carries a `cfg`.

use super::ServerSide;

impl ServerSide for tutti_au_host::AuLoaded {}
