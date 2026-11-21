//! A saved device reads back as the device it was, and the document's shape is
//! the one apps persist: renaming a variant or a field breaks saved files, so
//! the text is pinned for every `OsEndpointIdentity`. Run with `--features serde`.
#![cfg(feature = "serde")]

mod device;
mod endpoint;
mod learned;
mod product_id;
