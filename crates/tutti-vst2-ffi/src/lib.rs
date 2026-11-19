#![doc = include_str!("../README.md")]
// Forked from vst-rs 0.3.0. This is *owned* code, not a read-only vendor drop:
// the fork already diverges deliberately (host-side audioMaster callbacks, plus
// the host-safety fixes documented in `host.rs`), so its lints are chased like
// any other crate here.
//
// The remaining allows are narrow and each has a reason:
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![allow(missing_docs)]
// Elided-lifetime style across a 2021-edition port of a 2018-edition crate.
#![allow(mismatched_lifetime_syntaxes)]

//! The host half of the VST2.4 API: the ABI types, the plugin loader and the host callback.
//!
//! Every VST2 `unsafe` lives here (`cargo xtask check-ffi`).
//!
//! The VST API is multi-threaded. A VST host calls into a plugin generally from two threads -
//! the *processing* thread and the *UI* thread. The organization of this crate reflects this
//! structure to ensure that the threading assumptions of Safe Rust are fulfilled and data
//! races are avoided.
//!
//! # Plugins
//! The plugin-authoring half (the `Plugin` and `PluginParameters` traits, `HostCallback`,
//! `plugin_main!` and the plugin-side dispatcher) lives in the test-only
//! `tutti-vst2-author`; this crate holds the ABI types both sides share. A loaded plugin
//! is a `PluginInstance`, the main-thread handle, and a `RenderHandle`, the audio one.
//!
//! # Hosts
//!
//! ## `Host` Trait
//! All hosts must implement the [`Host` trait](host/trait.Host.html). To load a VST plugin, you
//! need to wrap your host in an `Arc<T>` wrapper. Along with the
//! plugin path, this can be passed to the [`PluginLoader::load`] method to create a plugin loader
//! which can spawn plugin instances.
//!
//! ## Example Host
//! ```no_run
//! extern crate tutti_vst2_ffi as vst;
//!
//! use std::sync::Arc;
//! use std::path::Path;
//!
//! use tutti_vst2_ffi::host::{Host, PluginLoader};
//!
//! struct SampleHost;
//!
//! impl Host for SampleHost {
//!     fn automate(&self, index: i32, value: f32) {
//!         println!("Parameter {} had its value changed to {}", index, value);
//!     }
//! }
//!
//! fn main() {
//!     let host = Arc::new(SampleHost);
//!     let path = Path::new("/path/to/vst");
//!
//!     let mut loader = PluginLoader::load(path, host.clone()).unwrap();
//!     let main = tutti_plugin_types::MainThreadMarker::new().unwrap();
//!     let (_collector, work) = tutti_plugin_types::MainWork::new();
//!     let mut instance = loader.instance(main, &work).unwrap();
//!
//!     println!("Loaded {}", instance.get_info().name);
//!
//!     instance.init(main);
//!     println!("Initialized instance!");
//!
//!     println!("Closing instance...");
//!     // Not necessary as the instance is shut down when it goes out of scope anyway.
//!     // drop(instance);
//! }
//!
//! ```
//!
//! [`PluginLoader::load`]: host/struct.PluginLoader.html#method.load
//!

extern crate libc;
extern crate libloading;
extern crate num_traits;
#[macro_use]
extern crate tracing;
#[macro_use]
extern crate bitflags;

#[macro_use]
mod primitive_enum;

pub mod api;
pub mod buffer;
pub mod editor;
pub mod host;
pub mod interfaces;
pub mod midi;
pub mod plugin;
pub mod scratch;
pub mod speaker;

pub use primitive_enum::UnknownPrimitive;

// Test-only global allocator for the RT-safety test of the render refusal; it aborts on
// any allocation inside an `assert_no_alloc` scope.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

#[cfg(test)]
mod primitive_enum_tests {
    use crate::UnknownPrimitive;

    /// A value no variant has is refused and named, in every repr: the
    /// dispatchers rely on the refusal to skip opcodes they do not know.
    #[test]
    fn a_value_no_variant_has_is_refused_with_that_value() {
        assert_eq!(
            crate::plugin::OpCode::try_from(i32::MAX).err(),
            Some(UnknownPrimitive(i64::from(i32::MAX)))
        );
        assert_eq!(
            crate::api::ProcessLevel::try_from(-1).err(),
            Some(UnknownPrimitive(-1))
        );
        assert_eq!(
            crate::plugin::Category::try_from(isize::MIN)
                .err()
                .map(UnknownPrimitive::get),
            Some(isize::MIN as i64)
        );
    }
}
