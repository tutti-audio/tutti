//! Plugin state save / restore.
//!
//! VST2's state model has two shapes, and both are wrapped in a four-byte tag
//! so a restore can tell them apart. Both carry the **current program's index**
//! (`effGetProgram`), and a restore sets it (`effSetProgram`) before it hands
//! the plugin anything else:
//!
//! - `b"CHK\x01"` — the plugin set `effFlagsProgramChunks` and supplied its own
//!   opaque binary blob for the **current program** (`effGetChunk`, index 1).
//! - `b"PRM\x01"` — fallback: each normalized `f32` parameter value of the current
//!   program, serialized in index order and replayed on restore. Loses any
//!   non-parameter state the plugin holds, but works for a plugin with no chunk
//!   mechanism.
//! - `b"CHK\0"` and `b"PRM\0"` are the same shapes written before the program
//!   index was carried, still read, with no program set.
//!
//! **Program chunk, not bank chunk, and why.** `aeffect.h` (authority 3) says
//! `effFlagsProgramChunks` means "program data is handled in formatless chunks", and that
//! `effGetChunk` and `effSetChunk` take `[index]: 0 for bank, 1 for program`. It does not say
//! which one a host saves. A bank chunk covers all `numPrograms` programs; a program chunk the
//! current one. What this state restores is the plugin as it sounds now, one program, and the
//! other programs are the plugin's presets, which tutti lists and selects by index and does
//! not own; so the state is the current program's chunk plus the index to put it back into.
//! A program chunk restored into a fresh instance lands in whichever program is current there
//! (program 0), which is the loss this index prevents. The header does not say what a plugin
//! does with a program chunk it is handed while another program is current; setting the
//! program first is the only order the opcodes' wording allows.

use tutti_plugin_types::{MainThreadMarker, StateFrame};
use tutti_units::Normalized;

use crate::error::{Result, Vst2Error};
use crate::loaded::Vst2Loaded;

/// State format tags:
/// - `b"CHK\x01"` = chunk-based state of the current program, with its index
/// - `b"PRM\x01"` = parameter-based state, with the program index
/// - `b"CHK\0"`, `b"PRM\0"` = the same with no program index (older states)
const STATE_HEADER_CHUNK: [u8; 4] = *b"CHK\x01";
const STATE_HEADER_PARAMS: [u8; 4] = *b"PRM\x01";
const STATE_HEADER_CHUNK_NO_PROGRAM: [u8; 4] = *b"CHK\0";
const STATE_HEADER_PARAMS_NO_PROGRAM: [u8; 4] = *b"PRM\0";

const TAG_CHUNK: usize = 0;
const TAG_PARAMS: usize = 1;
const TAG_CHUNK_NO_PROGRAM: usize = 2;

/// How a VST2 state blob opens: by one of the four tags, and nothing else.
///
/// The VST2 spec has no state container beyond `effGetChunk`/`effSetChunk`'s opaque bytes
/// (`aeffect.h`), and a plugin without chunks has no stream at all, so every state this host
/// writes is tagged and an untagged blob is no state of this host's: it is refused rather
/// than handed to `effSetChunk`.
const FRAME: StateFrame = StateFrame::new(
    "VST2",
    &[
        &STATE_HEADER_CHUNK,
        &STATE_HEADER_PARAMS,
        &STATE_HEADER_CHUNK_NO_PROGRAM,
        &STATE_HEADER_PARAMS_NO_PROGRAM,
    ],
);

/// The program index a state records when the plugin has no programs to select.
const NO_PROGRAM: i32 = -1;

/// The parsed shape of a state blob's framing, produced by
/// [`parse_state_header`] before any plugin interaction. Splitting this out
/// keeps the pure framing/validation logic testable without a live plugin.
#[derive(Debug, PartialEq)]
pub(crate) enum StateHeader<'a> {
    /// A chunk payload — hand straight to `load_preset_data`, after the program is set.
    Chunk {
        /// The program to select first; `None` for a state that predates the index.
        program: Option<i32>,
        payload: &'a [u8],
    },
    /// A parameter snapshot: `count` normalized f32 values follow,
    /// carried in `values` (already length-validated against `count`).
    Params {
        /// The program to select first; `None` for a state that predates the index.
        program: Option<i32>,
        count: i32,
        values: &'a [u8],
    },
}

/// Validates the framing of a state blob and split off its payload.
///
/// Pure: performs only the header/length/count checks that don't need a live
/// plugin (the plugin-parameter-count cross-check stays in `write_state`).
/// Errors mirror the `write_state` early-returns exactly.
pub(crate) fn parse_state_header(data: &[u8]) -> Result<StateHeader<'_>> {
    let (tag, body) = FRAME.open(data)?;
    let chunk = tag == TAG_CHUNK || tag == TAG_CHUNK_NO_PROGRAM;
    let with_program = tag == TAG_CHUNK || tag == TAG_PARAMS;
    let mut reader = FRAME.reader(body);
    let program = if with_program {
        Some(reader.i32_le("program index")?)
    } else {
        None
    };

    if chunk {
        let payload = reader.take_rest();
        if payload.is_empty() {
            return Err(reader.invalid("chunk", "empty").into());
        }
        return Ok(StateHeader::Chunk { program, payload });
    }

    let count = reader.i32_le("parameter count")?;
    if count < 0 {
        return Err(reader
            .invalid("parameter count", format!("{count} is negative"))
            .into());
    }
    let values = reader.take_rest();
    let Some(expected) = (count as usize).checked_mul(4) else {
        return Err(reader
            .invalid(
                "parameter count",
                format!("{count} values overflow a length"),
            )
            .into());
    };
    if values.len() != expected {
        return Err(reader
            .invalid(
                "parameter values",
                format!(
                    "{count} values need {expected} bytes, {} follow",
                    values.len()
                ),
            )
            .into());
    }
    Ok(StateHeader::Params {
        program,
        count,
        values,
    })
}

impl Vst2Loaded {
    /// Captures the plugin's current state as a byte blob that
    /// `write_state` can restore.
    ///
    /// Uses the plugin's own chunk format (`effGetChunk`, the current program's) when it
    /// declares `effFlagsProgramChunks` and returns a non-empty chunk; otherwise saves
    /// every parameter's normalized value, which loses any state the plugin
    /// keeps outside its parameters. The blob starts with a four-byte tag
    /// (`CHK\x01` or `PRM\x01`) and the current program's index (see the module's docs),
    /// and is only meaningful to this crate.
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::ChunkSave`] if the plugin's chunk save fails, or [`Vst2Error::NoGetParameter`] if
    /// it declares parameters but exposes no `getParameter` to read them.
    ///
    /// `effGetChunk` is a main-thread opcode, so this takes the proof.
    pub fn read_state(&self, main: MainThreadMarker) -> Result<Vec<u8>> {
        let info = self.instance.get_info();
        // `effGetProgram` is meaningful only for a plugin that has programs.
        let program = if self.instance.read_num_programs() > 0 {
            self.instance.get_preset_num(main)
        } else {
            NO_PROGRAM
        };

        if info.preset_chunks {
            // `try_get_preset_data`, not `get_preset_data`: the infallible one
            // folds "nothing saved" and "the save failed" into the same empty
            // `Vec`, so a failed `getChunk` would silently downgrade to a
            // parameter snapshot, losing the non-parameter state chunks exist
            // to carry. A failure is an error; an *empty* chunk still falls through
            // to the parameter snapshot, keeping a fresh plugin saveable.
            let chunk = self
                .instance
                .try_get_preset_data(main)
                .map_err(Vst2Error::ChunkSave)?;
            if !chunk.is_empty() {
                let mut state = Vec::with_capacity(8 + chunk.len());
                state.extend_from_slice(&STATE_HEADER_CHUNK);
                state.extend_from_slice(&program.to_le_bytes());
                state.extend_from_slice(&chunk);
                return Ok(state);
            }
        }

        // Fallback: serialize all parameters.
        //
        // `.max(0)`: `numParams` is raw off the `AEffect` and a plugin can put
        // anything there. Unclamped, `-1` sign-extends to `usize::MAX` and the
        // `* 4` overflows — profile-dependent, so debug panics on multiply
        // overflow while release wraps to a small capacity and carries on.
        let param_count = info.parameters.max(0);
        let mut state = Vec::with_capacity(4 + 4 + 4 + (param_count as usize) * 4);
        state.extend_from_slice(&STATE_HEADER_PARAMS);
        state.extend_from_slice(&program.to_le_bytes());
        state.extend_from_slice(&param_count.to_le_bytes());

        for i in 0..param_count {
            // A plugin that advertises parameters but exposes no accessor
            // cannot be serialized: writing 0.0 would produce a blob that
            // restores silently and wrongly, which is worse than refusing.
            let Some(value) = self.instance.get_parameter(i) else {
                return Err(Vst2Error::NoGetParameter {
                    count: param_count,
                    index: i,
                });
            };
            state.extend_from_slice(&value.to_le_bytes());
        }

        Ok(state)
    }

    /// Restores a state blob previously produced by
    /// [`read_state`](Self::read_state).
    ///
    /// Each value of a parameter snapshot is a host write: a plugin's `audioMasterAutomate`
    /// echo of it is not reported as its own edit. A chunk is the plugin's to apply, so what
    /// it reports while loading one (a gesture of its own) is reported. The program is
    /// selected inside `effBeginSetProgram` / `effEndSetProgram` as
    /// [`set_program`](Self::set_program) does.
    ///
    /// # Errors
    ///
    /// Returns [`Vst2Error::TooManyParameters`], [`Vst2Error::ParameterOutOfRange`] or
    /// [`Vst2Error::NoSetParameter`] if the blob is malformed (unknown header, truncated,
    /// or an empty chunk), if a parameter snapshot holds more parameters than the plugin
    /// declares or a value that is not on `0..=1` (NaN included; none is clamped), or if the
    /// plugin exposes no `setParameter` to write parameters with. A refused snapshot writes
    /// nothing. A plugin's own refusal of a chunk is not an error here: the header
    /// gives `effSetChunk` no return value to refuse with.
    pub(crate) fn write_state(&mut self, main: MainThreadMarker, data: &[u8]) -> Result<()> {
        let state = parse_state_header(data)?;
        if let StateHeader::Params { count, values, .. } = &state {
            // `.max(0)` as in `read_state`: `numParams` is raw from the
            // `AEffect`. Unclamped, a plugin declaring `-1` makes this
            // comparison `0 > -1` and rejects the empty snapshot
            // `read_state` just wrote for that same plugin.
            let actual_count = self.instance.get_info().parameters.max(0);
            if *count > actual_count {
                return Err(Vst2Error::TooManyParameters {
                    saved: *count,
                    actual: actual_count,
                });
            }
            for index in 0..*count {
                let value = saved_value(values, index);
                if Normalized::new(f64::from(value)).is_none() {
                    return Err(Vst2Error::ParameterOutOfRange { index, value });
                }
            }
        }
        // The program first: a program chunk is the current program's, and a parameter
        // snapshot is its values. A recorded index the plugin does not have (or `-1`, for a
        // plugin that had no programs) selects nothing.
        let (StateHeader::Chunk { program, .. } | StateHeader::Params { program, .. }) = &state;
        if let Some(program) = *program
            && (0..self.instance.read_num_programs()).contains(&program)
        {
            self.instance
                .with_preset_bracket(main, |instance| instance.change_preset(main, program));
        }
        match state {
            StateHeader::Chunk { payload, .. } => {
                // `effSetChunk` has no return value in the header (aeffect.h documents
                // none, and a JUCE plugin answers 0 after loading), so there is no refusal
                // to read: the call returning is all that is known.
                self.instance.load_preset_data(main, payload);
                // The chunk is the plugin's to apply: no value it holds is a host write.
                self.host.clear_written();
                Ok(())
            }
            StateHeader::Params { count, values, .. } => (0..count).try_for_each(|index| {
                // A saved word widens exactly, so its narrowing is the word itself.
                let saved = Normalized::clamped(f64::from(saved_value(values, index)));
                if self.host.write(index, saved, |word| {
                    self.instance.set_parameter(index, word)
                }) {
                    Ok(())
                } else {
                    // Reporting success here would claim a preset was
                    // restored while every value went nowhere.
                    Err(Vst2Error::NoSetParameter { index })
                }
            }),
        }
    }
}

/// The `index`th `f32` of a parameter snapshot's values, which hold a whole number of them.
fn saved_value(values: &[u8], index: i32) -> f32 {
    let offset = index as usize * 4;
    f32::from_le_bytes([
        values[offset],
        values[offset + 1],
        values[offset + 2],
        values[offset + 3],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::StateFrameError;

    #[test]
    fn header_too_short() {
        let err = parse_state_header(&[0x43, 0x48]).unwrap_err();
        assert!(matches!(
            err,
            Vst2Error::StateFrame(StateFrameError::Unframed { format: "VST2" })
        ));
        assert!(err.to_string().contains("none of the tags"));
    }

    #[test]
    fn unknown_header() {
        let err = parse_state_header(b"XXX\0payload").unwrap_err();
        assert!(err.to_string().contains("none of the tags"));
    }

    #[test]
    fn chunk_empty_payload_rejected() {
        let mut data = STATE_HEADER_CHUNK.to_vec();
        data.extend_from_slice(&0i32.to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(err.to_string().contains("chunk is invalid: empty"));
    }

    #[test]
    fn chunk_valid() {
        let mut data = STATE_HEADER_CHUNK.to_vec();
        data.extend_from_slice(&3i32.to_le_bytes());
        data.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            parse_state_header(&data).unwrap(),
            StateHeader::Chunk {
                program: Some(3),
                payload: &[1, 2, 3, 4]
            }
        );
    }

    #[test]
    fn a_state_without_a_program_index_is_still_read() {
        let mut data = STATE_HEADER_CHUNK_NO_PROGRAM.to_vec();
        data.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            parse_state_header(&data).unwrap(),
            StateHeader::Chunk {
                program: None,
                payload: &[1, 2, 3, 4]
            }
        );
    }

    #[test]
    fn a_program_state_cut_before_its_index_is_refused() {
        let mut data = STATE_HEADER_CHUNK.to_vec();
        data.extend_from_slice(&[0, 0]);
        let err = parse_state_header(&data).unwrap_err();
        assert!(matches!(
            err,
            Vst2Error::StateFrame(StateFrameError::Truncated {
                field: "program index",
                ..
            })
        ));
    }

    #[test]
    fn params_missing_count() {
        // Header present but fewer than 4 count bytes follow.
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&[0, 0]);
        let err = parse_state_header(&data).unwrap_err();
        assert!(matches!(
            err,
            Vst2Error::StateFrame(StateFrameError::Truncated {
                field: "parameter count",
                ..
            })
        ));
    }

    #[test]
    fn params_negative_count() {
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&(-1i32).to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(
            err.to_string()
                .contains("parameter count is invalid: -1 is negative")
        );
    }

    #[test]
    fn params_size_mismatch() {
        // count says 2 params (needs 4 + 2*4 = 12 bytes payload) but only 1
        // value's worth of bytes follow.
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&2i32.to_le_bytes());
        data.extend_from_slice(&1.0f32.to_le_bytes());
        let err = parse_state_header(&data).unwrap_err();
        assert!(matches!(
            err,
            Vst2Error::StateFrame(StateFrameError::Invalid {
                field: "parameter values",
                ..
            })
        ));
    }

    #[test]
    fn params_valid_roundtrip() {
        let mut data = STATE_HEADER_PARAMS_NO_PROGRAM.to_vec();
        data.extend_from_slice(&2i32.to_le_bytes());
        data.extend_from_slice(&0.25f32.to_le_bytes());
        data.extend_from_slice(&0.75f32.to_le_bytes());

        match parse_state_header(&data).unwrap() {
            StateHeader::Params {
                program,
                count,
                values,
            } => {
                assert_eq!(program, None);
                assert_eq!(count, 2);
                assert_eq!(values.len(), 8);
                assert_eq!(f32::from_le_bytes(values[0..4].try_into().unwrap()), 0.25);
                assert_eq!(f32::from_le_bytes(values[4..8].try_into().unwrap()), 0.75);
            }
            other => panic!("expected Params, got {other:?}"),
        }
    }

    #[test]
    fn params_with_a_program_index() {
        let mut data = STATE_HEADER_PARAMS.to_vec();
        data.extend_from_slice(&5i32.to_le_bytes());
        data.extend_from_slice(&1i32.to_le_bytes());
        data.extend_from_slice(&0.5f32.to_le_bytes());
        match parse_state_header(&data).unwrap() {
            StateHeader::Params {
                program,
                count,
                values,
            } => {
                assert_eq!((program, count, values.len()), (Some(5), 1, 4));
            }
            other => panic!("expected Params, got {other:?}"),
        }
    }
}
