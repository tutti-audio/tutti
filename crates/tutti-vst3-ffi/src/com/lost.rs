//! A count of what a COM object lost, written from any thread and read from
//! any thread.

use tutti_rt::{Tally, TallyWriter, tally};

/// A loss count: the reader and the stored writer of one [`Tally`], minted
/// together off the audio thread. Counting is a `fetch_add`: no allocation, no
/// lock, no wait, and it needs no borrow of the object it counts for, so a COM
/// entry that finds its object in use still counts its refusal.
#[derive(Clone)]
pub(crate) struct Lost {
    reader: Tally<u64>,
    writer: TallyWriter<u64>,
}

impl Lost {
    /// A zero count. Allocates.
    pub(crate) fn new() -> Self {
        let reader = tally();
        let writer = reader.writer();
        Self { reader, writer }
    }

    /// Counts `by` more.
    pub(crate) fn add(&self, by: u64) {
        self.writer.add(by);
    }

    /// The total counted.
    pub(crate) fn get(&self) -> u64 {
        self.reader.get()
    }
}
