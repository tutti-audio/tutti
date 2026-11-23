//! The cases. Each is a plain generic function over a [`Format`]; `contract!`
//! makes a test of each. Expected figures come from [`spec`].

use crate::active::main_thread;
use crate::transcript::select_tail;
use tutti_buffers::FloatWidth;
use tutti_buffers::InputOutput;
use tutti_graph::Offset;
use tutti_graph::UmpAt;
use tutti_plugin_types::{
    BusRole, BypassWrite, Control, Controller, Loaded, MidiAssignment, MidiMapApplied, OwnBypass,
    ParamFlags, ParamWriteError, ParameterChanges, PluginBus, PluginIo, PluginLearnError,
    PluginPreset, PluginTail, PluginWiring, PresetId, ProcessContext, ProcessOutput, Processor,
    Reopen, Samples, TransportChange, TransportInfo,
};
use tutti_plugin_types::{ParamEdit, ParamMapping, PlainMap, Request};
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, NoteVelocity};
use tutti_units::Normalized;

use crate::spec::{self, Mode, Param};
use crate::{
    Active, BLOCK, Format, Kind, ParamTiming, RATE, Render, prepare, select, set_level, set_mode,
    transcript,
};
use crate::{CLOSED_ON_CREATOR, close_record_path, state_with_fault};
use tutti_fixtures::midi;

use tutti_ump::U7;

fn read<C: Control + ?Sized>(c: &C, param: Param) -> Option<f64> {
    c.parameter(main_thread(), param.id()).map(Normalized::get)
}

fn write<C: Control + ?Sized>(c: &mut C, param: Param, value: f64) {
    c.set_parameter(main_thread(), param.id(), Normalized::clamped(value))
        .unwrap_or_else(|why| panic!("writing {param:?}: {why}"));
}

fn activate<L: Loaded>(loaded: L, block: usize) -> Active<L> {
    Active::activate(loaded, prepare(RATE, block, FloatWidth::F32))
        .unwrap_or_else(|(_, why)| panic!("activation refused: {why:?}"))
}

fn active_in<F: Format>(mode: Mode) -> Active<F::L> {
    active_with::<F>(mode, BLOCK, FloatWidth::F32)
}

fn active_with<F: Format>(mode: Mode, block: usize, format: FloatWidth) -> Active<F::L> {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, mode);
    Active::activate(loaded, prepare(RATE, block, format))
        .unwrap_or_else(|(_, why)| panic!("activation refused: {why:?}"))
}

fn silence(frames: usize) -> [Vec<f32>; 2] {
    [vec![0.0; frames], vec![0.0; frames]]
}

fn ramp(frames: usize, base: f32) -> Vec<f32> {
    (0..frames).map(|i| base + i as f32 * 0.001).collect()
}

fn note(offset: usize, on: bool) -> UmpAt {
    let (group, channel) = (MidiGroup::FIRST, MidiChannel::of(0));
    let ump = if on {
        // 7-bit 100, upscaled (M2-115).
        midi::note_on(
            tutti_ump::Address::new(group, channel),
            60,
            NoteVelocity::from(U7::of(100)).get(),
        )
    } else {
        midi::note_off(tutti_ump::Address::new(group, channel), 60, 0)
    };
    let offset = Offset::of(Samples::new(offset), Samples::new(BLOCK));
    (offset, ump)
}

fn changes(points: &[(Param, i32, f64)]) -> ParameterChanges {
    let mut changes = ParameterChanges::reserved(points.len(), 4);
    for &(param, offset, value) in points {
        let offset = usize::try_from(offset).expect("a case's offset is not negative");
        let offset = Offset::of(Samples::new(offset), Samples::new(BLOCK));
        let value = Normalized::new(value).expect("a case's value is normalized");
        assert!(changes.add_change(param.id(), offset, value));
    }
    changes
}

/// Each input channel lands in its own output slot: `out = in + tag`.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.plugin.process, vst3.IAudioProcessor.process, vst2.field.processReplacing, au.select.AudioUnitRender
pub fn routing_tag_oracle<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    let ins = [ramp(BLOCK, 0.25), ramp(BLOCK, 0.5)];
    let out = a.render(&ins, BLOCK, &ProcessContext::new());
    for c in 0..2 {
        for i in 0..BLOCK {
            assert_eq!(out[c][i], ins[c][i] + spec::tag(c), "ch{c} frame {i}");
        }
    }
}

/// A rate and block change reaches the plugin and rendering goes on.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn reconfigure_keeps_rendering<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    a.silent(BLOCK);
    a.reconfigure(prepare(44_100.0, 128, FloatWidth::F32))
        .expect("the reference plugin takes 44.1k");
    let out = a.render(&silence(128), 128, &ProcessContext::new());
    assert!(out[0].iter().all(|&v| v == spec::tag(0)));
    assert!(out[1].iter().all(|&v| v == spec::tag(1)));
    let t = transcript::<F, _>(a.controller_mut());
    assert_eq!((t.rate, t.max_block), (44_100.0, 128), "what it was told");
    assert_eq!(t.prepares, 2);
    assert_eq!((t.calls, t.frames), (1, 128), "counted from the activation");
    assert_eq!((t.level, t.aux), (0.5, 0.25), "the defaults");
}

/// The plugin's buses as the host reports them, loaded and active: one stereo
/// main bus a side, with the role the format gives it.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.audio_ports.count, clap.ext.audio_ports.get, vst3.IComponent.getBusCount, vst3.IComponent.getBusInfo, vst2.field.numInputs, vst2.field.numOutputs
pub fn buses_reported<F: Format>() {
    let main = main_thread();
    let want = [(spec::MAIN_BUS, BusRole::Main)];
    let of =
        |buses: &[PluginBus]| -> Vec<_> { buses.iter().map(|b| (b.channels, b.role)).collect() };
    let loaded = F::load().wiring(main);
    assert_eq!(of(&loaded.inputs), want, "loaded inputs");
    assert_eq!(of(&loaded.outputs), want, "loaded outputs");
    let active = active_in::<F>(Mode::Tag).controller().wiring(main);
    assert_eq!(of(&active.inputs), want, "active inputs");
    assert_eq!(of(&active.outputs), want, "active outputs");
}

/// Reported latency follows the plugin's mode.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.latency.get, vst3.IAudioProcessor.getLatencySamples, vst2.field.initialDelay, au.prop.Latency
pub fn latency_reported<F: Format>() {
    assert_eq!(
        active_in::<F>(Mode::Delay)
            .controller()
            .latency(main_thread()),
        spec::LATENCY
    );
    assert_eq!(
        active_in::<F>(Mode::Tag)
            .controller()
            .latency(main_thread()),
        Samples::new(0)
    );
}

/// The reported latency is the delay the plugin applies, across blocks.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn latency_applied<F: Format>() {
    let mut a = active_with::<F>(Mode::Delay, 128, FloatWidth::F32);
    let frames = 100;
    let mut impulse = vec![0.0f32; frames];
    impulse[0] = 1.0;
    let mut heard = Vec::new();
    for block in 0..4 {
        let ins = if block == 0 {
            [impulse.clone(), impulse.clone()]
        } else {
            silence(frames)
        };
        heard.extend_from_slice(&a.render(&ins, frames, &ProcessContext::new())[0]);
    }
    for (i, &v) in heard.iter().enumerate() {
        let want = spec::tag(0) + if i == spec::LATENCY.get() { 1.0 } else { 0.0 };
        assert_eq!(v, want, "frame {i}");
    }
}

/// A reset makes the plugin forget its signal history: an impulse in the delay line before the
/// reset never comes out after it.
/// The oracle is the spec's own delay, [`spec::LATENCY`]; the silence after the
/// reset is longer than it, so a remembered impulse would be heard.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.plugin.reset, vst3.IAudioProcessor.setProcessing, vst2.eff.StopProcess, au.select.AudioUnitReset
pub fn reset_clears_the_delay_line<F: Format>() {
    let mut a = active_with::<F>(Mode::Delay, 128, FloatWidth::F32);
    let frames = 100;
    let mut impulse = vec![0.0f32; frames];
    impulse[0] = 1.0;
    // Still in the delay line: it would come out at frame 137.
    a.render(&[impulse.clone(), impulse], frames, &ProcessContext::new());
    a.processor().reset();
    let mut heard = Vec::new();
    for _ in 0..4 {
        heard.extend_from_slice(&a.render(&silence(frames), frames, &ProcessContext::new())[0]);
    }
    assert!(heard.len() > spec::LATENCY.get(), "longer than the delay");
    for (i, &v) in heard.iter().enumerate() {
        assert_eq!(
            v,
            spec::tag(0),
            "the impulse came out after the reset, at frame {i}"
        );
    }
}

/// The tail the plugin reports.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.tail.get, vst3.IAudioProcessor.getTailSamples, vst2.eff.GetTailSize, au.prop.TailTime
pub fn tail_reported<F: Format>() {
    assert_eq!(
        active_in::<F>(Mode::Tag).controller().tail(main_thread()),
        PluginTail::Finite(spec::TAIL)
    );
}

/// **The format host reports the plugin's tail whole**: a tail past what the
/// graph bounds (`MAX_NODE_TAIL`, 60 s) is the finite figure the plugin gave,
/// not a clamped one (a clamp here would cut a longer ring-out off), and the
/// format's infinite marker is `Unbounded`, not a huge finite count.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn tail_markers_reported<F: Format>() {
    let spec = F::SURFACE;
    if !spec.selects_tail {
        return;
    }
    let main = main_thread();
    let mut loaded = F::load();
    select_tail::<F, _>(&mut loaded, spec::TAIL_PAST_CAP);
    assert_eq!(
        loaded.wiring(main).tail,
        PluginTail::Finite(spec::TAIL_PAST_CAP_FRAMES),
        "a finite tail past the graph's bound"
    );
    if let Some(infinite) = spec.infinite_tail {
        select_tail::<F, _>(&mut loaded, spec::TAIL_INFINITE);
        assert_eq!(loaded.wiring(main).tail, infinite, "the infinite marker");
    }
}

/// The plugin lists its parameters with their names, defaults and, where the
/// format reports it, that they are automatable. Not step counts: CLAP's stepped
/// flag carries none, and VST2 has no steps.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.params.count, clap.ext.params.get_info, vst3.IEditController.getParameterCount, vst3.IEditController.getParameterInfo, vst2.field.numParams, vst2.eff.GetParamName, au.prop.ParameterList, au.prop.ParameterInfo
pub fn parameters_listed<F: Format>() {
    let mut got = F::load().parameters(main_thread());
    // The bypass parameter some formats list is the bypass cases' witness.
    got.retain(|p| p.id() != spec::BYPASS_ID);
    if F::SURFACE.preset_ids == spec::PresetIds::Programs {
        got.retain(|p| p.id() != spec::PROGRAM_ID);
    }
    got.sort_by_key(|p| p.id().get());
    assert_eq!(got.len(), Param::ALL.len(), "the listed parameters");
    for (info, &(param, default)) in got.iter().zip(&Param::ALL) {
        assert_eq!(info.id(), param.id());
        assert_eq!(info.name(), param.name(), "{param:?} name");
        if param == Param::Tone {
            // The default is the format's own plain value where it has one,
            // and the plugin's exact figure (not a point on a line).
            let tone = F::SURFACE.tone;
            assert_close::<F>(info.default_value(), tone.default, "tone default");
            assert_eq!(info.bounds(), tone.bounds, "tone bounds");
        } else {
            assert_eq!(info.default_value(), default, "{param:?} default");
        }
        // The plain map the format declares: its variant, and where it has a
        // formula, the spec's plain figures for `tone` from that map.
        let kind = if param == Param::Mode {
            F::SURFACE.mode_plain_map
        } else {
            F::SURFACE.plain_map
        };
        assert_eq!(
            match info.plain_map() {
                Some(PlainMap::Linear(_)) => spec::MapKind::Linear,
                Some(PlainMap::Stepped(_)) => spec::MapKind::Stepped,
                None => spec::MapKind::PluginDefined,
            },
            kind,
            "{param:?} plain map"
        );
        if param == Param::Tone
            && let Some(map) = info.plain_map()
        {
            let plain = F::SURFACE
                .tone
                .plain
                .expect("a format with a formula has figures");
            for (n, want) in spec::TONE_AT.into_iter().zip(plain) {
                assert_close::<F>(
                    map.plain(Normalized::of(n)),
                    want,
                    "tone from the declared map",
                );
            }
        }
        // The access the spec gives each parameter: the meter alone is read-only
        // and never automatable; the others are writable. A format that reports
        // no read-only bit (VST2) reports neither answer.
        if F::SURFACE.reports_access {
            assert_eq!(
                info.flag(ParamFlags::READ_ONLY),
                Some(param == Param::Meter),
                "{param:?} read-only"
            );
        }
        if F::SURFACE.reports_automatable {
            assert_eq!(
                info.flag(ParamFlags::AUTOMATABLE),
                Some(param != Param::Meter),
                "{param:?} automatable"
            );
        }
    }
}

/// The plugin lists its presets in the spec's order with the spec's names
/// ([`spec::PROGRAMS`]), and loading a listed preset makes the plugin's current
/// preset the one loaded.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: vst3.IUnitInfo.getProgramListCount, vst3.IUnitInfo.getProgramListInfo, vst3.IUnitInfo.getProgramName, vst2.eff.GetProgramNameIndexed, vst2.eff.SetProgram, vst2.eff.GetProgram, au.prop.FactoryPresets, au.prop.PresentPreset
pub fn presets_listed_in_order<F: Format>() {
    let main = main_thread();
    let mut loaded = F::load();
    let programs = &spec::PROGRAMS;
    let want: Vec<PluginPreset> = programs
        .names
        .iter()
        .zip(0u32..)
        .map(|(&name, index)| match F::SURFACE.preset_ids {
            spec::PresetIds::Programs => PluginPreset::in_bank(
                PresetId::Program {
                    list_id: programs.list_id,
                    index,
                },
                name,
                programs.list,
            ),
            spec::PresetIds::Numbered => PluginPreset::new(PresetId::Number(index as i32), name),
        })
        .collect();
    let listed = loaded.presets(main);
    assert_eq!(
        listed,
        want,
        "{}: the presets listed, in the spec's order",
        F::NAME
    );
    for preset in listed.iter().rev() {
        assert!(
            loaded.load_preset(main, &preset.id),
            "{}: the plugin takes the listed preset {}",
            F::NAME,
            preset.name
        );
        assert_eq!(
            loaded.current_preset(main),
            Some(preset.id.clone()),
            "{}: the current preset is the one loaded, {}",
            F::NAME,
            preset.name
        );
    }
}

/// A plugin with a bypass of its own says so in its wiring, and the host's
/// write of it reaches the plugin: `set_bypassed(true)` tells it `1`,
/// `set_bypassed(false)` tells it `0`, each in the next block, loaded or not
/// (the plugin's own transcript is the witness). Where the format keeps the
/// bypass in a parameter (VST3, CLAP) it is that parameter's point; an AU's is
/// its `BypassEffect` property, which the plugin tells as the same point. The
/// host reads the bypass back (a bypass it cannot read is not claimed), so every
/// write is confirmed against the plugin.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.params.get_info, vst3.IEditController.getParameterInfo, vst2.eff.SetBypass, au.prop.BypassEffect
pub fn own_bypass_is_declared_and_written<F: Format>() {
    let main = main_thread();
    let site = |wiring: tutti_plugin_types::PluginWiring, when: &str| {
        let site = wiring
            .own_bypass
            .unwrap_or_else(|| panic!("{}: the {when} plugin has its own bypass", F::NAME));
        // The bypass lives where the format keeps it: a parameter the spec lists
        // (VST3, CLAP), or not a parameter at all (AU).
        match (F::SURFACE.bypass, site) {
            (spec::Bypass::Parameter, OwnBypass::Parameter(id)) => {
                assert_eq!(id, spec::BYPASS_ID, "{}: the bypass parameter", F::NAME);
            }
            (spec::Bypass::Property, OwnBypass::Property) => {}
            (spec::Bypass::None, got) => panic!("{}: claims {got:?}, the spec says none", F::NAME),
            (want, got) => panic!("{}: the bypass is {got:?}, the spec says {want:?}", F::NAME),
        }
    };
    site(F::load().wiring(main), "loaded");
    let mut a = active_in::<F>(Mode::Tag);
    site(a.controller().wiring(main), "active");
    for (bypassed, want) in [(true, 1.0), (false, 0.0)] {
        assert_eq!(
            a.controller_mut().set_bypassed(main, bypassed),
            BypassWrite::Sent,
            "{}: the plugin takes a write of its bypass",
            F::NAME
        );
        a.silent(BLOCK);
        assert_eq!(
            a.controller().bypassed(main),
            Some(bypassed),
            "{}: the plugin reads its bypass back as written",
            F::NAME
        );
        let told = transcript::<F, _>(a.controller_mut());
        let last = told
            .events
            .iter()
            .rev()
            .find(|e| e.kind == Kind::Param && e.id == spec::BYPASS_ID.get())
            .unwrap_or_else(|| panic!("{}: the plugin was told its bypass", F::NAME));
        assert_eq!(last.value, want, "{}: bypassed = {bypassed}", F::NAME);
    }
}

/// A plugin's bypass is the node's mode, so it is not a listed control. Where the
/// format keeps it in a parameter the parameter is listed, flagged as the
/// bypass, and left out of `controls`; where it is a property (AU) there is no
/// parameter to leave out. The four other parameters are controls either way.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.params.get_info, vst3.IEditController.getParameterInfo, vst2.eff.SetBypass, au.prop.BypassEffect
pub fn the_bypass_parameter_is_not_a_listed_control<F: Format>() {
    let main = main_thread();
    let loaded = F::load();
    let listed = loaded.parameters(main);
    let controls = loaded.controls(main);
    let flagged: Vec<_> = listed
        .iter()
        .filter(|p| p.flag(ParamFlags::BYPASS) == Some(true))
        .map(|p| p.id())
        .collect();
    match F::SURFACE.bypass {
        spec::Bypass::Parameter => assert_eq!(
            flagged,
            [spec::BYPASS_ID],
            "{}: the plugin lists its bypass parameter, flagged",
            F::NAME
        ),
        spec::Bypass::Property | spec::Bypass::None => assert_eq!(
            flagged,
            [],
            "{}: the plugin lists no bypass parameter",
            F::NAME
        ),
    }
    // The program-change parameter VST3 lists is a control the host leaves in: it is the
    // preset selector, and `presets_listed_in_order` is its witness.
    let mut ids: Vec<_> = controls
        .iter()
        .map(|p| p.id())
        .filter(|&id| {
            !(F::SURFACE.preset_ids == spec::PresetIds::Programs && id == spec::PROGRAM_ID)
        })
        .map(|id| id.get())
        .collect();
    ids.sort_unstable();
    let want: Vec<_> = Param::ALL.iter().map(|(p, _)| p.id().get()).collect();
    assert_eq!(ids, want, "{}: the controls are the four", F::NAME);
}

/// What the plugin declares of a control reaches the host: `aux`'s group (and
/// no other's), the roles of VST3's function names, the AU overview's
/// shortlist, and the shape a control surface draws (`mode` is a
/// five-position selector, the others continuous), loaded and active alike.
/// Spec: clap.ext.params.get_info, vst3.IUnitInfo.getUnitInfo, vst3.IParameterFunctionName.getParameterIDFromFunctionName, au.prop.ParameterClumpName, au.prop.ParameterIDName, au.prop.ParametersForOverview, au.prop.ParameterInfo
pub fn a_control_carries_what_the_plugin_declares_of_it<F: Format>() {
    fn check<F: Format, C: Control + ?Sized>(c: &C, when: &str) {
        let spec = F::SURFACE;
        for info in c.controls(main_thread()) {
            // The preset selector of the program list, not a parameter of the plugin's sound.
            if F::SURFACE.preset_ids == spec::PresetIds::Programs && info.id() == spec::PROGRAM_ID {
                continue;
            }
            let (param, _) = *Param::ALL
                .iter()
                .find(|(p, _)| p.id() == info.id())
                .unwrap_or_else(|| panic!("{} ({when}): an unlisted control", F::NAME));
            let what = format!("{} {param:?} ({when})", F::NAME);
            let group = if param == Param::Aux {
                spec.aux_group
            } else {
                ""
            };
            assert_eq!(info.group(), group, "{what} group");
            assert_eq!(
                info.short_name(),
                if spec.short_names {
                    spec::short_name(param)
                } else {
                    ""
                },
                "{what} short name"
            );
            let role = spec
                .roles
                .iter()
                .find(|(p, _)| *p == param)
                .map(|&(_, role)| role);
            assert_eq!(info.role(), role, "{what} role");
            assert_eq!(
                info.overview(),
                spec.overview.contains(&param),
                "{what} overview"
            );
            let steps = if param == Param::Mode {
                spec.mode_steps
            } else {
                spec.continuous_steps
            };
            assert_eq!(info.steps(), steps, "{what} steps");
        }
    }
    let loaded = F::load();
    check::<F, _>(&loaded, "loaded");
    let active = activate(loaded, BLOCK);
    check::<F, _>(active.controller(), "active");
}

/// What the plugin declares in tables is listed in its wiring as data: the
/// controller table (VST3's `IMidiMapping`, Control Change 74 on channel 0
/// drives `level`; an AU's mapping table; none for CLAP and VST2), the names of
/// MIDI keys (CLAP's `note-name`, VST3's keyswitches, VST2's
/// `effGetMidiKeyName`) and the groups of parameters as a tree (VST3's units,
/// CLAP's module path).
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: vst3.IMidiMapping.getMidiControllerAssignment, au.prop.AllParameterMIDIMappings, clap.ext.note_name.get, vst3.IKeyswitchController.getKeyswitchInfo, vst2.eff.GetMidiKeyName, vst3.IUnitInfo.getUnitInfo, clap.ext.params.get_info
pub fn the_plugins_declared_tables_are_listed<F: Format>() {
    let loaded = F::load();
    assert_tables::<F>(&loaded.wiring(main_thread()), "loaded");
    let active = activate(loaded, BLOCK);
    assert_tables::<F>(&active.controller().wiring(main_thread()), "active");
}

/// The tables of `wiring` are the format's own ([`the_plugins_declared_tables_are_listed`]).
///
/// # Panics
///
/// Panics when a table differs.
pub fn assert_tables<F: Format>(wiring: &PluginWiring, when: &str) {
    let spec = F::SURFACE;
    assert_eq!(wiring.assignments, spec.assignments, "{}: {when}", F::NAME);
    let keys: Vec<_> = wiring
        .note_names
        .iter()
        .map(|n| {
            (
                n.key.get(),
                n.channel.map(MidiChannel::get),
                n.text.as_str(),
            )
        })
        .collect();
    assert_eq!(keys, spec.note_names, "{}: key names, {when}", F::NAME);
    let groups: Vec<_> = wiring
        .param_groups
        .iter()
        .map(|g| (g.path.as_str(), g.parent.as_deref()))
        .collect();
    assert_eq!(groups, spec.groups, "{}: groups, {when}", F::NAME);
}

/// `got` lists the entries of `want`, in any order: a unit hands its table back
/// unordered.
fn assert_entries(got: &[MidiAssignment], want: &[MidiAssignment], what: &str) {
    let held = got.len() == want.len() && want.iter().all(|w| got.contains(w));
    assert!(held, "{what}: got {got:?}, want {want:?}");
}

/// The app's controller entries are applied by who the format says
/// ([`spec::Surface::app_map`]): the unit of an AU holds them in its own table
/// beside the plugin's, an entry for a parameter the plugin's table drives takes
/// that entry's place for as long as it is there, and an empty set brings the
/// plugin's table back untouched. Every other format answers that the host
/// applies them and its table does not change.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: au.prop.AllParameterMIDIMappings, au.prop.AddParameterMIDIMapping, au.prop.RemoveParameterMIDIMapping
pub fn an_app_midi_map_is_applied_by_who_the_format_says<F: Format>() {
    fn check<F: Format, C: Control + ?Sized>(c: &mut C, when: &str) {
        let main = main_thread();
        let spec = F::SURFACE;
        let applied_by_unit = spec.app_map == MidiMapApplied::Plugin;
        let with = |app: &[MidiAssignment], replaces_declared: bool| -> Vec<MidiAssignment> {
            let mut table = if applied_by_unit && replaces_declared {
                Vec::new()
            } else {
                spec.assignments.to_vec()
            };
            if applied_by_unit {
                table.extend_from_slice(app);
            }
            table
        };
        let steps: [(&[MidiAssignment], bool); 3] = [
            (&[spec::APP_ENTRY], false),
            (&[spec::APP_ENTRY, spec::APP_OVERRIDE], true),
            (&[], false),
        ];
        for (app, replaces_declared) in steps {
            assert_eq!(
                c.set_app_midi_map(main, app),
                spec.app_map,
                "{}: who applies {app:?}, {when}",
                F::NAME
            );
            assert_entries(
                &c.wiring(main).assignments,
                &with(app, replaces_declared),
                &format!("{}: the table after {app:?}, {when}", F::NAME),
            );
        }
    }
    let mut loaded = F::load();
    check::<F, _>(&mut loaded, "loaded");
    let mut active = activate(loaded, BLOCK);
    check::<F, _>(active.controller_mut(), "active");
}

/// The plugin's own MIDI learn is armed where the plugin has one and refused as
/// unsupported where it has none ([`spec::Surface::plugin_learn`]). Where armed,
/// a live controller reaches the plugin from the main thread only after the
/// host's tick, and none reaches it once disarmed.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: vst3.IMidiLearn.onLiveMIDIControllerInput
pub fn the_plugins_own_midi_learn_is_armed_or_refused<F: Format>() {
    let main = main_thread();
    let controller = |c: u8| {
        let address = tutti_ump::Address::new(MidiGroup::FIRST, MidiChannel::of(0));
        let ump = midi::cc(address, CCNumber::of(c), 0x8000_0000);
        [(Offset::of(Samples::new(0), Samples::new(BLOCK)), ump)]
    };
    let mut a = active_in::<F>(Mode::Silent);
    match F::SURFACE.plugin_learn {
        spec::PluginLearn::Refused => {
            for armed in [true, false] {
                assert_eq!(
                    a.controller_mut().arm_plugin_midi_learn(main, armed),
                    Err(PluginLearnError::Unsupported),
                    "{}: arming {armed}",
                    F::NAME
                );
            }
        }
        spec::PluginLearn::MovesAux => {
            let aux = |a: &Active<F::L>| read(a.controller(), Param::Aux).expect("aux is listed");
            assert_eq!(aux(&a), 0.25, "aux starts at its default");
            assert_eq!(a.controller_mut().arm_plugin_midi_learn(main, true), Ok(()));
            let ctx_events = controller(spec::LEARNED_CONTROLLER);
            a.render(
                &silence(BLOCK),
                BLOCK,
                &ProcessContext::new().midi(&ctx_events),
            );
            assert_eq!(
                aux(&a),
                0.25,
                "the plugin is told from the main thread, not the block"
            );
            Control::flush_edits(a.controller_mut(), main);
            assert_eq!(aux(&a), f64::from(spec::LEARNED_CONTROLLER) / 127.0);
            assert_eq!(
                a.controller_mut().arm_plugin_midi_learn(main, false),
                Ok(())
            );
            let later = controller(spec::LEARNED_CONTROLLER + 1);
            a.render(&silence(BLOCK), BLOCK, &ProcessContext::new().midi(&later));
            Control::flush_edits(a.controller_mut(), main);
            assert_eq!(
                aux(&a),
                f64::from(spec::LEARNED_CONTROLLER) / 127.0,
                "a controller after the disarm is not told"
            );
        }
    }
}

/// `got` is `want` to the format's own representation (`Format::PARAM_VALUE_TOLERANCE`,
/// relative) and, at most, to the double's last places.
fn assert_close<F: Format>(got: f64, want: f64, what: &str) {
    assert!(
        (got - want).abs() <= want.abs() * (1e-12 + F::PARAM_VALUE_TOLERANCE),
        "{what}: got {got}, want {want}"
    );
}

/// A value read back is the one written, to the format's tolerance.
fn assert_reads<F: Format>(got: Option<f64>, want: f64, what: &str) {
    let got = got.unwrap_or_else(|| panic!("{what}: read nothing"));
    assert_close::<F>(got, want, what);
}

/// The conversion between a normalized value and the plugin's plain one is the
/// format's own, whatever shape the plugin gives it.
///
/// The figures are [`spec::Tone`]'s: VST3 answers the plugin's decreasing
/// exponential (`20000` at 0, `20` at 1, not the line between them), CLAP the
/// host's line over the range the plugin declares, and VST2, with no integer
/// range, has no plain value. Each plain value converts back to the value it
/// came from, and an id the plugin lacks has none, loaded and active.
pub fn parameter_conversion_is_the_formats_own<F: Format>() {
    fn check<F: Format, C: Control + ?Sized>(c: &C, when: &str) {
        let main = main_thread();
        let tone = F::SURFACE.tone;
        for (i, &n) in spec::TONE_AT.iter().enumerate() {
            let plain = c.parameter_plain(main, Param::Tone.id(), Normalized::of(n));
            let Some(want) = tone.plain else {
                assert_eq!(plain, None, "tone plain at {n} ({when})");
                assert_eq!(
                    c.parameter_normalized(main, Param::Tone.id(), 0.5),
                    None,
                    "tone normalized ({when})"
                );
                continue;
            };
            let plain = plain.unwrap_or_else(|| panic!("tone plain at {n} ({when}): none"));
            assert_close::<F>(plain, want[i], &format!("tone plain at {n} ({when})"));
            let back = c
                .parameter_normalized(main, Param::Tone.id(), plain)
                .unwrap_or_else(|| panic!("tone normalized at {plain} ({when}): none"));
            assert!(
                (back.get() - n).abs() < 1e-9,
                "tone normalized at {plain} ({when}): got {}, want {n}",
                back.get()
            );
        }
        // A value past an end reads as that end, an infinity included, and
        // NaN, which is past neither, reads as position 0 (`ParamMapping`).
        if let Some(want) = tone.plain {
            let rising = want[want.len() - 1] > want[0];
            let at = |plain: f64| {
                c.parameter_normalized(main, Param::Tone.id(), plain)
                    .unwrap_or_else(|| panic!("tone normalized at {plain} ({when}): none"))
                    .get()
            };
            assert_eq!(at(f64::NAN), 0.0, "tone at NaN ({when})");
            assert_eq!(
                at(f64::INFINITY),
                if rising { 1.0 } else { 0.0 },
                "tone at +inf ({when})"
            );
            assert_eq!(
                at(f64::NEG_INFINITY),
                if rising { 0.0 } else { 1.0 },
                "tone at -inf ({when})"
            );
        }
        assert_eq!(
            c.parameter_plain(main, spec::UNLISTED, Normalized::of(0.5)),
            None,
            "an unlisted id has a plain value ({when})"
        );
        assert_eq!(
            c.parameter_normalized(main, spec::UNLISTED, 0.5),
            None,
            "an unlisted id has a normalized value ({when})"
        );
    }
    let loaded = F::load();
    check::<F, _>(&loaded, "loaded");
    let a = activate(loaded, BLOCK);
    check::<F, _>(a.controller(), "active");
}

/// A parameter written loaded reads back, loaded and active.
/// Spec: clap.ext.params.get_value, vst3.IEditController.getParamNormalized, vst3.IEditController.setParamNormalized, vst2.field.getParameter, vst2.field.setParameter, au.select.AudioUnitGetParameter, au.select.AudioUnitSetParameter
pub fn parameter_round_trip<F: Format>() {
    let mut loaded = F::load();
    write(&mut loaded, Param::Aux, 0.75);
    assert_reads::<F>(read(&loaded, Param::Aux), 0.75, "loaded");
    let a = activate(loaded, BLOCK);
    assert_reads::<F>(read(a.controller(), Param::Aux), 0.75, "active");
}

/// An id the plugin lacks reads `None`.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn unlisted_parameter_is_none<F: Format>() {
    let loaded = F::load();
    assert_eq!(
        loaded.parameter(main_thread(), spec::UNLISTED),
        None,
        "loaded"
    );
    // The program-change parameter is listed by the formats that declare a program list, and by no
    // other: where it is not listed it reads `None`.
    if F::SURFACE.preset_ids != spec::PresetIds::Programs {
        assert_eq!(
            loaded.parameter(main_thread(), spec::PROGRAM_ID),
            None,
            "loaded: the program parameter, which the format does not list"
        );
    }
    let a = activate(loaded, BLOCK);
    assert_eq!(
        a.controller().parameter(main_thread(), spec::UNLISTED),
        None,
        "active"
    );
    if F::SURFACE.preset_ids != spec::PresetIds::Programs {
        assert_eq!(
            a.controller().parameter(main_thread(), spec::PROGRAM_ID),
            None,
            "active: the program parameter, which the format does not list"
        );
    }
}

/// A write to the parameter the plugin declares read-only (the meter) fails at
/// the call with `ReadOnly`, loaded and active, and does not reach the plugin:
/// beside an accepted write of `level`, which the plugin holds afterwards, the
/// plugin's record of what the host sent has no event for the meter. (A format
/// that delivers a write directly, as AU's `AudioUnitSetParameter` and VST2's
/// `setParameter` do, leaves no event for either, so the transcript half is
/// vacuous there and `Err(ReadOnly)` is the check.)
///
/// VST2 is not applicable by its spec: no opcode carries a read-only bit
/// (`effGetParameterProperties` has none), so the format has no read-only
/// parameter to refuse a write to ([`spec::Surface::reports_access`]).
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn read_only_parameter_write_is_refused<F: Format>() {
    if !F::SURFACE.reports_access {
        return;
    }
    let main = main_thread();
    let id = Param::Meter.id();
    let refused = Err(ParamWriteError::ReadOnly(id));
    let mut loaded = F::load();
    assert_eq!(
        loaded.set_parameter(main, id, Normalized::of(spec::WRITE)),
        refused,
        "read-only write refused (loaded)"
    );
    let mut a = activate(loaded, BLOCK);
    assert_eq!(
        a.controller_mut()
            .set_parameter(main, id, Normalized::of(spec::WRITE)),
        refused,
        "read-only write refused (active)"
    );
    write(a.controller_mut(), Param::Level, spec::WRITE);
    a.silent(BLOCK);
    tutti_plugin_types::Control::flush_edits(a.controller_mut(), main_thread());
    let t = transcript::<F, _>(a.controller_mut());
    assert_eq!(
        t.level,
        spec::WRITE,
        "the writable parameter took its write"
    );
    assert!(
        t.of(Kind::Param).all(|e| e.id != id.get()),
        "the refused write reached the plugin: {:?}",
        t.events
    );
}

/// Automation for the read-only parameter is not applied: a block whose
/// automation names the meter and the level moves the level and sends the
/// plugin no event for the meter, whichever route carries the points. (A format
/// that delivers a point directly leaves no event for either, so the transcript
/// half is vacuous there, as in [`read_only_parameter_write_is_refused`]; VST2
/// is not applicable for the same spec reason.)
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn automation_to_a_read_only_parameter_is_not_applied<F: Format>() {
    if !F::SURFACE.reports_access {
        return;
    }
    let mut a = active_in::<F>(Mode::Level);
    let points = changes(&[
        (Param::Meter, 0, spec::WRITE),
        (Param::Level, 0, spec::WRITE),
    ]);
    let out = a.render(
        &silence(BLOCK),
        BLOCK,
        &ProcessContext::new().params(&points),
    );
    assert!(
        out[0].iter().all(|&v| v == spec::WRITE as f32),
        "the writable parameter took its point"
    );
    tutti_plugin_types::Control::flush_edits(a.controller_mut(), main_thread());
    let t = transcript::<F, _>(a.controller_mut());
    assert!(
        t.of(Kind::Param).all(|e| e.id != Param::Meter.id().get()),
        "automation reached the read-only parameter: {:?}",
        t.events
    );
}

/// Automation for an id the plugin does not declare is not applied: a block
/// whose automation names it and the level moves the level and sends the plugin
/// no event for the undeclared id, whichever route carries the points. (AU and
/// VST2 deliver a point directly and leave no event for either id, so the
/// transcript half is vacuous there. AU's refusal is pinned by its own unit test,
/// `automation_for_an_undeclared_parameter_is_counted_not_written` in
/// `tutti-au-host`'s processor; VST2's is `Vst2Processor::write_automation`, which
/// turns an id outside `numParams` into a refusal before `setParameter` and
/// counts it.)
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn automation_to_an_undeclared_parameter_is_not_applied<F: Format>() {
    let mut a = active_in::<F>(Mode::Level);
    let mut points = ParameterChanges::reserved(2, 4);
    let write = Normalized::new(spec::WRITE).expect("the spec's write is normalized");
    assert!(points.add_change(spec::UNLISTED, Offset::ZERO, write));
    assert!(points.add_change(Param::Level.id(), Offset::ZERO, write));
    let out = a.render(
        &silence(BLOCK),
        BLOCK,
        &ProcessContext::new().params(&points),
    );
    assert!(
        out[0].iter().all(|&v| v == spec::WRITE as f32),
        "the declared parameter took its point"
    );
    tutti_plugin_types::Control::flush_edits(a.controller_mut(), main_thread());
    let t = transcript::<F, _>(a.controller_mut());
    assert!(
        t.of(Kind::Param).all(|e| e.id != spec::UNLISTED.get()),
        "automation reached an undeclared parameter: {:?}",
        t.events
    );
}

/// A write to an id the plugin does not declare fails at the call with a named
/// error, loaded and active (`ParamWriteError`, one policy for every format).
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn undeclared_parameter_write_is_refused<F: Format>() {
    let main = main_thread();
    let refused = Err(ParamWriteError::Undeclared(spec::UNLISTED));
    let mut loaded = F::load();
    assert_eq!(
        loaded.set_parameter(main, spec::UNLISTED, Normalized::of(0.25)),
        refused,
        "loaded"
    );
    let mut a = activate(loaded, BLOCK);
    assert_eq!(
        a.controller_mut()
            .set_parameter(main, spec::UNLISTED, Normalized::of(0.25)),
        refused,
        "active"
    );
}

/// A write to an active plugin is applied at the head of its next block.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn active_write_applied<F: Format>() {
    let mut a = active_in::<F>(Mode::Level);
    assert!(
        a.silent(BLOCK)[0].iter().all(|&v| v == 0.5),
        "default level"
    );
    write(a.controller_mut(), Param::Level, 0.75);
    let out = a.silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// A parameter point lands at its sample offset, or at the block head where
/// the format cannot place it.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.event.param_value, vst3.IParamValueQueue.getPoint, vst3.IParamValueQueue.getPointCount, au.select.AudioUnitScheduleParameters, au.param.event.Immediate
pub fn automation_point_offset<F: Format>() {
    let mut a = active_in::<F>(Mode::Level);
    let points = changes(&[(Param::Level, 32, 0.75)]);
    let ctx = ProcessContext::new().params(&points);
    let out = a.render(&silence(BLOCK), BLOCK, &ctx);
    for (i, &v) in out[0].iter().enumerate() {
        let moved = i >= 32 || F::PARAM_TIMING == ParamTiming::BlockHead;
        assert_eq!(v, if moved { 0.75 } else { 0.5 }, "frame {i}");
    }
}

/// A segment between two points moves the parameter linearly and a step jumps:
/// the points are linear breakpoints. A format that ramps natively receives the
/// segment as one ramp with its start value, end value, start offset and
/// length, and the step as a point; a format that takes points receives the
/// points.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.event.param_value, vst3.IParamValueQueue.getPoint, vst3.IParamValueQueue.getPointCount, au.select.AudioUnitScheduleParameters, au.param.event.Immediate, au.param.event.Ramped
pub fn automation_segment_and_step<F: Format>() {
    let [(from_at, from), (to_at, to)] = spec::SEGMENT;
    let step = spec::SEGMENT_STEP;
    let mut a = active_in::<F>(Mode::Level);
    let points = changes(&[
        (Param::Level, from_at, from),
        (Param::Level, to_at, to),
        (Param::Level, to_at, step),
    ]);
    let out = a.render(
        &silence(BLOCK),
        BLOCK,
        &ProcessContext::new().params(&points),
    );
    tutti_plugin_types::Control::flush_edits(a.controller_mut(), main_thread());
    let t = transcript::<F, _>(a.controller_mut());
    let sent: Vec<(u32, f64)> = t.of(Kind::Param).map(|e| (e.offset, e.value)).collect();
    let level = Param::Level.id().get();
    match F::PARAM_TIMING {
        ParamTiming::Ramped => {
            let ramps: Vec<_> = t
                .of(Kind::Ramp)
                .map(|e| {
                    let r = e.ramp.expect("a ramp carries its span");
                    (e.offset, e.id, e.value, r.duration, r.end)
                })
                .collect();
            assert_eq!(
                ramps,
                [(from_at as u32, level, from, (to_at - from_at) as u32, to)]
            );
            assert_eq!(sent, [(to_at as u32, step)], "the step is a point");
            let (param, default) = Param::ALL[0];
            assert_eq!(param, Param::Level);
            for (i, &v) in out[0].iter().enumerate() {
                let i = i as i32;
                let want = if i < from_at {
                    default as f32
                } else if i < to_at {
                    (from + (to - from) * f64::from(i - from_at) / f64::from(to_at - from_at))
                        as f32
                } else {
                    step as f32
                };
                assert_eq!(v, want, "frame {i}");
            }
        }
        ParamTiming::SampleAccurate => {
            assert_eq!(t.of(Kind::Ramp).count(), 0, "the host sent a ramp");
            assert_eq!(
                sent,
                [
                    (from_at as u32, from),
                    (to_at as u32, to),
                    (to_at as u32, step)
                ]
            );
        }
        ParamTiming::BlockHead => {
            assert_eq!(t.of(Kind::Ramp).count(), 0, "the host sent a ramp");
            assert_eq!(sent, [(0, step)]);
        }
    }
}

/// A note starts and stops at its sample offset.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.event.note_on, clap.event.note_off, vst3.IEventList.getEvent, vst2.eff.ProcessEvents, vst2.event.kVstMidiType, au.midi.MusicDeviceMIDIEventList
pub fn note_at_offset<F: Format>() {
    let mut a = active_in::<F>(Mode::Notes);
    let events = [note(10, true), note(40, false)];
    let out = a.render(&silence(BLOCK), BLOCK, &ProcessContext::new().midi(&events));
    for (i, (&c0, &c1)) in out[0].iter().zip(&out[1]).enumerate() {
        let held = (10..40).contains(&i);
        assert_eq!(c0, if held { 60.0 } else { 0.0 }, "ch0 frame {i}");
        assert_eq!(c1, if held { 1060.0 } else { 0.0 }, "ch1 frame {i}");
    }
}

/// What the host sent arrives as sent: events by offset, blocks by size.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn arrival_is_ordered_and_sized<F: Format>() {
    let mut a = active_in::<F>(Mode::Notes);
    let events = [note(5, true), note(20, false), note(30, true)];
    let points = changes(&[(Param::Aux, 3, 0.5), (Param::Aux, 25, 0.75)]);
    let ctx = ProcessContext::new().midi(&events).params(&points);
    a.render(&silence(BLOCK), BLOCK, &ctx);
    for frames in [1, 7, 33] {
        a.silent(frames);
    }
    // The control side's tick: a host that tells its listeners of the block's
    // parameters writes the unit here, and the transcript would show it.
    tutti_plugin_types::Control::flush_edits(a.controller_mut(), main_thread());
    let t = transcript::<F, _>(a.controller_mut());
    assert_eq!(t.blocks, [64, 1, 7, 33], "block sizes the host sent");
    for kind in [Kind::Param, Kind::NoteOn, Kind::NoteOff] {
        let offsets: Vec<u32> = t.of(kind).map(|e| e.offset).collect();
        assert!(
            offsets.is_sorted(),
            "{kind:?} arrived out of order: {offsets:?}"
        );
        assert!(
            offsets.iter().all(|&o| (o as usize) < BLOCK),
            "{kind:?} outside the block"
        );
    }
    assert_eq!(
        (t.calls, t.frames),
        (4, 105),
        "blocks and frames the host sent"
    );
    assert_eq!(
        (t.level, t.aux),
        (0.5, 0.75),
        "parameters as the events left them"
    );
    let points: Vec<(u32, f64)> = t.of(Kind::Param).map(|e| (e.offset, e.value)).collect();
    match F::PARAM_TIMING {
        ParamTiming::SampleAccurate => assert_eq!(points, [(3, 0.5), (25, 0.75)]),
        // One value per parameter at the head: the latest. This is stricter than
        // VST2 requires (two in-order `setParameter` calls would be legal), and it
        // cannot tell a host that keeps the last point on purpose from one that
        // loses the first by accident. Kept, so a change in either direction is seen.
        ParamTiming::BlockHead => assert_eq!(points, [(0, 0.75)]),
        ParamTiming::Ramped => {
            assert!(points.is_empty(), "the segment arrives as a ramp");
            let ramps: Vec<_> = t
                .of(Kind::Ramp)
                .map(|e| (e.offset, e.ramp.map(|r| (r.duration, r.end))))
                .collect();
            assert_eq!(ramps, [(3, Some((22, 0.75)))]);
        }
    }
    assert_eq!(t.of(Kind::NoteOn).count(), 2);
    assert_eq!(t.of(Kind::NoteOff).count(), 1);
}

/// A transport at `tempo` BPM and quarter note `beat`, rolling or not: the
/// one position every format reads.
fn transport(playing: bool, tempo: f64, beat: f64) -> TransportInfo {
    TransportInfo::new(tutti_plugin_types::SampleRate::of(RATE))
        .playing(playing)
        .tempo(tutti_units::Bpm::of(tempo))
        .beat(tutti_units::Beat::new(beat))
}

/// Each block's transport reaches the plugin at its first frame, as the host
/// was given it: rolling, then stopped, each with its tempo and position, both
/// flagged valid. The figures are exact in every format's encoding (CLAP's
/// beat is a fixed point of 2^31 per beat).
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn transport_reaches_the_plugin<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    let (rolling, stopped) = (transport(true, 132.0, 6.5), transport(false, 90.0, 12.0));
    a.render(
        &silence(BLOCK),
        BLOCK,
        &ProcessContext::new().transport(&rolling),
    );
    a.render(
        &silence(BLOCK),
        BLOCK,
        &ProcessContext::new().transport(&stopped),
    );
    let t = transcript::<F, _>(a.controller_mut());
    let heads: Vec<_> = t
        .transports
        .iter()
        .filter(|x| x.offset == 0)
        .map(|x| {
            (
                x.call,
                x.playing,
                x.tempo_valid,
                x.tempo,
                x.beat_valid,
                x.beat,
            )
        })
        .collect();
    assert_eq!(
        heads,
        [
            (0, true, true, 132.0, true, 6.5),
            (1, false, true, 90.0, true, 12.0)
        ],
        "the transport at each block's first frame"
    );
}

/// The host's automation mode reaches the plugin in the format's own channel
/// (VST3 `IAutomationState`, CLAP `param-indication`, VST2
/// `audioMasterGetAutomationState`): each mode is told, a block runs (a VST2
/// plugin asks once per block), and the plugin's transcript holds the raw state
/// the spec names for the mode as the last it was told.
///
/// AU is not applicable by its spec: no property or call of `AudioUnitProperties.h`,
/// `AUComponent.h` or `AUAudioUnit.h` tells a unit the host's automation mode, so
/// the format has nothing to be told ([`spec::Surface::automation`] is empty) and
/// the case asks only that the host does not claim to have told it.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.param_indication.set_automation, vst3.IAutomationState.setAutomationState, vst2.audioMaster.GetAutomationState
pub fn the_automation_mode_reaches_the_plugin<F: Format>() {
    use tutti_plugin_types::AutomationMode;
    if F::SURFACE.automation.is_empty() {
        let mut a = active_in::<F>(Mode::Tag);
        assert!(
            !a.controller_mut()
                .set_automation_state(main_thread(), AutomationMode::Writing),
            "the host claims to have told a format with no automation state"
        );
        return;
    }
    let mut a = active_in::<F>(Mode::Tag);
    let mut told_before = transcript::<F, _>(a.controller_mut()).automation_sent;
    for mode in [
        AutomationMode::Off,
        AutomationMode::Reading,
        AutomationMode::Writing,
        AutomationMode::ReadWriting,
        AutomationMode::Off,
    ] {
        assert!(
            a.controller_mut().set_automation_state(main_thread(), mode),
            "the plugin was not told {mode:?}"
        );
        a.silent(BLOCK);
        let t = transcript::<F, _>(a.controller_mut());
        let (_, raw) = F::SURFACE
            .automation
            .iter()
            .find(|(m, _)| *m == mode)
            .expect("the spec names the raw state of every mode");
        assert_eq!(t.automation_last, *raw, "{mode:?}: the last raw state told");
        assert!(
            t.automation_sent > told_before,
            "{mode:?}: the host told the plugin nothing"
        );
        told_before = t.automation_sent;
    }
}

/// A transport change inside a block is told at its frame, after the block's
/// own: CLAP has an event for it (`CLAP_EVENT_TRANSPORT`); VST3 and VST2 have
/// none, and are gaps.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.event.transport
pub fn transport_change_is_told_at_its_frame<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    let head = transport(true, 120.0, 4.0);
    let changes = [TransportChange {
        offset: 32,
        transport: transport(true, 140.0, 10.0),
    }];
    let ctx = ProcessContext::new()
        .transport(&head)
        .transport_changes(&changes);
    a.render(&silence(BLOCK), BLOCK, &ctx);
    let t = transcript::<F, _>(a.controller_mut());
    let told: Vec<_> = t
        .transports
        .iter()
        .map(|x| (x.call, x.offset, x.playing, x.tempo, x.beat))
        .collect();
    assert!(
        told.iter().any(|&(_, offset, ..)| offset == 32),
        "no transport at frame 32: {told:?}"
    );
    assert_eq!(
        told,
        [(0, 0, true, 120.0, 4.0), (0, 32, true, 140.0, 10.0)],
        "the block's transport, then the change at its frame"
    );
}

/// What a plugin's control surface answers, read through `&mut dyn Control`.
#[derive(Debug, PartialEq)]
struct Surfaced {
    has_editor: bool,
    render_mode: tutti_plugin_types::RenderModeSupport,
    reports_render_mode: bool,
    requests: Vec<Request>,
    edits: Vec<ParamEdit>,
}

/// Reads the surface: the editor, the render mode, and what the plugin raises
/// when a state load selects the notice (once, outside any block).
fn surfaced<F: Format>(c: &mut dyn Control) -> Surfaced {
    let main = main_thread();
    let mut requests = Vec::new();
    let mut edits = Vec::new();
    // What the setup itself raised (its own state loads) is not the witness.
    c.take_requests(main, &mut requests);
    c.take_reported_edits(main, &mut edits);
    requests.clear();
    edits.clear();
    if F::SURFACE.notices {
        select::<F, _>(
            c,
            if F::SURFACE.selects_editor {
                spec::EDITOR_TIDY
            } else {
                0
            },
            spec::NOTICE_ON_LOAD,
        );
    }
    c.take_requests(main, &mut requests);
    c.take_reported_edits(main, &mut edits);
    Surfaced {
        has_editor: c.has_editor(main),
        render_mode: c.render_mode_support(main),
        reports_render_mode: c
            .wiring(main)
            .features
            .contains(tutti_plugin_types::Features::RENDER_MODE),
        requests,
        edits,
    }
}

/// The control surface a shell reads through (`has_editor`, `render_mode_support`
/// and the `RENDER_MODE` feature that reports it,
/// `take_requests`, `take_reported_edits`) answers the same in a loaded plugin
/// and in an active one, and each answer is the spec's row for the format. A
/// format that has no editor, no render mode, or reports nothing outside a
/// block answers so in both states.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn the_control_surface_answers_alike_loaded_and_active<F: Format>() {
    let spec = F::SURFACE;
    let mut loaded = F::load();
    if spec.selects_editor && !spec.notices {
        select::<F, _>(&mut loaded, spec::EDITOR_TIDY, 0);
    }
    let on_loaded = surfaced::<F>(&mut loaded);
    let mut active = activate(loaded, BLOCK);
    let on_active = surfaced::<F>(&mut *active);

    let expected = Surfaced {
        has_editor: spec.has_editor,
        render_mode: spec.render_mode,
        reports_render_mode: spec.render_mode != tutti_plugin_types::RenderModeSupport::Unsupported,
        requests: spec.request.into_iter().collect(),
        edits: spec.edits.iter().map(|e| e.expected()).collect(),
    };
    assert_eq!(on_loaded, expected, "loaded");
    assert_eq!(on_active, expected, "active");
}

/// The gesture notice's edits ([`spec::GESTURE`]) as a host reads them.
#[must_use]
pub fn gesture_edits() -> Vec<ParamEdit> {
    spec::GESTURE.iter().map(|e| e.expected()).collect()
}

/// A gesture the plugin makes of its own accord is reported in the order it
/// made it, as the spec's four edits ([`spec::GESTURE`]): the begin, the two
/// changes with their values, the end, each for `aux`, and each to be recorded.
/// CLAP's plugin says it in the first block after the state load (its output
/// events); the others at the load, and a block after it adds nothing.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.event.param_gesture_begin, clap.event.param_gesture_end, clap.event.param_value, vst3.IComponentHandler.beginEdit, vst3.IComponentHandler.performEdit, vst3.IComponentHandler.endEdit, vst2.audioMaster.BeginEdit, vst2.audioMaster.Automate, vst2.audioMaster.EndEdit
pub fn a_gesture_is_reported_in_order<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    let main = main_thread();
    let mut setup = Vec::new();
    a.controller_mut().take_reported_edits(main, &mut setup);
    select::<F, _>(
        a.controller_mut(),
        if F::SURFACE.selects_editor {
            spec::EDITOR_TIDY
        } else {
            0
        },
        spec::NOTICE_GESTURE_ON_LOAD,
    );
    a.silent(BLOCK);
    let mut got = Vec::new();
    a.controller_mut().take_reported_edits(main, &mut got);
    assert_eq!(got, gesture_edits(), "the gesture, in order");

    // One load is one gesture: nothing more arrives from a quiet block.
    a.silent(BLOCK);
    let mut more = Vec::new();
    a.controller_mut().take_reported_edits(main, &mut more);
    assert_eq!(more, [], "the gesture is reported once");
}

/// State round-trips what the plugin persists.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
///
/// Spec: clap.ext.state.save, clap.ext.state.load, clap.stream.istream.read, clap.stream.ostream.write, vst3.IComponent.getState, vst3.IComponent.setState, vst3.IBStream.read, vst3.IBStream.write, vst2.eff.GetChunk, vst2.eff.SetChunk, au.prop.ClassInfo
pub fn state_round_trip<F: Format>() {
    let mut loaded = F::load();
    write(&mut loaded, Param::Level, 0.75);
    write(&mut loaded, Param::Aux, 0.5);
    let saved = loaded
        .save_state(main_thread(), tutti_plugin_types::StateUse::Project)
        .expect("saves");
    write(&mut loaded, Param::Level, 0.125);
    write(&mut loaded, Param::Aux, 0.0);
    loaded
        .load_state(main_thread(), &saved, tutti_plugin_types::StateUse::Project)
        .expect("loads");
    assert_reads::<F>(read(&loaded, Param::Level), 0.75, "level");
    assert_reads::<F>(read(&loaded, Param::Aux), 0.5, "aux");
}

/// A parameter written to a loaded plugin reaches its audio at the first block.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn loaded_write_reaches_the_audio<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    write(&mut loaded, Param::Level, 0.75);
    let out = activate(loaded, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// A parameter written to a loaded plugin and saved before it ever renders is
/// in the state; a restore of that state reads it back and renders it.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn loaded_write_survives_a_save<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    write(&mut loaded, Param::Level, 0.75);
    let saved = loaded
        .save_state(main_thread(), tutti_plugin_types::StateUse::Project)
        .expect("saves");
    let mut other = F::load();
    other
        .load_state(main_thread(), &saved, tutti_plugin_types::StateUse::Project)
        .expect("loads");
    assert_eq!(read(&other, Param::Level), Some(0.75), "read");
    let out = activate(other, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// A reopen is a fresh instance of the same plugin, from the same file: its
/// identity, its defaults and nothing of the one it came from, which it leaves
/// as it was; and it renders.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
// Not covered: a reopen that carries the original's state over (`reopen` takes
// `&self`, and saving a state takes `&mut`, so the impl cannot write one); the fresh-
// default read is what would catch it.
pub fn reopen_is_a_fresh_instance<F: Format>()
where
    F::L: Reopen,
{
    let mut loaded = F::load();
    write(&mut loaded, Param::Level, 0.75);
    let mut fresh = loaded.reopen(main_thread()).expect("reopens");
    assert_eq!(fresh.info(), loaded.info(), "the same plugin");
    let (_, default) = Param::ALL[0];
    assert_eq!(Param::ALL[0].0, Param::Level);
    assert_eq!(read(&fresh, Param::Level), Some(default), "the fresh one");
    assert_eq!(read(&loaded, Param::Level), Some(0.75), "the original");
    set_mode::<F, _>(&mut fresh, Mode::Level);
    let out = activate(fresh, BLOCK).silent(BLOCK);
    assert!(
        out[0].iter().all(|&v| f64::from(v) == default),
        "got {:?}",
        &out[0][..4]
    );
}

/// Bytes that are not the plugin's state leave it working, with the level it held.
///
/// The level and the mode go in through the state, so they sit in the component
/// itself: a host that carries a later write to the component's first block
/// cannot hide a plugin that loses them on a refusal.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn corrupt_state_leaves_plugin_usable<F: Format>() {
    let mut loaded = F::load();
    set_mode::<F, _>(&mut loaded, Mode::Level);
    set_level::<F, _>(&mut loaded, 0.75);
    let _refused = loaded.load_state(
        main_thread(),
        b"not the plugin's state",
        tutti_plugin_types::StateUse::Project,
    );
    assert_eq!(read(&loaded, Param::Level), Some(0.75));
    let out = activate(loaded, BLOCK).silent(BLOCK);
    assert!(out[0].iter().all(|&v| v == 0.75), "got {:?}", &out[0][..4]);
}

/// Halves of two activations do not rejoin.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn halves_of_two_activations_are_stuck<F: Format>() {
    let p = prepare(RATE, BLOCK, FloatWidth::F32);
    let (pa, ca) = F::load()
        .activate(main_thread(), p)
        .map_err(|r| r.reason)
        .expect("first");
    let (pb, cb) = F::load()
        .activate(main_thread(), p)
        .map_err(|r| r.reason)
        .expect("second");
    let stuck = <F::L as Loaded>::deactivate(main_thread(), pa, cb);
    assert!(stuck.is_err(), "halves of two activations joined");
    drop((stuck, pb, ca));
}

/// A plugin that declares 64-bit processing reports both widths before
/// activation, a 64-bit request settles on 64-bit, a 32-bit request is never
/// raised, and the activation at the settled width renders.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn widths_report_the_declared_precision<F: Format>() {
    let loaded = F::load();
    let widths = loaded.widths();
    assert!(
        widths.accepts(FloatWidth::F64),
        "the shared plugin declares 64-bit audio: widths {widths:?}"
    );
    assert!(widths.accepts(FloatWidth::F32), "every plugin takes 32-bit");
    assert_eq!(
        loaded.info().widths,
        Some(spec::WIDTHS),
        "the loaded plugin's descriptor carries its widths, as the scan record does"
    );
    let settled = widths.negotiate(FloatWidth::F64);
    assert_eq!(settled, FloatWidth::F64);
    assert_eq!(widths.negotiate(FloatWidth::F32), FloatWidth::F32);
    let mut a = Active::activate(loaded, prepare(RATE, BLOCK, settled))
        .unwrap_or_else(|(_, why)| panic!("activation at {settled:?} refused: {why:?}"));
    let ins = [vec![0.0f64; BLOCK], vec![0.0f64; BLOCK]];
    a.render(&ins, BLOCK, &ProcessContext::new());
}

/// 64-bit audio carries the same arithmetic, at 64 bits: 0.1 is not exact in 32.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn f64_carries_same_audio<F: Format>() {
    let mut a = active_with::<F>(Mode::Tag, BLOCK, FloatWidth::F64);
    let ins = [vec![0.1f64; BLOCK], vec![0.2f64; BLOCK]];
    let out = a.render(&ins, BLOCK, &ProcessContext::new());
    assert!(
        out[0].iter().all(|&v| v == 0.1 + 1.0),
        "ch0 {:?}",
        &out[0][..2]
    );
    assert!(
        out[1].iter().all(|&v| v == 0.2 + 2.0),
        "ch1 {:?}",
        &out[1][..2]
    );
}

/// A plugin whose log overflowed fails the oracle instead of passing on the
/// part it kept.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn overflowing_log_fails_the_oracle<F: Format>() {
    let mut a = active_in::<F>(Mode::Tag);
    for _ in 0..=spec::LOG_BLOCKS {
        a.silent(BLOCK);
    }
    let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        transcript::<F, _>(a.controller_mut())
    }));
    let why = read.expect_err("a transcript past the log's room is refused");
    let text = why
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| why.downcast_ref::<&str>().copied());
    assert!(
        text.is_some_and(|t| t.contains("log overflowed")),
        "{text:?}"
    );
}

/// Rendering a block allocates nothing (run under `AllocDisabler`).
///
/// A failure aborts the process, so under plain `cargo test` it takes the whole
/// test binary with it, and a [`Gap`](crate::Gap) cannot express it.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn process_does_not_allocate<F: Format>() {
    let mut a = active_in::<F>(Mode::Notes);
    let events = [note(5, true), note(20, false)];
    let points = changes(&[(Param::Aux, 3, 0.5)]);
    let ctx = ProcessContext::new().midi(&events).params(&points);
    let ins = silence(BLOCK);
    let refs = [ins[0].as_slice(), ins[1].as_slice()];
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut outs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: InputOutput::from_slices(Samples::new(BLOCK), &refs, &mut outs)
            .expect("equal channels"),
        ctx: &ctx,
        out: &mut out,
    };
    // The first block may size the host's buffers.
    a.processor().process(&mut io).expect("renders");
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..50 {
            a.processor().process(&mut io).expect("renders");
        }
    });
}

/// The last handle dropped off the main thread does not close the plugin
/// there: its close waits for the owner's main-thread tick and runs on the
/// thread that loaded it, and nothing is stranded.
///
/// The plugin is told through its state to record its close (the thread that
/// closed it against the thread that created it), the only witness once it is
/// gone.
///
/// # Panics
///
/// Panics when an assertion of the case fails.
pub fn the_last_handle_dropped_off_the_main_thread_closes_on_the_main_thread<F: Format>()
where
    F::L: 'static,
{
    let record = close_record_path();
    let _ = std::fs::remove_file(&record);
    let (work, handle) = tutti_plugin_types::MainWork::new();
    let mut loaded = F::load_with(&handle);
    loaded
        .load_state(
            main_thread(),
            &state_with_fault::<F>(spec::FAULT_RECORD_CLOSE),
            tutti_plugin_types::StateUse::Project,
        )
        .expect("the plugin takes its configuration");
    let stranded = tutti_plugin_types::MainWork::stranded();

    std::thread::spawn(move || drop(loaded))
        .join()
        .expect("the drop does not panic");
    assert!(
        !record.exists(),
        "the plugin closed before the main thread ran"
    );

    assert!(
        work.run(main_thread()) > 0,
        "the close was handed to the collector"
    );
    let closed = std::fs::read(&record).expect("the plugin recorded its close");
    let _ = std::fs::remove_file(&record);
    assert_eq!(
        closed,
        [CLOSED_ON_CREATOR],
        "the plugin closed on the thread that loaded it"
    );
    assert_eq!(
        tutti_plugin_types::MainWork::stranded(),
        stranded,
        "nothing stranded"
    );
}
