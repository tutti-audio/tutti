//! The bus names of [`ChannelTopology`], kept while their uses are renamed.

use crate::{ChannelTopology, TopologyError};

/// [`ChannelTopology`], under the name a bus's channels were held by.
pub type BusTopology = ChannelTopology;

/// [`TopologyError`], under the name a too-wide bus was refused by.
pub type BusTooWide = TopologyError;
