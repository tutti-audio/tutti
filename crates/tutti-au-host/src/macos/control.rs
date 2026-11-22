//! The control surface of an Audio Unit: [`Control`] and [`EditorWindow`], on
//! [`AuLoaded`] and, through [`Wraps`], on [`AuController`].
//!
//! Parameters cross here as normalized `0..=1`, the host's authoring
//! convention, and are denormalized against the range the AU itself declared:
//! `AudioUnitSetParameter` takes native plain units. A parameter missing from
//! the table (the AU grew one after load, or refused `ParameterInfo`) is
//! reported and written as the plain value it was given, never scaled by a
//! guessed range.

use std::collections::HashMap;
use std::sync::Arc;

use dpi::{LogicalSize, Size};
use tutti_au_ffi::{AuEditor, Scope};
use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    AuComponentType, BypassWrite, ContentScale, Control, EditorCapabilities, EditorError,
    EditorPresence, EditorWindow, Features, FourCc, MainThreadMarker, OwnBypass, ParamEdit,
    ParamFlags, ParamMapping, ParamSteps, ParamWriteError, ParameterInfo, PluginBus, PluginBuses,
    PluginClass, PluginDescriptor, PluginError, PluginId, PluginParamRange, PluginPreset,
    PluginResult, PluginTail, PluginVersion, PluginWiring, PresetId, RenderModeSupport, Request,
    RescanScope, StateUse, WindowHandle, Wraps, taken,
};
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::Samples;

use crate::bus::BusDirection;
use crate::component::{AuType, ComponentKey, component_info};
use crate::controller::AuController;
use crate::edits::Edit;
use crate::error::AuError;
use crate::handle::AuHandle;
use crate::instance::AuLoaded;
use crate::marks::Marks;
use crate::parameters;
use crate::ranges::{ParamBounds, RangeTable};

/// The wire's mirror of the AU host's component type.
pub(crate) fn class_of(au_type: AuType) -> AuComponentType {
    match au_type {
        AuType::Effect => AuComponentType::Effect,
        AuType::Instrument => AuComponentType::Instrument,
        AuType::Generator => AuComponentType::Generator,
        AuType::MusicEffect => AuComponentType::MusicEffect,
        AuType::Mixer => AuComponentType::Mixer,
        AuType::Converter => AuComponentType::Converter,
        AuType::Output => AuComponentType::Output,
        AuType::MidiProcessor => AuComponentType::MidiProcessor,
        AuType::Unknown(code) => AuComponentType::Unknown(code),
    }
}

/// The id of the unit `desc` describes: its type, subtype and manufacturer.
/// The type is in it because an effect and an instrument of one manufacturer
/// can share a subtype. The bundle lookup and a load's origin both use this
/// function, so they cannot disagree.
pub(crate) fn unit_id(key: ComponentKey) -> PluginId {
    PluginId::Au {
        component_type: AuComponentType::from_code(key.component_type()),
        subtype: FourCc::new(key.sub_type()),
        manufacturer: FourCc::new(key.manufacturer()),
    }
}

/// The unit's identity in the shared vocabulary, with the editor measured now:
/// a Cocoa view is a property of the instance, readable before `initialize`.
pub(crate) fn describe(handle: &AuHandle) -> PluginDescriptor {
    let info = component_info(handle.component());
    let (vendor, version) = match &info {
        Some(info) => (info.manufacturer.clone(), info.version.clone()),
        None => (String::new(), PluginVersion::Absent),
    };
    PluginDescriptor {
        name: handle.get_name(),
        vendor,
        version,
        class: PluginClass::Au {
            component_type: class_of(handle.au_type()),
        },
        editor: EditorPresence::measured(AuEditor::has_editor(handle.unit())),
        measured: None,
        widths: Some(super::lifecycle::WIDTHS),
    }
}

/// `READ_ONLY` for a parameter the host cannot write: one that is not
/// `kAudioUnitParameterFlag_IsWritable`, or that is `kAudioUnitParameterFlag_MeterReadOnly`
/// (a meter the unit writes and the host only reads).
fn read_only_flags(writable: bool, meter_read_only: bool) -> ParamFlags {
    if writable && !meter_read_only {
        ParamFlags::empty()
    } else {
        ParamFlags::READ_ONLY
    }
}

/// A host-side error as the shared vocabulary's.
pub(crate) fn plugin_error(error: AuError) -> PluginError {
    match error {
        AuError::LoadFailed(failed) => PluginError::Load {
            stage: failed.stage,
            reason: failed.reason.clone(),
        },
        other => PluginError::Other(other.to_string()),
    }
}

fn editor_error(what: &str, error: impl std::fmt::Display) -> PluginError {
    PluginError::Editor(EditorError::PluginError(format!("{what}: {error}")))
}

/// The AU selector an id names, or `None` when it names none.
///
/// Only [`PresetId::Number`] addresses an AU preset. A VST3 `(list, index)`
/// pair and a CLAP path name nothing in AU's selector space, and coercing
/// either (taking the `index`, say) would load a real preset the caller never
/// asked for. That is silent, and worse than a refusal.
fn au_selector(id: &PresetId) -> Option<i32> {
    match id {
        PresetId::Number(n) => Some(*n),
        PresetId::Program { .. } | PresetId::Location(_) => None,
    }
}

impl AuLoaded {
    /// One direction's buses: AU's element 0 read as its one main bus (an AU
    /// marks no element a sidechain, and this host reads no other element).
    /// Its placement is the AU's own layout tag; `topology_of` declines the tags
    /// that name a processing relationship rather than speaker placement
    /// (MidSide, MatrixStereo, ambisonics), which arrive here as `None`.
    ///
    /// The width is the layout the stream configuration holds, which was checked
    /// when it was applied: no read of the unit, and nothing that can fail.
    pub(crate) fn buses(&self, direction: BusDirection) -> PluginBuses {
        let layout = self.configured_layout(direction);
        let placement = self
            .layout_tag(direction, 0)
            .ok()
            .and_then(crate::topology::topology_of);
        PluginBus::single(layout)
            .into_iter()
            .map(|bus| match &placement {
                Some(topology) => bus.placed(topology.clone()),
                None => bus,
            })
            .collect()
    }

    /// The capabilities this unit has, as the shared vocabulary names them.
    ///
    /// This AUv2 host is f32-only, single-bus, with a Cocoa editor, latency
    /// read-back, MIDI *input* and host transport callbacks. MIDI output,
    /// sample-accurate automation, note expression, sequencer context, f64 and
    /// host-driven editor resize are not offered.
    pub(crate) fn features(&self) -> Features {
        let mut features = Features::empty();
        let has_editor = AuEditor::has_editor(self.unit());
        features.set(Features::EDITOR, has_editor);
        features.set(Features::TRANSPORT, self.transport().is_some());
        // The process path routes MIDI to any AU whose component type
        // `receives_midi()`: reporting the same predicate here keeps one fact
        // from being answered twice.
        features.set(Features::MIDI_IN, self.au_type().receives_midi());
        // One property backs both bits: a unit that lists factory presets can be
        // asked to load any of them. An empty list is a genuine "none", not a
        // failed read.
        let has_presets = !self.factory_presets().is_empty();
        features.set(Features::PRESET_LIST, has_presets);
        features.set(Features::PRESET_LOAD, has_presets);
        features.set(Features::RENDER_MODE, self.render_mode_accepted());
        features
    }

    /// The tail the unit reports now, classified.
    ///
    /// AU is the one format that reports tail in *seconds*, and the one where a
    /// refusal is meaningful: every Apple instrument, mixer and generator rejects
    /// the property outright, which is "no tail concept", not "no tail". That is
    /// `Unknown`. An infinite tail (TAL Reverb 4) is asked about *before* the
    /// seconds-to-samples conversion, which maps every non-finite input to zero:
    /// converting first would make an infinite reverb indistinguishable from a
    /// plugin with no tail.
    pub(crate) fn tail_figure(&self) -> PluginTail {
        match self.get_tail_time() {
            Err(_) => PluginTail::Unknown,
            Ok(seconds) => PluginTail::from_seconds(seconds, self.config().sample_rate),
        }
    }

    /// The latency the unit reports now; a refusal answers the last figure that
    /// was read, so a unit that stops answering is not silently recompensated to
    /// zero (which would move audio).
    pub(crate) fn latency_figure(&self) -> Samples {
        match self.get_latency() {
            Ok(samples) => {
                self.last_latency().set(samples);
                samples
            }
            Err(_) => self.last_latency().get(),
        }
    }

    /// A plain value the unit reported, as a position: through the declared
    /// range, or, for a parameter off the table, as the plain value it was
    /// given.
    fn normalize(&self, id: u32, plain: f32) -> Normalized {
        match self.ranges.bounds(id) {
            Some(bounds) => bounds.normalized(f64::from(plain)),
            None => Normalized::clamped(f64::from(plain)),
        }
    }

    /// `value` in the declared plain units; `None` for a parameter off the
    /// table, which has no range to denormalize against.
    fn denormalize(&self, id: ParamId, value: Normalized) -> Option<f32> {
        Some(self.ranges.bounds(id.get())?.word(value))
    }

    /// Reads the declared ranges again, after the unit said its parameter list
    /// moved. The table is what a block denormalizes automation against, so a
    /// list that grew would leave new parameters unwritable.
    pub(crate) fn reread_ranges(&mut self) {
        let previous = Arc::clone(self.ranges.marks());
        self.ranges = RangeTable::read(self.unit(), Some(&previous));
        if !Arc::ptr_eq(&previous, self.ranges.marks()) {
            // The render may be setting bits in the old marks for a block yet.
            self.retired_marks.push(previous);
        }
        // A parameter the list gained has no subscription yet.
        let ids: Vec<u32> = self.ranges.ids().collect();
        if let Some(watch) = self.watch_mut() {
            watch.watch_parameters(ids);
        }
    }

    /// The plugin's edits since the last call, oldest first, as [`ParamEdit`]s:
    /// a value change normalized against the declared ranges (a parameter off
    /// the table, which the AU grew and the host has not re-read, is reported as
    /// the plain value it was given, as the read path does), and a gesture's
    /// ends, which carry no value in AU.
    fn drain_edits(&self, out: &mut Vec<ParamEdit>) {
        let mut queued = Vec::new();
        self.edits.drain(&mut queued);
        out.extend(queued.into_iter().filter_map(|edit| match edit {
            Edit::Begin(id) => Some(ParamEdit::Begin(ParamId::new(id))),
            Edit::End(id) => Some(ParamEdit::End(ParamId::new(id))),
            // The unit repeating the narrowing of a host write is not an edit.
            Edit::Change(id, plain) if self.ranges.absorbs_report(id, plain) => None,
            Edit::Change(id, plain) => Some(ParamEdit::change(
                ParamId::new(id),
                self.normalize(id, plain),
            )),
        }));
    }

    /// Forgets every written position: the unit set its own state (a preset, a
    /// document, a reset), so no value it reports is a host write repeated.
    pub(crate) fn clear_written(&self) {
        self.ranges.marks().clear_written();
    }

    /// The marks the render sets bits in: the current table's, and every
    /// earlier one the render may not have let go of.
    pub(crate) fn live_marks(&self) -> impl Iterator<Item = &Arc<Marks>> {
        std::iter::once(self.ranges.marks()).chain(&self.retired_marks)
    }

    /// Forgets the retired marks only this side holds: the render's last table
    /// that named them was reclaimed, so nothing sets a bit in them again.
    /// Call after draining them.
    pub(crate) fn prune_marks(&mut self) {
        self.retired_marks
            .retain(|marks| Arc::strong_count(marks) > 1);
    }

    pub(crate) fn range_snapshot(&self) -> RangeTable {
        self.ranges.clone()
    }
}

impl Control for AuLoaded {
    fn info(&self) -> &PluginDescriptor {
        self.descriptor()
    }

    fn wiring(&self, _main: MainThreadMarker) -> PluginWiring {
        PluginWiring {
            inputs: self.buses(BusDirection::Input),
            outputs: self.buses(BusDirection::Output),
            latency_samples: self.latency_figure(),
            tail: self.tail_figure(),
            features: self.features(),
            probed: probed::AU,
            // A unit without `kAudioUnitProperty_BypassEffect` (every
            // instrument) refuses the read.
            own_bypass: self.is_bypassed().is_ok().then_some(OwnBypass::Property),
            // A unit that does not implement the mapping family (most effects)
            // refuses the read and declares none.
            assignments: self
                .parameter_midi_mappings()
                .map(|table| crate::midi_map::declared(&table))
                .unwrap_or_default(),
        }
    }

    /// Every parameter the AU declares.
    fn parameters(&self, _main: MainThreadMarker) -> Vec<ParameterInfo> {
        let unit = self.unit();
        let params = parameters::list(unit);

        // Resolve each distinct clump once. `clump_name` is a property read into
        // the AU per call, and a synth with 400 parameters across 7 clumps would
        // otherwise pay 400 round trips for 7 answers.
        let mut clump_names: HashMap<u32, String> = HashMap::new();
        for clump in params.iter().filter_map(|p| p.clump) {
            clump_names
                .entry(clump)
                .or_insert_with(|| parameters::clump_name(unit, clump).unwrap_or_default());
        }

        // `ParametersForOverview` is the unit's own shortlist; a unit that does
        // not implement it marks none.
        let overview: Vec<u32> = self
            .parameters_for_overview()
            .map(|list| {
                list.into_iter()
                    .filter(|o| o.scope == Scope::GLOBAL.raw())
                    .map(|o| o.id)
                    .collect()
            })
            .unwrap_or_default();

        params
            .into_iter()
            .map(|p| {
                // Indexed params are a choice list whose `[min, max]` are the
                // first and last index, so the position count comes from the
                // span: AUTimePitch's "Overlap" is 0..10, eleven positions.
                let steps = match p.unit {
                    parameters::ParameterUnit::Boolean => ParamSteps::Toggle,
                    parameters::ParameterUnit::Indexed => {
                        ParamSteps::from_span((p.range.max - p.range.min) as f64)
                    }
                    _ => ParamSteps::Continuous,
                };
                // AUv2 advertises IsWritable and nothing else. Writability is
                // not automatability, so only READ_ONLY is known.
                // `p.clump` is `None` unless the AU set
                // `kAudioUnitParameterFlag_HasClump`, so an AU that declared no
                // clump cannot arrive here as clump 0. A clump the AU declines
                // to name yields no group rather than a numeric placeholder.
                let group = p
                    .clump
                    .and_then(|c| clump_names.get(&c))
                    .cloned()
                    .unwrap_or_default();
                let flags = read_only_flags(p.writable, p.meter_read_only);
                // `AudioUnitParameterID` is AU's opaque plugin-chosen handle.
                ParameterInfo::new(ParamId::new(p.id), p.name)
                    .with_unit(p.unit.to_string())
                    .with_range(PluginParamRange::Plain {
                        min: p.range.min as f64,
                        max: p.range.max as f64,
                        default: p.range.default as f64,
                    })
                    .with_steps(steps)
                    .with_plain_map(ParamBounds::of(p.range.min, p.range.max).plain_map())
                    .with_flags(ParamFlags::READ_ONLY, flags)
                    .with_group(group)
                    // The unit code is a closed enumeration, so it types the
                    // parameter exactly.
                    .with_measures(crate::unit_measures(p.unit_code))
                    .with_overview(overview.contains(&p.id))
            })
            .collect()
    }

    /// What `kAudioUnitProperty_BypassEffect` holds now.
    fn bypassed(&self, _main: MainThreadMarker) -> Option<bool> {
        self.is_bypassed().ok()
    }

    /// Writes `kAudioUnitProperty_BypassEffect`: an AU's bypass is a unit
    /// property, never a parameter. A unit that refuses the write loses it.
    fn set_bypassed(&mut self, _main: MainThreadMarker, bypassed: bool) -> BypassWrite {
        match self.set_bypass(bypassed) {
            Ok(()) => BypassWrite::Sent,
            Err(_) => BypassWrite::Lost,
        }
    }

    /// Plain, in the AU's own units: `ParamBounds`'s [`ParamMapping`] over the
    /// declared range (see it for why the display curves do not apply). `None`
    /// for an id the AU never declared.
    fn parameter_plain(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<f64> {
        Some(self.ranges.bounds(id.get())?.plain(value))
    }

    /// The inverse, [`ParamMapping::normalized`].
    fn parameter_normalized(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized> {
        Some(self.ranges.bounds(id.get())?.normalized(plain))
    }

    /// Normalized `0..=1`: `AudioUnitGetParameter` answers in plain units, so
    /// the declared range maps it back. `None` for an id the AU refuses.
    fn parameter(&self, _main: MainThreadMarker, id: ParamId) -> Option<Normalized> {
        let plain = parameters::get(self.unit(), id.get()).ok()?;
        Some(self.normalize(id.get(), plain))
    }

    /// Applied now (`AudioUnitSetParameter` is callable from any thread, so an
    /// active unit takes it directly rather than through a queue to the
    /// Processor). Normalized in, denormalized against the same table the
    /// automation path uses, so the two cannot disagree about a range.
    fn set_parameter(
        &mut self,
        main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Result<(), ParamWriteError> {
        self.parameter_writable(main, id)?;
        // Declared, so the table has its range.
        if let Some(plain) = self.denormalize(id, value) {
            // Recorded once the unit took the write: a refused one repeats nothing.
            if parameters::set(self.unit(), id.get(), plain).is_ok()
                && let Some(slot) = self.ranges.slot(id.get())
            {
                self.ranges.marks().record(slot, value);
            }
        }
        Ok(())
    }

    /// From the range table, which holds every declared parameter and the ids
    /// the AU declared read-only.
    fn parameter_writable(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
    ) -> Result<(), ParamWriteError> {
        ParamWriteError::check(id, self.ranges.access(id.get()))
    }

    /// The AU's own display string. `AudioUnitParameterStringFromValue` takes
    /// the value in **plain** units, so the range converts first: asking a
    /// `[10, 22050]` Hz cutoff to describe `1.0` would answer `"1 Hz"`, a
    /// plausible string for a value the caller never named.
    ///
    /// Measured on macOS 15.6: no Apple AU implements the property; third-party
    /// AUs do.
    fn parameter_text(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        value: Normalized,
    ) -> Option<String> {
        parameters::string_from_value(self.unit(), id.get(), self.denormalize(id, value)?)
    }

    /// The inverse, re-normalized so the result can be handed straight to
    /// [`set_parameter`](Control::set_parameter).
    fn parameter_value_from_text(
        &self,
        _main: MainThreadMarker,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        let plain = parameters::value_from_string(self.unit(), id.get(), text)?;
        Some(self.normalize(id.get(), plain))
    }

    fn save_state(&mut self, _main: MainThreadMarker, _: StateUse) -> PluginResult<Vec<u8>> {
        self.class_info()
            .map_err(|e| PluginError::State(e.to_string()))
    }

    fn load_state(
        &mut self,
        _main: MainThreadMarker,
        bytes: &[u8],
        _: StateUse,
    ) -> PluginResult<()> {
        self.set_class_info(bytes)
            .map_err(|e| PluginError::State(e.to_string()))
    }

    /// The AU's factory presets. `AuPreset::number` is a **unit-assigned
    /// selector**, not a position, so the id is built from it and never from the
    /// enumeration index. `bank` is `None`: AU exposes one flat factory set.
    fn presets(&mut self, _main: MainThreadMarker) -> Vec<PluginPreset> {
        self.factory_presets()
            .into_iter()
            .map(|p| PluginPreset::new(PresetId::Number(p.number), p.name))
            .collect()
    }

    /// `false` for an id this format cannot address, and for a number the unit
    /// does not advertise (the AU is still renderable afterwards, parameters
    /// untouched).
    fn load_preset(&mut self, _main: MainThreadMarker, id: &PresetId) -> bool {
        match au_selector(id) {
            Some(number) => self.load_factory_preset(number).is_ok(),
            None => false,
        }
    }

    /// `None` when the unit does not implement `kAudioUnitProperty_PresentPreset`:
    /// there is no honest value to fabricate.
    fn current_preset(&mut self, _main: MainThreadMarker) -> Option<PresetId> {
        self.current_preset_figure()
    }

    fn rescan_parameters(&mut self, _main: MainThreadMarker, scope: RescanScope) {
        if matches!(scope, RescanScope::Info | RescanScope::All) {
            self.reread_ranges();
        }
    }

    fn take_requests(&mut self, _main: MainThreadMarker, out: &mut Vec<Request>) {
        out.extend(taken(&self.requests));
    }

    /// `OnActivation` where the unit takes `kAudioUnitProperty_OfflineRender`,
    /// `Unsupported` where it refuses it (the property is optional). A unit
    /// that sizes an oversampling or look-ahead buffer from the flag does so at
    /// `AudioUnitInitialize`, so the mode is written before each activation.
    fn render_mode_support(&self, _main: MainThreadMarker) -> RenderModeSupport {
        if self.render_mode_accepted() {
            RenderModeSupport::OnActivation
        } else {
            RenderModeSupport::Unsupported
        }
    }

    /// The plugin's own edits (its editor moving a knob, with the gesture around
    /// it), heard through the parameter listener `watch_properties` installs.
    /// The host's own writes are not among them: they name the listener as their
    /// sender.
    fn take_reported_edits(&mut self, _main: MainThreadMarker, out: &mut Vec<ParamEdit>) {
        self.drain_edits(out);
    }

    fn dropped_reported_edits(&self) -> u64 {
        self.edits.dropped()
    }

    /// Audio Unit has no host-side main-thread refusal: none is counted.
    fn refused_off_thread_calls(&self) -> tutti_units::Calls {
        tutti_units::Calls::ZERO
    }
}

impl AuLoaded {
    fn current_preset_figure(&self) -> Option<PresetId> {
        self.current_preset()
            .ok()
            .map(|p| PresetId::Number(p.number))
    }
}

impl EditorWindow for AuLoaded {
    fn has_editor(&self, _main: MainThreadMarker) -> bool {
        // Asks the live unit for its Cocoa view property: the AU host takes no
        // token for the read, and an uninitialized unit answers it.
        AuEditor::has_editor(self.unit())
    }

    /// `_scale` is unused: an AU view is an `NSView`, which lays out in logical
    /// pixels (points) and reads its backing scale itself, so the size it returns
    /// is a [`Size::Logical`]. `preferred` is a *hint* to the
    /// view factory (`AUCocoaUIView.h:47-48`); this entry point receives no size,
    /// so it offers 800 by 600 and returns the size the view actually made.
    /// Re-opening closes the previous view first, so the AU is never asked to
    /// hold two.
    fn open_editor(
        &mut self,
        main: MainThreadMarker,
        parent: WindowHandle,
        _scale: ContentScale,
    ) -> PluginResult<tutti_plugin_types::EditorOpened<Size>> {
        self.close_editor(main);
        let editor = AuEditor::open(main, self.unit(), Some(parent), {
            let (width, height) = tutti_plugin_types::EditorUnit::DEFAULT_EXTENT;
            LogicalSize::new(width, height)
        })
        .map_err(|e| editor_error("AU open_editor failed", e))?;
        let size = editor.editor_size(main);
        *self.editor_slot() = Some(editor);
        Ok(tutti_plugin_types::EditorOpened::new(size.into()))
    }

    /// Detaches and releases the editor view. The teardown is AppKit's
    /// `removeFromSuperview` / `release`, which the `MainThreadMarker` proves is on
    /// the main thread.
    fn close_editor(&mut self, main: MainThreadMarker) {
        if let Some(mut editor) = self.editor_slot().take() {
            editor.close(main);
        }
    }

    fn editor_idle(&mut self, _main: MainThreadMarker) {
        // AUv2 Cocoa views are driven by the AppKit run loop.
    }

    fn editor_capabilities(&mut self, _main: MainThreadMarker) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    fn set_editor_size(&mut self, _main: MainThreadMarker, _requested: Size) -> PluginResult<Size> {
        Err(PluginError::Editor(EditorError::PluginError(
            "set_editor_size not supported".into(),
        )))
    }

    fn poll_editor_resize_request(&mut self, _main: MainThreadMarker) -> Option<Size> {
        None
    }
}

impl Wraps for AuController {
    type Plugin = AuLoaded;

    fn plugin(&self) -> &AuLoaded {
        &self.loaded
    }

    fn plugin_mut(&mut self) -> &mut AuLoaded {
        &mut self.loaded
    }

    /// Reads the ranges again and publishes them to the Processor.
    fn republish_ranges(&mut self, _main: MainThreadMarker) {
        AuController::republish_ranges(self);
    }

    /// Applies the writes the Processor handed over for the parameters the AU
    /// flagged `NonRealTime`, latest value of each in order: they are written
    /// here, between blocks, and never from the render. Also the range table's
    /// reclaim: a retired table is let go on this tick even when the parameters
    /// go quiet and nothing republishes.
    fn tick(&mut self, _main: MainThreadMarker) {
        self.ranges.reclaim();
        self.apply_deferred();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only [`PresetId::Number`] addresses an AU preset: a VST3 program id or a
    /// CLAP path must not coerce into a selector and load a preset the caller
    /// never asked for.
    #[test]
    fn an_id_from_another_format_addresses_no_au_preset() {
        assert_eq!(au_selector(&PresetId::Number(7)), Some(7));
        assert_eq!(
            au_selector(&PresetId::Program {
                list_id: 0,
                index: 1
            }),
            None,
            "a VST3 program id must not yield an AU selector"
        );
        assert_eq!(
            au_selector(&PresetId::Location("/x.clap-preset".into())),
            None,
            "a CLAP preset path must not yield an AU selector"
        );
    }
}

#[cfg(test)]
mod flag_tests {
    use super::*;

    /// A writable parameter is not read-only; one that is not writable is; a meter is,
    /// whatever its writable bit says.
    /// Spec: au.param.flag.MeterReadOnly
    #[test]
    fn a_meter_parameter_is_read_only_even_when_it_is_writable() {
        assert_eq!(read_only_flags(true, false), ParamFlags::empty());
        assert_eq!(read_only_flags(false, false), ParamFlags::READ_ONLY);
        assert_eq!(read_only_flags(true, true), ParamFlags::READ_ONLY);
        assert_eq!(read_only_flags(false, true), ParamFlags::READ_ONLY);
    }
}
