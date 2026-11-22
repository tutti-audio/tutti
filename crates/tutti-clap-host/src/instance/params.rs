//! Parameter-query and parameter-update methods for [`ClapLoaded`].
//!
//! Values here are **plain**, in each parameter's own declared range — CLAP has
//! no normalized parameter interface. Denormalization against the range table
//! happens on the `process` path, not here.

use super::ClapLoaded;
use crate::events::ParamBounds;
use crate::events::{ClapEvent, FlushEvents, OutputEventList, OutputRooms};
use crate::types::ParameterChanges;

/// The events other than parameter values one `params.flush` may output.
const FLUSH_EVENT_CAPACITY: usize = 256;

/// What one `params.flush` output, and what it could not hold.
#[derive(Debug, Default)]
pub struct Flushed {
    events: Vec<ClapEvent>,
    lost: u64,
}

impl Flushed {
    /// The events the plugin output, in the order it pushed them.
    #[must_use]
    pub fn events(&self) -> &[ClapEvent] {
        &self.events
    }

    /// The events, owned.
    #[must_use]
    pub fn into_events(self) -> Vec<ClapEvent> {
        self.events
    }

    /// Events the plugin output that [`events`](Self::events) does not hold,
    /// for want of room in the flush: refused, or a parameter value past the
    /// room that a later one replaced or that no list carries. Zero while the
    /// plugin stays within the room.
    #[must_use]
    pub fn lost(&self) -> u64 {
        self.lost
    }
}
use crate::types::{ClapParamFlags, ClapParamInfo};
use crate::types::{Color, ParamAutomationState};
use tutti_plugin_types::MainThreadMarker;
use tutti_units::ParamId;

fn color_to_clap(color: Color) -> clap_sys::color::clap_color {
    clap_sys::color::clap_color {
        alpha: color.alpha,
        red: color.red,
        green: color.green,
        blue: color.blue,
    }
}

impl ClapLoaded {
    /// Number of parameters the plugin exposes. Returns 0 if the plugin
    /// does not implement `CLAP_EXT_PARAMS`.
    pub fn parameter_count(&self, main: MainThreadMarker) -> usize {
        self.plugin.param_count(main) as usize
    }

    /// Current value of a parameter in its own declared range, or `None` if the
    /// plugin does not support the extension or rejects the ID. The normalized
    /// reading is [`Control::parameter`](tutti_plugin_types::Control::parameter).
    pub fn plain_value(&self, main: MainThreadMarker, id: u32) -> Option<f64> {
        self.plugin.param_value(main, id)
    }

    /// The declared `[min, max]` for one parameter, or `None` if the plugin
    /// does not declare it.
    ///
    /// `None` means "no range to convert against", which the caller must treat
    /// as pass-through rather than substituting bounds. Inventing a `0..=1`
    /// default here would silently rescale a parameter the plugin never
    /// described.
    pub fn parameter_range(&self, _main: MainThreadMarker, id: ParamId) -> Option<(f64, f64)> {
        let bounds = self.params.bounds(id)?;
        Some((bounds.min, bounds.max))
    }

    /// The declared bounds and steppedness for one parameter: what a
    /// normalized value is denormalized against. [`None`] as for
    /// [`parameter_range`](Self::parameter_range).
    pub(crate) fn parameter_bounds(&self, id: ParamId) -> Option<ParamBounds> {
        self.params.bounds(id)
    }

    /// Every parameter projected onto the shared, format-agnostic
    /// [`ParameterInfo`](tutti_plugin_types::ParameterInfo) — the value that
    /// crosses the crate boundary. CLAP has no unit string, so `unit` is empty;
    /// `step_count` comes from the `STEPPED` flag plus the declared span, since
    /// CLAP reports steppedness as a flag and the count only via `min`/`max`.
    /// Read from the table the plugin's rescans refresh.
    pub fn parameter_list(
        &self,
        _main: MainThreadMarker,
    ) -> Vec<tutti_plugin_types::ParameterInfo> {
        self.params.listed().to_vec()
    }

    /// Format a parameter `value` to its human-readable display string via the
    /// plugin's `clap_plugin_params.value_to_text`. Returns `None` if the
    /// plugin does not implement params / value_to_text or declines the id.
    ///
    /// `value` is **plain**, in the parameter's declared range — CLAP has no
    /// normalization concept. `Control::parameter_text` denormalizes against
    /// [`parameter_range`](Self::parameter_range) before calling this, the same
    /// conversion its `set_parameter` applies.
    pub fn value_to_text(
        &self,
        main: MainThreadMarker,
        param_id: u32,
        value: f64,
    ) -> Option<String> {
        self.plugin.param_value_to_text(main, param_id, value)
    }

    /// Parses a display `text` back to a parameter value via the plugin's
    /// `clap_plugin_params.text_to_value`. Returns `None` if the plugin does
    /// not implement params / text_to_value, the string has an interior NUL,
    /// or the plugin cannot parse it.
    ///
    /// The answer is **plain**, in the parameter's declared range — see
    /// [`value_to_text`](Self::value_to_text). `Control` re-normalizes it.
    pub fn text_to_value(&self, main: MainThreadMarker, param_id: u32, text: &str) -> Option<f64> {
        self.plugin.param_text_to_value(main, param_id, text)
    }

    /// Whether the parameter with `param_id` carries the
    /// `CLAP_PARAM_REQUIRES_PROCESS` flag: its changes must be delivered
    /// through `process()`, in event order, and not through an out-of-band
    /// `flush`, while the plugin is processing. `false` if the id is unknown or
    /// the plugin has no params.
    ///
    /// Answered from the same table as `parameter_list`, so the two cannot
    /// drift about which parameters exist.
    pub fn param_requires_process(&self, _main: MainThreadMarker, param_id: ParamId) -> bool {
        self.params.requires_process(param_id)
    }

    /// Delivers parameter changes outside of `process()` via
    /// `clap_plugin_params.flush()`, returning the events the plugin produced in
    /// response.
    ///
    /// Returns no events if the plugin does not implement params or lacks a
    /// flush function. Events the plugin output past the flush's room are not
    /// returned and are counted in [`Flushed::lost`] and in
    /// [`dropped_reported_edits`](tutti_plugin_types::Control::dropped_reported_edits).
    /// The events returned are the caller's to use: the
    /// flush a plugin's `request_flush` causes while inactive drops them,
    /// since only a block's output is reported.
    ///
    /// # Threading
    /// CLAP declares `params.flush` as `[active ? audio-thread : main-thread]`
    /// and states it "must not be called concurrently to
    /// `clap_plugin->process()`". This gates by state, and on the active path
    /// enforces that with real mutual exclusion, not an assertion:
    ///
    /// - **Inactive** (`!flags.active`) — `activate()` has not run, so a
    ///   main-thread flush is what the spec asks for. This is the GUI-only /
    ///   setup path.
    /// - **Active** (`flags.active`) — the flush runs under an
    ///   audio claim, so the calling
    ///   thread *becomes* the audio thread for the duration (which the spec
    ///   explicitly permits for any OS thread) and **blocks** until any
    ///   in-flight `process` on the real audio thread has returned.
    ///
    /// The condition is `active`, **not** `processing`. Those differ for the
    /// whole window between `activate()` and the first `process()` — which is
    /// exactly when a host sets up initial parameter values, and when a plugin
    /// that considers itself active expects the audio-thread role.
    ///
    /// Callers driving an active instance should prefer routing param changes
    /// through the next `process` block (a controller's `set_parameter`, or
    /// `ClapProcessContext::params`) — that is in-order delivery rather than an
    /// out-of-band poke — but doing it here is safe.
    ///
    /// An inactive instance is a main-thread call, which `_main` proves; an
    /// active plugin takes the `[audio-thread]` claim inside the FFI call
    /// (`flush` is an audio-thread entry then).
    pub fn flush_params(
        &mut self,
        main: MainThreadMarker,
        input_events: Vec<ClapEvent>,
    ) -> Flushed {
        let mut input_list = FlushEvents::new(input_events);
        input_list.sort_by_time();
        // One parameter value's worth of points per parameter, as the output
        // of a block is budgeted at least; a flush has no block to size by.
        let ranges = &self.params.ranges().ranges;
        let ids: Vec<ParamId> = ranges.iter().map(|b| b.id).collect();
        let points = ParameterChanges::point_budget(ranges.len(), 1);
        let mut output_list =
            OutputEventList::new(OutputRooms::new(FLUSH_EVENT_CAPACITY).params(points, &ids));

        if !self
            .plugin
            .params_flush(main, &input_list, &mut output_list)
        {
            return Flushed::default();
        }

        let lost = output_list.lost();
        self.flush_lost += lost;
        Flushed {
            events: output_list.take_events(),
            lost,
        }
    }

    /// Sets one parameter by flushing a single `PARAM_VALUE` event.
    ///
    /// `value` is plain, in the parameter's declared range. On an active plugin
    /// the flush runs under the `[audio-thread]` claim (see
    /// [`flush_params`](Self::flush_params)); a [`ClapController`](crate::ClapController)
    /// routes its normalized writes through the Processor instead, which delivers
    /// them in order with the block.
    ///
    /// # Panics
    ///
    /// As for [`flush_params`](Self::flush_params).
    pub fn set_plain_value(&mut self, main: MainThreadMarker, id: u32, value: f64) {
        self.flush_params(main, vec![ClapEvent::param_value(0, id, value)]);
    }

    /// Informs the plugin of a parameter's automation state so it can update
    /// UI feedback (e.g. knob rings). No-op if the plugin does not implement
    /// `CLAP_EXT_PARAM_INDICATION`.
    pub fn set_param_automation(
        &self,
        main: MainThreadMarker,
        param_id: u32,
        state: ParamAutomationState,
        color: Option<Color>,
    ) {
        self.plugin
            .set_param_automation(main, param_id, state, color.map(color_to_clap));
    }
}

/// Project CLAP's native [`ClapParamInfo`] onto the shared, format-agnostic
/// [`ParameterInfo`](tutti_plugin_types::ParameterInfo). Maps the CLAP flag
/// subset the shared vocabulary models (automatable / read-only / periodic→wrap
/// / bypass / hidden), derives `step_count` from the `STEPPED` bit and the
/// declared span, and leaves `unit` empty (CLAP carries no unit string). CLAP
/// parameter values are in the plugin's native plain range, so
/// `min_value`/`max_value` pass through verbatim.
pub(super) fn project_param_info(info: ClapParamInfo) -> tutti_plugin_types::ParameterInfo {
    use tutti_plugin_types::{ParamFlags, ParamSteps, PluginParamRange};

    // CLAP is the one hosted format that reports every capability we model,
    // including the per-voice modulation bits no other format has.
    const KNOWN: ParamFlags = ParamFlags::all();

    let pairs = [
        (ClapParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE),
        (ClapParamFlags::READONLY, ParamFlags::READ_ONLY),
        (ClapParamFlags::PERIODIC, ParamFlags::WRAP),
        (ClapParamFlags::BYPASS, ParamFlags::BYPASS),
        (ClapParamFlags::HIDDEN, ParamFlags::HIDDEN),
        (ClapParamFlags::MODULATABLE, ParamFlags::MODULATABLE),
        (
            ClapParamFlags::AUTOMATABLE_PER_NOTE_ID,
            ParamFlags::PER_NOTE_ID,
        ),
        (ClapParamFlags::AUTOMATABLE_PER_KEY, ParamFlags::PER_KEY),
        (
            ClapParamFlags::AUTOMATABLE_PER_CHANNEL,
            ParamFlags::PER_CHANNEL,
        ),
        (ClapParamFlags::AUTOMATABLE_PER_PORT, ParamFlags::PER_PORT),
    ];
    // CLAP splits per-voice targeting across automation and modulation; either
    // one means the host may address that scope.
    let modulation_pairs = [
        (
            ClapParamFlags::MODULATABLE_PER_NOTE_ID,
            ParamFlags::PER_NOTE_ID,
        ),
        (ClapParamFlags::MODULATABLE_PER_KEY, ParamFlags::PER_KEY),
        (
            ClapParamFlags::MODULATABLE_PER_CHANNEL,
            ParamFlags::PER_CHANNEL,
        ),
        (ClapParamFlags::MODULATABLE_PER_PORT, ParamFlags::PER_PORT),
    ];

    let mut flags = ParamFlags::empty();
    for (clap, ours) in pairs.into_iter().chain(modulation_pairs) {
        if info.flags.contains(clap) {
            flags |= ours;
        }
    }

    // CLAP's STEPPED says every value in `[min, max]` is an integer, so the
    // position count comes from the span — not 1. Reporting 1 made an 8-way
    // choice list indistinguishable from a two-state toggle.
    let steps = if info.flags.contains(ClapParamFlags::STEPPED) {
        ParamSteps::from_span(info.max_value - info.min_value)
    } else {
        ParamSteps::Continuous
    };

    // `clap_param_info.id` is a `clap_id`, an opaque plugin-chosen handle, the
    // same concept VST3 spells `ParamID` and AU spells `AudioUnitParameterID`.
    // CLAP carries no unit string, so no unit is typed from one, and it declares
    // no role. Its values are in the plugin's native plain range.
    //
    // `clap_param_info.module` is a `/`-separated path ("Osc 1/Filter"), and
    // CLAP documents it as a display hint rather than an addressing scheme. It
    // passes through verbatim: splitting it here would impose a hierarchy the
    // shared vocabulary does not model, and three of the four hosted formats
    // could not fill.
    let plain_map = crate::ParamBounds::declared(&info).plain_map();
    tutti_plugin_types::ParameterInfo::new(tutti_units::ParamId::new(info.id), info.name)
        .with_range(PluginParamRange::Plain {
            min: info.min_value,
            max: info.max_value,
            default: info.default_value,
        })
        .with_steps(steps)
        .with_plain_map(plain_map)
        .with_flags(KNOWN, flags)
        .with_group(info.module)
}

#[cfg(test)]
mod grouping_tests {
    use super::*;

    /// CLAP's `module` reaches the shared type as the group, unchanged.
    ///
    /// Kept verbatim rather than split on `/`: CLAP documents the path as a
    /// display hint, and splitting here would build a hierarchy the shared
    /// vocabulary does not model and three of the four hosted formats could
    /// not fill.
    #[test]
    fn a_clap_module_becomes_the_group_verbatim() {
        let mut info = ClapParamInfo::new(1, "Cutoff");
        info.module = "Osc 1/Filter".to_string();

        let projected = project_param_info(info);
        assert_eq!(projected.group(), "Osc 1/Filter");
        assert_eq!(projected.qualified_name(), "Osc 1/Filter / Cutoff");
    }

    /// A parameter that declares no module has no group.
    ///
    /// The common case — CLAP leaves `module` empty unless the plugin fills it
    /// — so this is what most parameters must produce.
    #[test]
    fn a_clap_parameter_without_a_module_has_no_group() {
        let projected = project_param_info(ClapParamInfo::new(1, "Cutoff"));
        assert!(projected.group().is_empty());
        assert_eq!(projected.qualified_name(), "Cutoff");
    }
}

#[cfg(test)]
mod measures_tests {
    use super::*;

    fn projected(flags: ClapParamFlags) -> tutti_plugin_types::ParameterInfo {
        let mut info = ClapParamInfo::new(1, "p");
        info.min_value = 0.0;
        info.max_value = 3.0;
        info.flags = flags;
        project_param_info(info)
    }

    /// `clap.params` declares no unit, so no flag gives a parameter one.
    #[test]
    fn no_flag_gives_a_unit() {
        for flags in [
            ClapParamFlags::IS_ENUM | ClapParamFlags::STEPPED,
            ClapParamFlags::STEPPED,
            ClapParamFlags::empty(),
        ] {
            assert_eq!(projected(flags).measures(), None, "{flags:?}");
        }
    }

    /// CLAP declares no role: not even the bypass flag gives one, which is the
    /// `BYPASS` capability bit instead.
    #[test]
    fn no_flag_gives_a_role() {
        for flags in [
            ClapParamFlags::BYPASS,
            ClapParamFlags::AUTOMATABLE,
            ClapParamFlags::IS_ENUM,
        ] {
            assert_eq!(projected(flags).role(), None, "{flags:?}");
        }
        assert_eq!(
            projected(ClapParamFlags::BYPASS).flag(tutti_plugin_types::ParamFlags::BYPASS),
            Some(true)
        );
    }

    /// `IS_ENUM` is the spec's bit 16 (`CLAP_PARAM_IS_ENUM`, clap/ext/params.h),
    /// read from the plugin's raw flag word rather than truncated away.
    #[test]
    fn the_raw_enum_bit_is_read() {
        assert_eq!(ClapParamFlags::IS_ENUM.bits(), 1 << 16);
        assert_eq!(
            ClapParamFlags::from_bits_truncate(1 << 16),
            ClapParamFlags::IS_ENUM
        );
    }
}
