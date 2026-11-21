//! Streamer thread command enum and the stream's identity.

use tutti_rt::RingFloat;

use super::record::Record;

tutti_rt::id! {
    /// One open stream's identity: minted on the control side when the open is
    /// asked for ([`DiskStreamer::open`](crate::DiskStreamer::open)), never
    /// reused, and what every command about the stream names. A host is
    /// handed one ([`StreamHandle::id`](crate::StreamHandle::id),
    /// [`StreamReader::id`](crate::StreamReader::id)); a number it makes up
    /// names no open stream, which the streamer ignores.
    pub struct StreamId;
}

/// Command sent to the Streamer thread.
#[derive(Debug)]
pub(crate) enum StreamerCommand<F: RingFloat> {
    /// Serve a stream the control side built
    /// ([`DiskStreamer::adopt`](crate::DiskStreamer::adopt)): its record joins the
    /// table, live.
    Adopt(Box<Record<F>>),
    /// End a stream: its record, and with it its ring and region, goes in the
    /// step that handles this. Sent by `Drop for StreamHandle`. A stream the
    /// streamer no longer holds ignores it.
    Close { stream: StreamId },

    /// The session rate moved (`DiskStreamer::set_sample_rate` stored it before
    /// sending this): re-derive every open stream's conversion ratio against
    /// the rate as it now is.
    SessionRate,
}
