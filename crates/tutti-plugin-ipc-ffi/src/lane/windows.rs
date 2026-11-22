//! The audio lane on Windows: a fresh named pipe, named in the frame.

use crate::SlabError;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// The start of every lane name this module makes.
const NAME_PREFIX: &str = "tutti_lane-";

/// What `OpenLane` carries of a lane: the name of the pipe the server listens on,
/// checked as it is decoded (a name this module could not have made does not
/// decode), so a peer cannot point the server at another pipe.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "WireName")]
pub struct LaneWire(String);

/// A name as it travels, unchecked until it becomes a [`LaneWire`]. Its field is
/// `pub` because `check-range-types` flags a private field under a plain
/// `Deserialize`.
#[derive(Deserialize)]
struct WireName(pub String);

impl TryFrom<WireName> for LaneWire {
    type Error = String;

    fn try_from(WireName(name): WireName) -> Result<Self, String> {
        // Exactly what `fresh` writes: the prefix, a decimal pid, a decimal
        // count and sixteen lowercase hex digits, joined by `-`.
        let decimal = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let spelled = name.strip_prefix(NAME_PREFIX).is_some_and(|rest| {
            let mut parts = rest.split('-');
            matches!(
                (parts.next(), parts.next(), parts.next(), parts.next()),
                (Some(pid), Some(count), Some(hex), None)
                    if decimal(pid)
                        && decimal(count)
                        && hex.len() == 16
                        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            )
        });
        if spelled {
            Ok(Self(name))
        } else {
            Err(format!("{name:?} is not a lane name"))
        }
    }
}

impl LaneWire {
    /// A fresh name: the process id, a per-process count and sixty-four random
    /// bits, so no two lanes this process makes meet and a peer cannot guess it.
    ///
    /// # Errors
    ///
    /// The operating system's random source cannot be read.
    pub fn fresh() -> Result<Self, SlabError> {
        static COUNT: AtomicU64 = AtomicU64::new(0);
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(SlabError::Name)?;
        Ok(Self(format!(
            "{NAME_PREFIX}{}-{}-{:016x}",
            std::process::id(),
            COUNT.fetch_add(1, Ordering::Relaxed),
            u64::from_ne_bytes(bytes),
        )))
    }

    /// The pipe's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name decoded from a peer's bytes is one `fresh` could have made: a foreign
    /// name, a path, or a malformed count does not decode.
    #[test]
    fn a_decoded_name_must_be_one_fresh_could_have_made() {
        let decode = |name: &str| LaneWire::try_from(WireName(name.to_owned()));
        let made = LaneWire::fresh().unwrap();
        assert_eq!(decode(made.name()).unwrap(), made);
        for bad in [
            "",
            "tutti_lane-",
            "..\\pipe\\x",
            "tutti_lane-/../x",
            "tutti_plugin-1-2-0123456789abcdef",
            "tutti_lane-1-2-0123456789abcde",
            "tutti_lane-1-2-0123456789ABCDEF",
            "tutti_lane-1-0123456789abcdef",
            "tutti_lane-x-2-0123456789abcdef",
            "tutti_lane-1-2-0123456789abcdef-3",
        ] {
            assert!(decode(bad).is_err(), "{bad:?} decoded");
        }
    }
}
