//! An alsa-lib failure: the call and its code.

/// An alsa-lib function this crate calls, named in the errors it raises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    /// `snd_seq_open`.
    Open,
    /// `snd_seq_client_id`.
    ClientId,
    /// `snd_seq_set_client_midi_version`.
    SetClientMidiVersion,
    /// `snd_seq_set_client_ump_conversion`.
    SetClientUmpConversion,
    /// `snd_seq_create_simple_port`.
    CreateSimplePort,
    /// `snd_seq_connect_from`.
    ConnectFrom,
    /// `snd_seq_connect_to`.
    ConnectTo,
    /// `snd_seq_ump_event_input`.
    UmpEventInput,
    /// `snd_seq_ump_event_output_direct`.
    UmpEventOutputDirect,
    /// `snd_seq_event_output_direct`.
    EventOutputDirect,
    /// `snd_seq_get_client_pool`.
    GetClientPool,
    /// `snd_seq_alloc_queue`.
    AllocQueue,
    /// `snd_seq_control_queue`.
    ControlQueue,
    /// `snd_seq_drain_output`.
    DrainOutput,
    /// `snd_seq_get_port_info`.
    GetPortInfo,
    /// `snd_seq_set_port_info`.
    SetPortInfo,
    /// `snd_seq_get_queue_status`.
    GetQueueStatus,
    /// `snd_seq_poll_descriptors_count`.
    PollDescriptorsCount,
    /// `snd_seq_poll_descriptors`.
    PollDescriptors,
    /// `snd_seq_get_ump_endpoint_info`.
    GetUmpEndpointInfo,
    /// `snd_seq_get_ump_block_info`.
    GetUmpBlockInfo,
    /// `snd_seq_set_ump_endpoint_info`.
    SetUmpEndpointInfo,
    /// `snd_seq_set_ump_block_info`.
    SetUmpBlockInfo,
    /// `snd_seq_port_info_malloc`.
    PortInfoMalloc,
    /// `snd_seq_client_info_malloc`.
    ClientInfoMalloc,
    /// `snd_ump_block_info_malloc`.
    UmpBlockInfoMalloc,
    /// `snd_ump_endpoint_info_malloc`.
    UmpEndpointInfoMalloc,
    /// `snd_seq_queue_status_malloc`.
    QueueStatusMalloc,
    /// `snd_seq_client_pool_malloc`.
    ClientPoolMalloc,
}

impl Operation {
    /// The alsa-lib function's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Open => "snd_seq_open",
            Self::ClientId => "snd_seq_client_id",
            Self::SetClientMidiVersion => "snd_seq_set_client_midi_version",
            Self::SetClientUmpConversion => "snd_seq_set_client_ump_conversion",
            Self::CreateSimplePort => "snd_seq_create_simple_port",
            Self::ConnectFrom => "snd_seq_connect_from",
            Self::ConnectTo => "snd_seq_connect_to",
            Self::UmpEventInput => "snd_seq_ump_event_input",
            Self::UmpEventOutputDirect => "snd_seq_ump_event_output_direct",
            Self::EventOutputDirect => "snd_seq_event_output_direct",
            Self::GetClientPool => "snd_seq_get_client_pool",
            Self::AllocQueue => "snd_seq_alloc_queue",
            Self::ControlQueue => "snd_seq_control_queue",
            Self::DrainOutput => "snd_seq_drain_output",
            Self::GetPortInfo => "snd_seq_get_port_info",
            Self::SetPortInfo => "snd_seq_set_port_info",
            Self::GetQueueStatus => "snd_seq_get_queue_status",
            Self::PollDescriptorsCount => "snd_seq_poll_descriptors_count",
            Self::PollDescriptors => "snd_seq_poll_descriptors",
            Self::GetUmpEndpointInfo => "snd_seq_get_ump_endpoint_info",
            Self::GetUmpBlockInfo => "snd_seq_get_ump_block_info",
            Self::SetUmpEndpointInfo => "snd_seq_set_ump_endpoint_info",
            Self::SetUmpBlockInfo => "snd_seq_set_ump_block_info",
            Self::PortInfoMalloc => "snd_seq_port_info_malloc",
            Self::ClientInfoMalloc => "snd_seq_client_info_malloc",
            Self::UmpBlockInfoMalloc => "snd_ump_block_info_malloc",
            Self::UmpEndpointInfoMalloc => "snd_ump_endpoint_info_malloc",
            Self::QueueStatusMalloc => "snd_seq_queue_status_malloc",
            Self::ClientPoolMalloc => "snd_seq_client_pool_malloc",
        }
    }
}

impl std::fmt::Display for Operation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What alsa-lib did not do: a call that returned a negative code, an id that
/// is not an address, or a record it said it made and did not.
///
/// alsa-lib reports failure as a negated code. Nearly all are `errno` values,
/// which `source` carries and prints; the few that are alsa-lib's own
/// (`SND_ERROR_*`, from 500000: `snd_seq_open` answers
/// `SND_ERROR_INCOMPATIBLE_VERSION` for a kernel with another sequencer
/// protocol) are not `errno`s, so [`errno`](Self::errno) hands back the raw code
/// either way and `source` prints those as an unknown OS error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An alsa-lib call returned a negative code.
    #[error("ALSA {operation}: {source}")]
    Call {
        /// The alsa-lib function that failed.
        operation: Operation,
        /// The code, as the OS error it names.
        source: std::io::Error,
    },
    /// A client, port or queue id from alsa-lib that is no address byte, which
    /// is what a sequencer address holds.
    #[error("ALSA {what} {id} is not a sequencer address byte")]
    Address {
        /// What the id names (`"client"`, `"port"`, `"queue"`).
        what: &'static str,
        /// The id alsa-lib gave.
        id: i32,
        /// Why it does not fit a byte.
        #[source]
        source: std::num::TryFromIntError,
    },
    /// An allocation alsa-lib reported as made returned no record.
    #[error("ALSA {operation} made no record")]
    NoRecord {
        /// The alsa-lib function that returned the null.
        operation: Operation,
    },
    /// The sequencer did not give a queue's real time.
    #[error("the sequencer gave no real time for the queue's status")]
    NoQueueTime,
}

/// What this crate's calls return.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// The code a failed call returned, made positive: an `errno` or, for a few
    /// alsa-lib failures, one of alsa-lib's own codes. `None` for a failure
    /// that came from no call.
    #[must_use]
    pub fn errno(&self) -> Option<i32> {
        match self {
            Self::Call { source, .. } => source.raw_os_error(),
            _ => None,
        }
    }
}

/// The address byte of the client, port or queue `id` that `what` names.
///
/// # Errors
///
/// [`Error::Address`] when `id` does not fit a byte.
pub(crate) fn address_byte(what: &'static str, id: i32) -> Result<u8> {
    u8::try_from(id).map_err(|source| Error::Address { what, id, source })
}

/// A return code as a result: failure is negative, anything else is the value.
pub(crate) fn check(code: i32, operation: Operation) -> Result<i32> {
    if code < 0 {
        Err(Error::Call {
            operation,
            source: std::io::Error::from_raw_os_error(code.saturating_neg()),
        })
    } else {
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_negative_code_is_the_failure_and_zero_is_a_value() {
        let failed = check(-2, Operation::Open).unwrap_err();
        assert!(matches!(
            &failed,
            Error::Call { operation: Operation::Open, source }
                if source.kind() == std::io::ErrorKind::NotFound
        ));
        assert_eq!(failed.errno(), Some(2));
        assert_eq!(check(0, Operation::Open).unwrap(), 0);
        assert_eq!(check(7, Operation::Open).unwrap(), 7);
    }

    #[test]
    fn a_code_that_is_no_errno_is_kept_whole() {
        // `SND_ERROR_INCOMPATIBLE_VERSION`, alsa-lib's own code.
        assert_eq!(
            check(-500_000, Operation::Open).unwrap_err().errno(),
            Some(500_000)
        );
    }

    #[test]
    fn a_call_error_names_the_function_and_the_os_error() {
        let text = check(-2, Operation::CreateSimplePort)
            .unwrap_err()
            .to_string();
        assert!(text.contains("snd_seq_create_simple_port"));
        assert!(text.contains("No such file"));
    }

    #[test]
    fn an_id_past_a_byte_is_refused_with_why() {
        use std::error::Error as _;
        assert_eq!(address_byte("port", 254).unwrap(), 254);
        let refused = address_byte("client", 256).unwrap_err();
        assert!(matches!(refused, Error::Address { id: 256, .. }));
        assert!(refused.source().is_some(), "the integer error is kept");
        assert_eq!(
            address_byte("port", -1).unwrap_err().to_string(),
            "ALSA port -1 is not a sequencer address byte"
        );
    }
}
