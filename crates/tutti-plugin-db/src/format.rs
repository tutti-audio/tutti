//! The plugin format, and the catalog-identity types re-exported beside it.

// Catalog-identity types, defined in `tutti-plugin-types` so the format host
// crates can name them without depending on `tutti-plugin`.
pub use tutti_plugin_types::{AuComponentType, PluginClass, PluginDescriptor};

// The classification vocabularies each format reports, and the normalized
// `PluginRole` derived from all four. Defined in `tutti-plugin-types` so the
// persisted database stays nameable with no format feature enabled.
pub use tutti_plugin_types::Vst2Category;

// The format is the wire's too (a load names it to the server), so it is
// `tutti-plugin-types`'.
pub use tutti_plugin_types::{
    ClapFeature, PluginFormat, PluginRole, Vst3PlugType, Vst3SubCategories,
};
