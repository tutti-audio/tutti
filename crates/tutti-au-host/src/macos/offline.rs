//! Bounce-time and buffer-strategy properties, plus the push-model render path.
//!
//! Four AUv2 facilities that a live-only host never needs and an exporting host
//! cannot do without. Grouped here rather than in `crate::stream` because they
//! share one duty: how the host intends to *drive* the render, as opposed to
//! what the stream looks like (`crate::stream`) or what the topology is
//! (`crate::bus`).
//!
//! * **Offline render** — tells the AU this block is part of a
//!   faster-than-real-time bounce, so it may take the slow high-quality path.
//! * **In-place processing** — whether the AU tolerates input and output
//!   aliasing the same memory, which is a copy per block per plugin the host can
//!   skip.
//! * **Render quality** — a 0–127 knob some AUs honour.
//! * **`AudioUnitProcess`** — the *push* render model, where the host hands
//!   input buffers in directly instead of waiting to be pulled through a render
//!   callback.
//!
//! ## Why the push path is a separate module and not a `process` variant
//!
//! [`AuProcessor::render_planes`](crate::processor::AuProcessor::render_planes) is a *pull* render:
//! it stages input into the heap-pinned `RenderScratch`, then `AudioUnitRender`
//! calls back into that scratch to fetch it. The push path never installs a
//! callback — with `AudioUnitProcess` the `ioData` buffer list carries the
//! input **in** and the output back **out** in the same call. Keeping the two
//! apart (each needs differently-shaped scratch: one list per bus for push,
//! exactly one for pull) is what stops the pull path's callback from firing
//! during a push render and overwriting the input the host just handed in.

use tutti_au_ffi::Unit;
use tutti_au_ffi::property::{InPlaceProcessing, OfflineRender, RenderQuality};
use tutti_au_ffi::{Element, Scope};

use crate::error::{AuError, Result};
use crate::types::*;

/// The inclusive maximum this host will write to
/// `kAudioUnitProperty_RenderQuality`.
///
/// 127, the range Apple's `AudioUnitProperties.h` documents (`kRenderQuality_Max`).
/// Enforced host-side because an AU can accept an out-of-range write with
/// `noErr` and read it back. See `set_render_quality`.
pub const RENDER_QUALITY_MAX: u32 = 127;

/// Whether the AU has been told it is rendering offline.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU does not implement
/// `kAudioUnitProperty_OfflineRender`. Propagated rather than flattened to
/// `false`: "this AU is in real-time mode" (changeable) and "this AU has no
/// offline mode" (the export may differ from the audition) are different facts a
/// bouncing host must distinguish.
pub(crate) fn is_offline_render(unit: &Unit) -> Result<bool> {
    let value: u32 = unit.get::<OfflineRender>(Scope::GLOBAL, Element::ZERO)?;
    Ok(value != 0)
}

/// Declare to the AU whether this render is a faster-than-real-time bounce.
///
/// The render calls are identical either way; the property changes what the AU
/// is *permitted* to do: drop to a cheaper algorithm to meet a real-time
/// deadline, pick a short resampling kernel, or seed dither from the clock.
/// Without it a bounce can differ from the monitor path.
///
/// ## Set it before `initialize`
///
/// Legal in either state, but an AU that sizes an internal
/// oversampling buffer from the flag can only do so at `AudioUnitInitialize` —
/// setting it after is accepted yet may silently not take effect. Set the flag,
/// then `initialize`, the same order as `MaximumFramesPerSlice`.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU has no offline-render property. See
/// [`is_offline_render`] for why that is an error rather than a silent no-op.
pub(crate) fn set_offline_render(unit: &Unit, offline: bool) -> Result<()> {
    let value: u32 = u32::from(offline);
    Ok(unit.set::<OfflineRender>(Scope::GLOBAL, Element::ZERO, &value)?)
}

/// Whether the AU is willing to render with input and output aliasing the same
/// buffer.
///
/// A host that gets `true` here may hand [`process_push`] one buffer list and
/// let the AU overwrite it, saving a copy per block per plugin.
///
/// ## Read-only, deliberately, even though the property is writable
///
/// Apple declares this Read/**Write**; the write lets a host whose buffer
/// management would be defeated by in-place operation forbid it. This crate
/// exposes only the read: tutti has no such strategy, so a setter would offer
/// only the pessimal setting.
///
/// The read, by contrast, is load-bearing: it decides whether [`process_push`]
/// may alias its buffers or must copy.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU does not implement
/// `kAudioUnitProperty_InPlaceProcessing`. Not flattened to `false`: `false`
/// means "the AU says no, do not alias", a refusal means "the AU did not say".
/// Both lead the host to copy, but only the first is a fact about the AU.
pub(crate) fn supports_in_place(unit: &Unit) -> Result<bool> {
    let value: u32 = unit.get::<InPlaceProcessing>(Scope::GLOBAL, Element::ZERO)?;
    Ok(value != 0)
}

/// The AU's current render-quality setting.
///
/// # Errors
/// [`AuError::OsStatus`] when the AU has no `kAudioUnitProperty_RenderQuality`,
/// which is the common case — see [`set_render_quality`].
pub(crate) fn render_quality(unit: &Unit) -> Result<u32> {
    Ok(unit.get::<RenderQuality>(Scope::GLOBAL, Element::ZERO)?)
}

/// Sets the AU's render quality, `0` (cheapest) to [`RENDER_QUALITY_MAX`] (best).
///
/// Not a substitute for [`set_offline_render`]: the offline flag says *why* the
/// host is rendering and leaves the AU to choose; this says what the host wants
/// regardless of context. A bounce sets both — offline so deterministic paths
/// engage, quality at maximum so an AU that exposes the knob also cooperates.
///
/// ## Why the range is enforced here rather than left to the AU
///
/// An AU may accept an out-of-range value with `noErr` and read it back, which
/// a read-back cannot tell from a legitimate setting. Rejecting host-side turns
/// that silent acceptance into a visible [`AuError::InvalidBuffer`].
///
/// # Errors
/// * [`AuError::InvalidBuffer`] for a `quality` above [`RENDER_QUALITY_MAX`].
/// * [`AuError::OsStatus`] when the AU has no render-quality property. That is
///   the majority: it is refused with `kAudioUnitErr_InvalidProperty` (-10879).
pub(crate) fn set_render_quality(unit: &Unit, quality: u32) -> Result<()> {
    if quality > RENDER_QUALITY_MAX {
        return Err(AuError::InvalidBuffer(format!(
            "render quality {quality} exceeds the maximum {RENDER_QUALITY_MAX}"
        )));
    }
    Ok(unit.set::<RenderQuality>(Scope::GLOBAL, Element::ZERO, &quality)?)
}

pub use tutti_au_ffi::{PushScratch, output_byte_sizes};

/// Runs one block through `AudioUnitProcess`: the input staged in `scratch`
/// goes in, the processed block comes out in `scratch`, and the unit's render
/// flags are returned. See `tutti_au_ffi::process_push` for the contract.
///
/// # Errors
/// [`AuError::BlockTooLarge`] if `frames` exceeds the scratch's block size,
/// [`AuError::MissingBus`] if the scratch lacks a bus the call needs, or
/// [`AuError::RenderFailed`] with the unit's last render error attached (read
/// here, on the control side, after the failed call).
pub fn process_push(
    unit: &Unit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    tutti_au_ffi::process_push(unit, scratch, frames).map_err(|e| with_last_render_error(unit, e))
}

/// As [`process_push`], through `AudioUnitProcessMultiple`: every input and
/// output bus in one call.
///
/// # Errors
/// As [`process_push`].
pub fn process_push_multiple(
    unit: &Unit,
    scratch: &mut PushScratch,
    frames: u32,
) -> Result<AudioUnitRenderActionFlags> {
    tutti_au_ffi::process_push_multiple(unit, scratch, frames)
        .map_err(|e| with_last_render_error(unit, e))
}

/// `e` as an [`AuError`], with the unit's last render error read in when a
/// render failed.
fn with_last_render_error(unit: &Unit, e: tutti_au_ffi::Error) -> AuError {
    match e {
        tutti_au_ffi::Error::RenderFailed { function, status } => {
            AuError::render_failed(function, status, unit.last_render_error())
        }
        other => other.into(),
    }
}
