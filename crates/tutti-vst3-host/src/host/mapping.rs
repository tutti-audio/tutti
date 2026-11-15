//! One VST3 parameter's [`ParamMapping`]: the plugin's own conversion.

use tutti_plugin_types::{MainThreadMarker, Normalized, ParamMapping};
use tutti_vst3_ffi::Plugin;

/// The controller's `normalizedParamToPlain` and `plainParamToNormalized` for
/// one parameter, whatever shape they have (VST3 fixes none,
/// `ivsteditcontroller.h:508-514`): the plugin's own map, so a decreasing or
/// exponential parameter converts as its editor shows it.
///
/// It borrows the plugin and the [`MainThreadMarker`] token the two controller calls
/// need (`[UI-thread & Connected]`), so it lives for the call that builds it:
/// [`Vst3Loaded::mapping`](crate::Vst3Loaded::mapping).
///
/// The SDK's calls return a bare `ParamValue`, which cannot report a failure, so
/// a controller that answers a value the laws of [`ParamMapping`] exclude is
/// held to them here: a non-finite plain value is `0.0`. A NaN plain input
/// reads as position `0.0` without asking the controller, and an infinity as
/// the position of the end of the map it lies past, found by asking the
/// controller for both ends (the map may run either way).
#[derive(Clone, Copy)]
pub struct ControllerMapping<'a> {
    pub(super) plugin: &'a Plugin,
    pub(super) main: MainThreadMarker,
    pub(super) id: u32,
}

impl ParamMapping for ControllerMapping<'_> {
    fn plain(&self, value: Normalized) -> f64 {
        self.plugin
            .normalized_to_plain(self.main, self.id, value.get())
            .filter(|plain| plain.is_finite())
            .unwrap_or(0.0)
    }

    fn normalized(&self, plain: f64) -> Normalized {
        if plain.is_nan() {
            return Normalized::of(0.0);
        }
        if plain.is_infinite() {
            let (start, end) = (
                self.plain(Normalized::of(0.0)),
                self.plain(Normalized::of(1.0)),
            );
            // Past an end: the one the infinity's sign points at, which is
            // position 1 when the map rises and 0 when it falls.
            let rising = end > start;
            return Normalized::of(if (plain > 0.0) == rising && start != end {
                1.0
            } else {
                0.0
            });
        }
        self.plugin
            .plain_to_normalized(self.main, self.id, plain)
            .map_or(Normalized::of(0.0), Normalized::clamped)
    }
}

impl std::fmt::Debug for ControllerMapping<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControllerMapping")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}
