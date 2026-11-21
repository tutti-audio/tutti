//! The probe's result file: the wire between `tutti-plugin-probe` and the host
//! that spawned it.
//!
//! The probe writes its outcome to `<result>.tmp` and renames it to `<result>`,
//! so the final path holds nothing or a whole result, never a prefix of one. The
//! payload is framed (a magic, a length, the bytes) as well, so a file that is
//! cut short some other way is refused rather than decoded as far as it goes.
//!
//! Present means finished: a descriptor, or a typed failure. Absent means the
//! probe did not finish (a crash or a hang), which the reader tells from the
//! process, not from the file.

use crate::{Described, ProbeRefusal, ResultError, Unmeasured};
use std::ffi::OsString;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Leads every result file: [`TAG`], then the format version.
const TAG: [u8; 4] = *b"TPRB";

/// The format version, the byte after [`TAG`]. **Bump it in the same change as
/// any change to what [`ProbeOutcome`] holds** (a field, a variant, a type
/// inside a descriptor): postcard has no field names and no defaults, so a
/// reader of another shape decodes garbage or fails far from the cause. A
/// version mismatch is [`ResultError::ProtocolMismatch`] and not a decode
/// error. Nothing derives the version from the types; a test that pinned the
/// bytes of a value would pin tutti's own output, which is no spec.
const VERSION: u8 = 8;

/// Magic, the version, then the payload length as a little-endian `u64`.
const HEADER: usize = TAG.len() + 1 + 8;

/// What a probe that finished found.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub enum ProbeOutcome {
    /// The file loaded far enough to read the descriptor of every plugin it holds,
    /// in the file's own order: the plugin of most files, each plugin of a CLAP
    /// factory, each audio class of a VST3 factory, each unit an AU bundle
    /// declares, each sub-plugin of a VST2 shell. Never empty: a file that holds
    /// no plugin is a [`Failed`](Self::Failed).
    Described {
        /// The descriptors.
        plugins: Vec<Described>,
        /// The plugins a measuring probe could not measure, and why; empty when
        /// it did not measure. Such a plugin's descriptor has no
        /// [`measured`](crate::PluginDescriptor::measured).
        unmeasured: Vec<Unmeasured>,
    },
    /// The probe ran to its end and refused the plugin.
    Failed(ProbeRefusal),
}

/// The bytes of a result file for `outcome`.
///
/// # Panics
///
/// Never in practice: a [`ProbeOutcome`] holds strings, numbers and enums, which
/// postcard writes to a `Vec` without failing.
pub(crate) fn encode_result(outcome: &ProbeOutcome) -> Vec<u8> {
    let payload = postcard::to_allocvec(outcome).expect("a ProbeOutcome serializes to a Vec");
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(&TAG);
    bytes.push(VERSION);
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes
}

/// The outcome in `bytes`, which must be a whole result file.
///
/// # Errors
///
/// [`ResultError::Truncated`] if the file is cut short, [`ResultError::BadMagic`]
/// if it is not a result, [`ResultError::ProtocolMismatch`] if it is a result of
/// another version, [`ResultError::Malformed`] if the payload does not
/// decode, and [`ResultError::TrailingFile`] or [`ResultError::TrailingPayload`] if
/// bytes follow it.
///
/// # Panics
///
/// Never: the eight-byte slice converts to an array by construction.
pub(crate) fn decode_result(bytes: &[u8]) -> Result<ProbeOutcome, ResultError> {
    let Some((tag, rest)) = bytes.split_at_checked(TAG.len()) else {
        return Err(ResultError::Truncated);
    };
    if tag != TAG {
        return Err(ResultError::BadMagic);
    }
    let Some((&version, rest)) = rest.split_first() else {
        return Err(ResultError::Truncated);
    };
    if version != VERSION {
        return Err(ResultError::ProtocolMismatch {
            expected: VERSION,
            found: version,
        });
    }
    let Some((len, payload)) = rest.split_at_checked(8) else {
        return Err(ResultError::Truncated);
    };
    let len = u64::from_le_bytes(len.try_into().expect("eight bytes"));
    if (payload.len() as u64) < len {
        return Err(ResultError::Truncated);
    }
    if (payload.len() as u64) > len {
        return Err(ResultError::TrailingFile);
    }
    match postcard::take_from_bytes(payload) {
        Ok((outcome, [])) => Ok(outcome),
        Ok(_) => Err(ResultError::TrailingPayload),
        Err(e) => Err(ResultError::Malformed(e)),
    }
}

/// Where `write_result` stages `result` before it renames it into place.
pub(crate) fn result_tmp_path(result: &Path) -> PathBuf {
    let mut name: OsString = result.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Writes `outcome` to `result`: to `<result>.tmp` first, flushed to disk, then
/// renamed over `result`, so a reader sees all of it or none.
///
/// The staging file is made with `create_new`, so a file or a symlink already at
/// that name is an error and is never written through; the host gives the probe
/// a directory of its own for that reason.
///
/// # Errors
///
/// Any I/O error from the write or the rename (the staging file is removed
/// unless it was not ours to begin with, `AlreadyExists`).
pub fn write_result(result: &Path, outcome: &ProbeOutcome) -> io::Result<()> {
    let tmp = result_tmp_path(result);
    let staged = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&encode_result(outcome))?;
        file.sync_all()
    })();
    let moved = staged.and_then(|()| std::fs::rename(&tmp, result));
    if moved
        .as_ref()
        .is_err_and(|e| e.kind() != io::ErrorKind::AlreadyExists)
    {
        let _ = std::fs::remove_file(&tmp);
    }
    moved
}

/// The outcome at `result`, or `None` if the probe left no file there. The
/// staging file is never looked at.
///
/// # Errors
///
/// [`ResultError`] if a file is there and is not a whole result.
pub fn read_result(result: &Path) -> Result<Option<ProbeOutcome>, ResultError> {
    match std::fs::read(result) {
        Ok(bytes) => decode_result(&bytes).map(Some),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ResultError::Io(e.kind())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MeasureFailure, PluginClass, PluginDescriptor, PluginId, Widths};

    fn described() -> ProbeOutcome {
        let plugin = |id: &str, widths| Described {
            id: Some(PluginId::Clap(id.into())),
            descriptor: PluginDescriptor {
                widths,
                ..PluginDescriptor::new(id, PluginClass::Unknown)
            },
        };
        ProbeOutcome::Described {
            plugins: vec![plugin("test", Some(Widths::BOTH)), plugin("other", None)],
            unmeasured: vec![Unmeasured {
                plugin: PluginId::Clap("other".into()),
                failure: MeasureFailure::NoAudioOutput,
            }],
        }
    }

    /// A file already at the staging name is refused, never written through
    /// (the symlink a neighbour planted in a shared directory), and is left alone.
    #[cfg(unix)]
    #[test]
    fn a_planted_staging_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"mine").unwrap();
        std::os::unix::fs::symlink(&victim, result_tmp_path(&path)).unwrap();
        let err = write_result(&path, &described()).expect_err("must refuse");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&victim).unwrap(), b"mine");
    }

    /// Both outcomes survive the file.
    #[test]
    fn an_outcome_round_trips_through_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        for outcome in [
            described(),
            ProbeOutcome::Failed(ProbeRefusal::Describe("no factory".into())),
        ] {
            write_result(&path, &outcome).unwrap();
            assert_eq!(read_result(&path).unwrap(), Some(outcome));
        }
    }

    /// No file is `None`, and the staging file is not a result.
    #[test]
    fn only_the_final_path_is_a_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        assert_eq!(read_result(&path), Ok(None));
        std::fs::write(result_tmp_path(&path), encode_result(&described())).unwrap();
        assert_eq!(read_result(&path), Ok(None), "a staged result is not one");
    }

    /// Every proper prefix of a result is refused, none decodes.
    #[test]
    fn a_cut_short_file_is_never_a_result() {
        let bytes = encode_result(&described());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result");
        std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
        assert_eq!(read_result(&path), Err(ResultError::Truncated));
        for cut in 0..bytes.len() {
            let r = decode_result(&bytes[..cut]);
            assert!(
                matches!(r, Err(ResultError::Truncated | ResultError::BadMagic)),
                "a prefix of {cut} bytes decoded as {r:?}"
            );
        }
        assert_eq!(decode_result(&bytes), Ok(described()));
    }

    /// Bytes that are not a result are refused by name.
    #[test]
    fn foreign_bytes_are_refused() {
        assert_eq!(
            decode_result(b"garbage!garbage!"),
            Err(ResultError::BadMagic)
        );
        let mut extra = encode_result(&described());
        extra.push(0);
        assert_eq!(decode_result(&extra), Err(ResultError::TrailingFile));
    }

    /// A payload whose declared length covers bytes after the outcome is
    /// refused, not decoded from its front: the outcome is the whole payload.
    #[test]
    fn a_payload_with_bytes_after_the_outcome_is_refused() {
        let mut payload = postcard::to_allocvec(&described()).unwrap();
        payload.push(0);
        let mut bytes = TAG.to_vec();
        bytes.push(VERSION);
        bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&payload);
        assert_eq!(decode_result(&bytes), Err(ResultError::TrailingPayload));
    }

    /// **A result of another format version is a version mismatch, not a bad
    /// file**: it names both versions, so a host and a probe of different builds
    /// read as that and not as a corrupt result.
    #[test]
    fn a_result_of_another_version_is_a_protocol_mismatch() {
        for found in [VERSION - 1, VERSION + 1, 0] {
            let mut other = encode_result(&described());
            other[TAG.len()] = found;
            assert_eq!(
                decode_result(&other),
                Err(ResultError::ProtocolMismatch {
                    expected: VERSION,
                    found
                })
            );
        }
        assert_eq!(decode_result(&TAG), Err(ResultError::Truncated));
    }

    /// **A measuring probe's figures survive the file**: the descriptor's
    /// `measured` is a payload of postcard, which has no defaults, so a reader
    /// of the same version sees every verdict a probe can write.
    #[test]
    fn a_measurement_survives_the_result_file() {
        use crate::{
            LatencyVerdict, MaxBlock, Measured, NotChecked, PROBE_FLOOR, ProbeConfig, SampleRate,
            Samples, StateHash, TailVerdict, Verdict,
        };
        let measured = |tail, latency| Measured {
            tail: Some(Samples::new(8191)),
            latency: None,
            config: ProbeConfig {
                rate: SampleRate::SR_44K1,
                block: MaxBlock::new(Samples::new(64)).unwrap(),
                floor: PROBE_FLOOR,
                state: StateHash::of(b"state"),
            },
            verdict: Verdict { tail, latency },
        };
        let verdicts = [
            (TailVerdict::Honest, LatencyVerdict::Honest),
            (
                TailVerdict::TailLonger {
                    reported: Samples::new(1),
                    measured: Samples::new(2),
                },
                LatencyVerdict::LatencyDiffers {
                    reported: Samples::new(0),
                    measured: Samples::new(64),
                },
            ),
            (
                TailVerdict::NeverSettles {
                    level: tutti_units::Db::new(-60.0),
                },
                LatencyVerdict::NotChecked(NotChecked::Silent),
            ),
        ];
        let plugins = verdicts
            .into_iter()
            .enumerate()
            .map(|(k, (tail, latency))| Described {
                id: Some(PluginId::Vst2(k as u32)),
                descriptor: PluginDescriptor {
                    measured: Some(measured(tail, latency)),
                    ..PluginDescriptor::new("", PluginClass::Unknown)
                },
            })
            .collect();
        let outcome = ProbeOutcome::Described {
            plugins,
            unmeasured: Vec::new(),
        };
        assert_eq!(decode_result(&encode_result(&outcome)), Ok(outcome));
    }

    /// **Every refusal and every measurement failure a probe can write survives
    /// the file**: postcard has no defaults, so a variant a reader cannot decode
    /// would read as a malformed result, and a host would mistake a refusal for a
    /// broken probe.
    #[test]
    fn every_refusal_and_failure_survives_the_result_file() {
        use crate::{LoadStage, MaxBlock, PluginFormat, SampleRate, Samples};
        let (rate, block) = (
            SampleRate::SR_48K,
            MaxBlock::new(Samples::new(512)).unwrap(),
        );
        let reason = || "the plugin's words".to_string();
        let refusals = [
            ProbeRefusal::Missing,
            ProbeRefusal::UnsupportedFormat,
            ProbeRefusal::FormatNotCompiled(PluginFormat::AudioUnit),
            ProbeRefusal::StateUnreadable(reason()),
            ProbeRefusal::StateRefused {
                plugin: PluginId::Vst2(1),
                reason: reason(),
            },
            ProbeRefusal::Load {
                stage: LoadStage::Activation,
                reason: reason(),
            },
            ProbeRefusal::Describe(reason()),
            ProbeRefusal::UnknownPlugin(PluginId::Vst2(1)),
            ProbeRefusal::AmbiguousPlugin { count: 2 },
            ProbeRefusal::Usage,
            ProbeRefusal::NotMainThread,
            ProbeRefusal::Empty,
        ];
        for refusal in refusals {
            let outcome = ProbeOutcome::Failed(refusal);
            assert_eq!(decode_result(&encode_result(&outcome)), Ok(outcome));
        }
        let failures = [
            MeasureFailure::Load { reason: reason() },
            MeasureFailure::StateRefused { reason: reason() },
            MeasureFailure::SaveState { reason: reason() },
            MeasureFailure::NoAudioOutput,
            MeasureFailure::BudgetSpent,
            MeasureFailure::Activate {
                rate,
                block,
                reason: reason(),
            },
            MeasureFailure::Render {
                rate,
                block,
                reason: reason(),
            },
            MeasureFailure::Deactivate {
                rate,
                block,
                reason: reason(),
            },
            MeasureFailure::TimedOut { rate, block },
        ];
        let outcome = ProbeOutcome::Described {
            plugins: vec![Described {
                id: None,
                descriptor: PluginDescriptor::new("", PluginClass::Unknown),
            }],
            unmeasured: failures
                .into_iter()
                .map(|failure| Unmeasured {
                    plugin: PluginId::Vst2(1),
                    failure,
                })
                .collect(),
        };
        assert_eq!(decode_result(&encode_result(&outcome)), Ok(outcome));
    }

    /// Every plugin of a file, in order, is in the file: the list is the outcome.
    #[test]
    fn every_plugin_of_a_file_survives_it_in_order() {
        let ProbeOutcome::Described { plugins, .. } =
            decode_result(&encode_result(&described())).unwrap()
        else {
            panic!("described");
        };
        let ids: Vec<_> = plugins.iter().map(|p| p.id.clone().unwrap()).collect();
        assert_eq!(
            ids,
            [
                PluginId::Clap("test".into()),
                PluginId::Clap("other".into())
            ]
        );
    }
}
