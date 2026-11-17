//! Steps of the measurements allocate nothing once their state is built.
//!
//! The crate carries no `assert_no_alloc` dev-dependency (see its Cargo.toml),
//! so this binary counts allocations with its own global allocator. It counts
//! per thread, so the harness's other threads do not show up in a count.

mod chroma;
mod spectrum;
mod windowed_loudness;

/// An `out` that drops every item: pushing into it allocates nothing.
struct Discard;

impl<T> Extend<T> for Discard {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        items.into_iter().for_each(drop);
    }
}

#[global_allocator]
static ALLOC: tutti_fixtures::ThreadCounting = tutti_fixtures::ThreadCounting;
