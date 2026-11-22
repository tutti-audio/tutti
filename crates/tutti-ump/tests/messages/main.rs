//! Messages and their fields: the codec, the spec tables and the checked types.

/// Inert outside `assert_no_alloc`, so every test here may also gate a
/// closure on allocating nothing.
#[global_allocator]
static A: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod common;

mod articulation;
mod bipolar;
mod by_value;
mod constructors;
mod fields;
mod flex_setup;
mod mono_mode;
mod mpe_zone;
mod no_alloc;
mod normalized;
mod note_id;
mod note_key_event;
mod note_number_pitch;
mod note_velocity;
mod orchestral;
mod properties;
mod reserved;
mod serde;
mod spec_tables;
mod spec_words;
mod value_types;
