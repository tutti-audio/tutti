//! A hosted plugin's parameters through the session's views:
//! `Plugin::params`, `Param`, `Param::try_unit` and `ParamGesture`, over a fake
//! backend that declares its parameters and records what the host sets (no
//! `plugin-server`), hosted at a node over `host`.

use super::host;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::{
    AnyPlugin, DetachedSession, Key, PluginChange, PluginEvent, PluginParamRefusal, Session,
    SessionEvent, SetRefusal, UnitError, ValueError, ValueErrorKind,
};
use tutti_buffers::ChannelLayout;
use tutti_graph::{ForkByClone, NodeKey};
use tutti_nodes::testing::Const;
use tutti_plugin_host::{PluginControls, PluginHandle, RefreshSink};
use tutti_plugin_types::{
    AnyThread, AutomationMode, AutomationModeError, Health, LinearRange, ParamFlags, ParamUnit,
    ParameterInfo, PlainMap, PluginFailure, PluginRefresh, PluginTail, RenderMode,
};
use tutti_rt::LockExt;
use tutti_units::Normalized;
use tutti_units::ParamId;
use tutti_units::{Db, Hz, SampleRate, Samples, Seconds, Semitones};

const CUTOFF: ParamId = ParamId::new(1);
const LEVEL: ParamId = ParamId::new(2);
const MODE: ParamId = ParamId::new(3);
const LOCKED: ParamId = ParamId::new(4);
const TRANSPOSE: ParamId = ParamId::new(5);
const OPAQUE: ParamId = ParamId::new(6);

/// `cutoff` as the fake declares it: hertz over `20..=20000`, on a line.
fn cutoff_info() -> ParameterInfo {
    ParameterInfo::new(CUTOFF, "Cutoff")
        .with_plain_range(20.0, 20_000.0, 1_000.0)
        .with_plain_map(PlainMap::Linear(LinearRange::new(20.0, 20_000.0)))
        .with_measures(Some(ParamUnit::Hertz))
}

/// What a plugin declares: `cutoff` in hertz over `20..=20000`, `level` in
/// decibels over `-60..=12`, `mode` with no unit, a read-only `locked`,
/// `transpose` in whole semitones over `-12..=12`, and `opaque`, which declares
/// seconds but no map (the plugin defines it, as a VST3 parameter does).
fn declared() -> Vec<ParameterInfo> {
    vec![
        cutoff_info(),
        ParameterInfo::new(LEVEL, "Level")
            .with_plain_range(-60.0, 12.0, 0.0)
            .with_plain_map(PlainMap::Linear(LinearRange::new(-60.0, 12.0)))
            .with_measures(Some(ParamUnit::Decibels)),
        ParameterInfo::new(MODE, "Mode").with_normalized_default(0.0),
        ParameterInfo::new(TRANSPOSE, "Transpose")
            .with_plain_range(-12.0, 12.0, 0.0)
            .with_plain_map(PlainMap::Stepped(LinearRange::new(-12.0, 12.0)))
            .with_measures(Some(ParamUnit::Semitones)),
        ParameterInfo::new(OPAQUE, "Opaque")
            .with_plain_range(0.0, 10.0, 5.0)
            .with_measures(Some(ParamUnit::Seconds)),
        ParameterInfo::new(LOCKED, "Locked")
            .with_normalized_default(0.25)
            .with_flags(ParamFlags::READ_ONLY, ParamFlags::READ_ONLY),
    ]
}

/// The plugin: what it declares now, the value of each, and every value the
/// host set, in order. Plain values map linearly over the declared range.
#[derive(Default)]
struct Fake {
    infos: Mutex<Vec<ParameterInfo>>,
    values: Mutex<HashMap<ParamId, Normalized>>,
    sets: Mutex<Vec<(ParamId, Normalized)>>,
    /// Writes the host made while `held`, which the plugin has not taken yet.
    held: std::sync::atomic::AtomicBool,
    pending: Mutex<Vec<(ParamId, Normalized)>>,
}

impl Fake {
    fn new() -> Arc<Self> {
        let fake = Arc::new(Self::default());
        *fake.infos.lock_recover() = declared();
        for (id, value) in [
            (CUTOFF, 0.5),
            (LEVEL, 0.25),
            (MODE, 0.0),
            (LOCKED, 0.25),
            (TRANSPOSE, 0.5),
            (OPAQUE, 0.5),
        ] {
            fake.values.lock_recover().insert(id, Normalized::of(value));
        }
        fake
    }

    fn range(&self, id: ParamId) -> Option<(f64, f64)> {
        self.infos
            .lock_recover()
            .iter()
            .find(|i| i.id() == id)
            .and_then(|i| i.bounds())
    }

    /// Makes the plugin take the writes held back, in order.
    fn take_held(&self) {
        self.held.store(false, std::sync::atomic::Ordering::SeqCst);
        for (id, value) in std::mem::take(&mut *self.pending.lock_recover()) {
            self.take(id, value);
        }
    }

    fn take(&self, id: ParamId, value: Normalized) {
        self.values.lock_recover().insert(id, value);
        self.sets.lock_recover().push((id, value));
    }

    fn sets(&self) -> Vec<(ParamId, f64)> {
        self.sets
            .lock_recover()
            .iter()
            .map(|&(id, v)| (id, v.get()))
            .collect()
    }
}

impl AnyThread for Fake {
    fn parameter_descriptors(&self) -> Option<Vec<ParameterInfo>> {
        Some(self.infos.lock_recover().clone())
    }
    fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        self.values.lock_recover().get(&id).copied()
    }
    fn set_parameter_value(
        &self,
        id: ParamId,
        value: Normalized,
    ) -> Result<(), tutti_plugin_types::UnknownParam> {
        if !self.infos.lock_recover().iter().any(|i| i.id() == id) {
            return Err(tutti_plugin_types::UnknownParam(id));
        }
        if self.held.load(std::sync::atomic::Ordering::SeqCst) {
            self.pending.lock_recover().push((id, value));
        } else {
            self.take(id, value);
        }
        Ok(())
    }
    fn parameter_plain(&self, id: ParamId, value: Normalized) -> Option<f64> {
        let (min, max) = self.range(id)?;
        Some(min + (max - min) * value.get())
    }
    fn parameter_normalized(&self, id: ParamId, plain: f64) -> Option<Normalized> {
        let (min, max) = self.range(id)?;
        Some(Normalized::clamped((plain - min) / (max - min)))
    }
    fn parameter_text(&self, _id: ParamId, value: Normalized) -> Option<String> {
        Some(format!("{:.2} units", value.get()))
    }
    fn parameter_value_from_text(&self, _id: ParamId, text: &str) -> Option<Normalized> {
        text.strip_suffix(" units")?
            .parse()
            .ok()
            .and_then(Normalized::new)
    }
    fn save_state(
        &self,
        _: tutti_plugin_types::StateUse,
    ) -> Result<Vec<u8>, tutti_plugin::error::StateError> {
        Err(tutti_plugin::error::StateError::NoStateRoute)
    }
    fn set_render_mode(&self, _mode: RenderMode) -> bool {
        false
    }
    fn set_automation_mode(&self, _mode: AutomationMode) -> Result<(), AutomationModeError> {
        Err(AutomationModeError::Unsupported {
            format: "fake".into(),
        })
    }
}

impl Health for Fake {
    fn is_crashed(&self) -> bool {
        false
    }
    fn crash_cause(&self) -> Option<PluginFailure> {
        None
    }
    fn rate_refusal(&self) -> Option<(SampleRate, String)> {
        None
    }
    fn block_counts(&self) -> tutti_plugin_types::BlockCounts {
        tutti_plugin_types::BlockCounts::default()
    }
    fn terminate(&self, _reason: tutti_plugin_types::TerminationReason) {}
}

/// A detached session with the fake hosted at a node, its parameters not yet
/// read, and the sink the fake announces a rescan through.
fn session() -> (DetachedSession, NodeKey, Arc<Fake>, RefreshSink) {
    let (s, key, fake, refresh) = session_keyed();
    (s, key.erase(), fake, refresh)
}

/// [`session`], with the plugin's key.
fn session_keyed() -> (DetachedSession, Key<AnyPlugin>, Arc<Fake>, RefreshSink) {
    let fake = Fake::new();
    let refresh = RefreshSink::default();
    let handle = PluginHandle::from_backend(
        Arc::clone(&fake),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    )
    .with_refresh_sink(refresh.clone());
    let mut s = Session::builder()
        .executor(tutti_platform::Executor::new())
        .outputs_floor(ChannelLayout::STEREO)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    let (node, _) = s.graph().add(ForkByClone(Const::mono(0.0)));
    let key = host(
        &mut s,
        node,
        handle,
        tutti_plugin::catalog::PluginRef::file("fake.clap"),
    );
    s.commit();
    (s, key, fake, refresh)
}

/// The plugin events among `events`.
fn plugin_events(events: Vec<SessionEvent>) -> Vec<PluginEvent> {
    events
        .into_iter()
        .filter_map(|event| match event {
            SessionEvent::Plugin(event) => Some(event),
            _ => None,
        })
        .collect()
}

/// An update that submits a parameter read (the next update polls it) and
/// one that lands it; the plugin events both reported.
fn update(s: &mut DetachedSession) -> Vec<PluginEvent> {
    let mut reported = plugin_events(s.update());
    reported.extend(plugin_events(s.update()));
    reported
}

/// A session whose parameters are read.
fn ready() -> (DetachedSession, NodeKey, Arc<Fake>, RefreshSink) {
    let (mut s, node, fake, refresh) = session();
    let _ = update(&mut s);
    (s, node, fake, refresh)
}

fn cutoff(s: &DetachedSession, node: NodeKey) -> crate::Param<'_> {
    s.plugin_view(node).unwrap().params().get(CUTOFF).unwrap()
}

fn changes(events: Vec<PluginEvent>) -> Vec<PluginChange> {
    events.into_iter().map(|e| e.change).collect()
}

/// **A gesture guard that is dropped in the middle of a drag ends the
/// gesture.** The hand lets go after two moves of four planned (the loop is cut
/// short by an early return): the session reports the begin, the two changes
/// and the end, in that order, and the plugin took the two values.
#[test]
fn a_gesture_guard_dropped_mid_drag_ends_the_gesture() {
    let (mut s, node, fake, _) = ready();
    let drag = |s: &DetachedSession| -> Result<(), &'static str> {
        let gesture = cutoff(s, node).edit().unwrap();
        for (i, v) in [0.1, 0.2, 0.3, 0.4].into_iter().enumerate() {
            if i == 2 {
                return Err("the hand was lifted");
            }
            gesture.set(Normalized::of(v)).unwrap();
        }
        Ok(())
    };
    assert_eq!(drag(&s), Err("the hand was lifted"));

    let heard = changes(update(&mut s));
    let [
        PluginChange::GestureBegan { id: began },
        PluginChange::ParamChanged {
            id: first_id,
            value: first,
            recorded: first_recorded,
        },
        PluginChange::ParamChanged {
            value: second,
            recorded: second_recorded,
            ..
        },
        PluginChange::GestureEnded { id: ended },
    ] = heard.as_slice()
    else {
        panic!("begin, the two changes and the end, in order: {heard:?}");
    };
    assert_eq!((*began, *first_id, *ended), (CUTOFF, CUTOFF, CUTOFF));
    assert_eq!((first.get(), second.get()), (0.1, 0.2));
    assert!(
        *first_recorded && *second_recorded,
        "a host's edit is recorded"
    );
    assert_eq!(
        fake.sets(),
        [(CUTOFF, 0.1), (CUTOFF, 0.2)],
        "the plugin took the two values"
    );
    assert_eq!(cutoff(&s, node).normalized(), Normalized::of(0.2));
}

/// **A gesture on a read-only parameter is refused before it begins**, so the
/// guard's own `set` has nothing to refuse and the stream shows no gesture.
#[test]
fn a_gesture_on_a_read_only_parameter_never_begins() {
    let (mut s, node, fake, _) = ready();
    let locked = s.plugin_view(node).unwrap().params().get(LOCKED).unwrap();
    assert!(matches!(
        locked.edit(),
        Err(PluginParamRefusal::ReadOnly(id)) if id == LOCKED
    ));
    assert!(changes(update(&mut s)).is_empty());
    assert!(fake.sets().is_empty());
}

/// **`try_unit::<Db>` on a parameter that measures hertz is `UnitMismatch`**,
/// once, when the target is made; `try_unit::<Hz>` on it is accepted.
#[test]
fn a_unit_target_of_the_wrong_unit_is_a_mismatch() {
    let (s, node, _, _) = ready();
    let params = s.plugin_view(node).unwrap().params();
    let try_unit = |id| params.get(id).unwrap();
    assert!(matches!(
        try_unit(CUTOFF).try_unit::<Db>(),
        Err(UnitError::UnitMismatch { id, declared: ParamUnit::Hertz, .. }) if id == CUTOFF
    ));
    assert!(matches!(
        try_unit(LEVEL).try_unit::<Hz>(),
        Err(UnitError::UnitMismatch {
            declared: ParamUnit::Decibels,
            ..
        })
    ));
    assert!(try_unit(CUTOFF).try_unit::<Hz>().is_ok());
    assert!(try_unit(LEVEL).try_unit::<Db>().is_ok());
}

/// **`try_unit` on a parameter with no map is `NoUnit`**: one that declares no
/// unit, and one that declares a unit but whose map the plugin defines.
#[test]
fn a_unit_target_without_a_unit_or_a_map_is_no_unit() {
    let (s, node, _, _) = ready();
    let params = s.plugin_view(node).unwrap().params();
    assert_eq!(
        params.get(MODE).unwrap().try_unit::<Hz>(),
        Err(UnitError::NoUnit(MODE))
    );
    assert_eq!(
        params.get(OPAQUE).unwrap().try_unit::<Seconds>(),
        Err(UnitError::NoUnit(OPAQUE))
    );
}

/// The refusal of a value call as a `SetRefusal`, or a panic naming what it
/// did instead.
fn set_refusal(result: Result<(), ValueError<&'static str>>) -> SetRefusal {
    match result {
        Err(ValueError {
            kind: ValueErrorKind::Refused(refusal),
            ..
        }) => refusal,
        other => panic!("a refusal was expected: {other:?}"),
    }
}

/// **A unit write lands as the line says**: the fake's line over `20..=20000`
/// is the oracle (`Hz::new(10010)` is the middle, position `0.5`), `get` reads
/// the value back in hertz, and the position the plugin was given is the one the
/// line names.
#[test]
fn a_linear_unit_write_lands_the_position_the_line_names() {
    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    let cutoff = cutoff(&s, key.erase()).try_unit::<Hz>().unwrap();
    // The fake started it at 0.5: 20 + 19980 / 2.
    assert_eq!(s.get(cutoff).unwrap(), Hz::new(10_010.0));

    s.set(cutoff.to(Hz::new(20.0 + 19_980.0 * 0.25))).unwrap();
    assert_eq!(fake.sets(), [(CUTOFF, 0.25)]);
    assert_eq!(s.get(cutoff).unwrap(), Hz::new(5_015.0), "read-your-writes");
    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.25));
}

/// **A stepped unit write lands the position its step names**: a whole step
/// lands at `(step + 12) / 24`, and a value between two steps reads back as the
/// nearer one, the way the stepped map rounds.
#[test]
fn a_stepped_unit_write_reads_back_the_step_the_map_rounds_to() {
    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    let transpose = s
        .plugin_view(key)
        .unwrap()
        .params()
        .get(TRANSPOSE)
        .unwrap()
        .try_unit::<Semitones>()
        .unwrap();
    s.set(transpose.to(Semitones::new(6.0))).unwrap();
    assert_eq!(fake.sets(), [(TRANSPOSE, 0.75)]);
    assert_eq!(s.get(transpose).unwrap(), Semitones::new(6.0));

    s.set(transpose.to(Semitones::new(-2.6))).unwrap();
    assert_eq!(
        fake.sets()[1],
        (TRANSPOSE, 0.375),
        "the plugin is sent the step's position"
    );
    assert_eq!(s.get(transpose).unwrap(), Semitones::new(-3.0));
}

/// **A unit write outside the declared range is `OutOfRange`, and a NaN or an
/// infinity is `NotFinite`**; nothing reaches the plugin.
#[test]
fn a_unit_write_outside_the_range_or_not_finite_is_refused() {
    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    let cutoff = cutoff(&s, key.erase()).try_unit::<Hz>().unwrap();
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            set_refusal(s.set(cutoff.to(Hz::new(bad)))),
            SetRefusal::NotFinite,
            "{bad}"
        );
    }
    for out in [30_000.0, 10.0] {
        assert_eq!(
            set_refusal(s.set(cutoff.to(Hz::new(out)))),
            SetRefusal::OutOfRange,
            "{out}"
        );
    }
    assert!(fake.sets().is_empty());
}

/// **The unit is read at every call, never kept by the target**: a rescan that
/// leaves `cutoff` without its unit refuses both the write and the read of a
/// target made before it with `NoUnit`, and a rescan that moves its unit to
/// decibels refuses them as a mismatch.
#[test]
fn a_rescan_that_removes_the_unit_refuses_a_unit_target_made_before_it() {
    let (mut s, key, fake, refresh) = session_keyed();
    let _ = update(&mut s);
    let cutoff = cutoff(&s, key.erase()).try_unit::<Hz>().unwrap();
    s.set(cutoff.to(Hz::new(100.0))).unwrap();
    let written = s.get(key.param(CUTOFF)).unwrap();

    let rescan = |s: &mut DetachedSession, info: ParameterInfo| {
        *fake.infos.lock_recover() = vec![info];
        refresh.fire(PluginRefresh::ParamTitles);
        let _ = update(s);
    };
    rescan(
        &mut s,
        ParameterInfo::new(CUTOFF, "Cutoff").with_normalized_default(0.5),
    );
    let unit = |refusal| SetRefusal::Plugin(PluginParamRefusal::Unit(refusal));
    assert_eq!(
        set_refusal(s.set(cutoff.to(Hz::new(200.0)))),
        unit(UnitError::NoUnit(CUTOFF))
    );
    assert_eq!(
        set_refusal(s.get(cutoff).map(drop)),
        unit(UnitError::NoUnit(CUTOFF))
    );
    assert_eq!(fake.sets().len(), 1, "the refused write reached nothing");
    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), written);

    rescan(
        &mut s,
        cutoff_info()
            .with_plain_range(-60.0, 12.0, 0.0)
            .with_measures(Some(ParamUnit::Decibels)),
    );
    assert!(matches!(
        set_refusal(s.set(cutoff.to(Hz::new(200.0)))),
        SetRefusal::Plugin(PluginParamRefusal::Unit(UnitError::UnitMismatch {
            declared: ParamUnit::Decibels,
            ..
        }))
    ));
}

/// **An id the plugin does not declare is refused**, by `get`, and before the
/// parameters are read nothing resolves.
#[test]
fn an_unknown_id_is_refused_and_an_unread_plugin_resolves_nothing() {
    let (mut s, node, _, _) = session();
    let params = s.plugin_view(node).unwrap().params();
    assert!(!params.is_ready());
    assert!(matches!(
        params.get(CUTOFF),
        Err(PluginParamRefusal::NotRead)
    ));

    let _ = update(&mut s);
    let params = s.plugin_view(node).unwrap().params();
    assert!(params.is_ready());
    assert_eq!(params.len(), 6);
    let unknown = ParamId::new(99);
    assert!(matches!(
        params.get(unknown),
        Err(PluginParamRefusal::UnknownParam(id)) if id == unknown
    ));
}

/// **A rescan replaces what `params()` shows, and an id the plugin stopped
/// declaring is refused when it is resolved again.** The plugin announces a
/// rescan of its titles: until the read lands the old declarations stand, after
/// it the new ones do, together with their values. (A handle held across the
/// landing is a borrow error: see the module docs of `Params`.)
#[test]
fn a_rescan_replaces_the_declarations_and_the_values_with_them() {
    let (mut s, node, fake, refresh) = ready();
    assert!(s.plugin_view(node).unwrap().params().get(MODE).is_ok());

    // The plugin drops `mode` and gains `shape`, and moves `cutoff`.
    let shape = ParamId::new(7);
    *fake.infos.lock_recover() = vec![
        cutoff_info(),
        ParameterInfo::new(shape, "Shape").with_normalized_default(0.5),
    ];
    fake.values
        .lock_recover()
        .insert(CUTOFF, Normalized::of(0.75));
    fake.values
        .lock_recover()
        .insert(shape, Normalized::of(0.5));
    refresh.fire(PluginRefresh::ParamTitles);

    // The notice is heard by this update, which submits the read; the old
    // declarations stand until the read lands, in the next update.
    let mut events = plugin_events(s.update());
    assert!(
        s.plugin_view(node).unwrap().params().get(MODE).is_ok(),
        "not yet"
    );
    events.extend(plugin_events(s.update()));
    assert!(
        changes(events)
            .iter()
            .any(|c| matches!(c, PluginChange::ParamTitlesChanged))
    );

    let params = s.plugin_view(node).unwrap().params();
    assert!(matches!(params.get(MODE), Err(PluginParamRefusal::UnknownParam(id)) if id == MODE));
    assert_eq!(params.get(shape).unwrap().normalized(), Normalized::of(0.5));
    assert_eq!(
        params.get(CUTOFF).unwrap().normalized(),
        Normalized::of(0.75),
        "the values are the rescan's"
    );
    assert_eq!(params.len(), 2);
}

/// **The text and the plain value come from the plugin**: the fake's line is
/// the oracle for the plain value, and the text follows a write.
#[test]
fn text_and_plain_values_go_through_the_plugin() {
    let (mut s, key, _, _) = session_keyed();
    let _ = update(&mut s);
    fn level(s: &DetachedSession, key: Key<AnyPlugin>) -> crate::Param<'_> {
        s.plugin_view(key).unwrap().params().get(LEVEL).unwrap()
    }
    assert_eq!(level(&s, key).plain(), Some(-60.0 + 72.0 * 0.25));

    s.set(key.param(LEVEL).to(Normalized::of(0.5))).unwrap();
    assert_eq!(level(&s, key).normalized(), Normalized::of(0.5));
    assert_eq!(level(&s, key).plain(), Some(-24.0));
    assert_eq!(level(&s, key).text().as_deref(), Some("0.50 units"));
}

/// **A plugin parameter is targeted by the same reference a native one is**
/// (`modulation`): `Param::target` is the `ParamRef` a route takes.
#[test]
fn a_plugin_parameter_targets_a_modulation_route_through_a_param_ref() {
    let (s, node, _, _) = ready();
    let level = s.plugin_view(node).unwrap().params().get(LEVEL).unwrap();
    assert_eq!(
        crate::ParamRef::from(level.target()),
        crate::ParamRef::plugin(node, LEVEL)
    );
}

/// **A plugin's key names a parameter as a route's target** (`modulation`):
/// `key.param(id)` is the reference `Param::target` gives, and a route onto it
/// is declared and read back through the key, as a native param's is.
#[test]
fn a_plugin_key_names_a_parameter_a_route_targets() {
    use crate::{ParamRef, RouteSpec};
    use tutti_units::Depth;

    let (mut s, key, _, _) = session_keyed();
    let _ = update(&mut s);
    let target = key.param(LEVEL);
    assert_eq!(ParamRef::from(target), ParamRef::plugin(key, LEVEL));
    assert_eq!(
        target,
        s.plugin_view(key)
            .unwrap()
            .params()
            .get(LEVEL)
            .unwrap()
            .target()
    );
    let id = {
        let mut m = s.mods_mut();
        m.range_raw(target, 0.0, 1.0).unwrap();
        let source = m.macro_(0.5);
        m.route(RouteSpec::new(source, target, Depth::of(0.25)))
            .unwrap()
    };
    let routed: Vec<_> = s.mods().routes_to(target).map(|r| r.id).collect();
    assert_eq!(routed, [id]);
}

/// **A hosted plugin's key wires like a native node's**: `connect` takes the
/// key of a hosted plugin (and its ports) and the root, and the edge is in the
/// graph.
#[test]
fn a_hosted_plugin_key_wires_like_a_native_key() {
    let (mut s, key, _, _) = session_keyed();
    let _ = update(&mut s);
    s.graph()
        .connect(key.out(0), crate::Root::output(0))
        .unwrap();
    let sources = s.graph().sources(crate::Root::output(0)).unwrap();
    assert_eq!(sources, [key.out(0)]);
}

/// **A gesture outlives frames**: begun, then `update` runs (and a rescan lands),
/// then the hand moves, `update` runs again, then lets go. The stream holds
/// exactly one begin, the changes and one end, and `set` after the plugin is
/// removed is refused and sends nothing.
#[test]
fn a_gesture_spans_updates_and_refuses_once_the_plugin_is_gone() {
    let (mut s, node, fake, refresh) = ready();
    let gesture = cutoff(&s, node).edit().unwrap();
    let mut heard = changes(update(&mut s));
    // A rescan lands in between; the parameter is still declared.
    refresh.fire(PluginRefresh::ParamTitles);
    heard.extend(changes(update(&mut s)));
    gesture.set(Normalized::of(0.3)).unwrap();
    heard.extend(changes(update(&mut s)));
    gesture.set(Normalized::of(0.6)).unwrap();
    heard.extend(changes(update(&mut s)));
    drop(gesture);
    heard.extend(changes(update(&mut s)));

    let began = heard
        .iter()
        .filter(|c| matches!(c, PluginChange::GestureBegan { .. }))
        .count();
    let ended = heard
        .iter()
        .filter(|c| matches!(c, PluginChange::GestureEnded { .. }))
        .count();
    let moved: Vec<f64> = heard
        .iter()
        .filter_map(|c| match c {
            PluginChange::ParamChanged {
                value,
                recorded: true,
                ..
            } => Some(value.get()),
            _ => None,
        })
        .collect();
    assert_eq!((began, ended), (1, 1), "{heard:?}");
    assert_eq!(moved, [0.3, 0.6]);
    assert_eq!(fake.sets(), [(CUTOFF, 0.3), (CUTOFF, 0.6)]);

    // Gone: removed.
    let gesture = cutoff(&s, node).edit().unwrap();
    let _ = update(&mut s);
    s.remove(node).unwrap();
    assert_eq!(
        gesture.set(Normalized::of(0.9)),
        Err(PluginParamRefusal::UnknownParam(CUTOFF))
    );
    drop(gesture);
    assert_eq!(fake.sets().len(), 2, "nothing was sent blind");
    assert!(
        !changes(update(&mut s))
            .iter()
            .any(|c| matches!(c, PluginChange::GestureEnded { .. })),
        "a gone plugin hears no end"
    );
}

/// **A rescan that drops the parameter ends the hold**: `set` is refused.
#[test]
fn a_gesture_on_a_parameter_the_rescan_dropped_is_refused() {
    let (mut s, node, fake, refresh) = ready();
    let gesture = cutoff(&s, node).edit().unwrap();
    *fake.infos.lock_recover() =
        vec![ParameterInfo::new(MODE, "Mode").with_normalized_default(0.0)];
    refresh.fire(PluginRefresh::ParamTitles);
    let _ = update(&mut s);
    assert_eq!(
        gesture.set(Normalized::of(0.5)),
        Err(PluginParamRefusal::UnknownParam(CUTOFF))
    );
}

/// Whether `result` failed as `NoSuchTarget`, naming the target.
fn no_such_target(result: Result<(), ValueError<&'static str>>) -> bool {
    matches!(
        result,
        Err(ValueError {
            target: Some(_),
            kind: ValueErrorKind::NoSuchTarget,
            ..
        })
    )
}

/// The refusal a value call carries, or a panic naming what it did instead.
fn refusal(result: Result<(), ValueError<&'static str>>) -> PluginParamRefusal {
    match result {
        Err(ValueError {
            kind: ValueErrorKind::Refused(SetRefusal::Plugin(refusal)),
            ..
        }) => refusal,
        other => panic!("a plugin refusal was expected: {other:?}"),
    }
}

/// **A set reads back from the mirror before the plugin has it.** The plugin
/// holds the write back; `get` and the parameter's view answer the new
/// position, the plugin has taken nothing, and it takes the value when it
/// takes its writes.
#[test]
fn a_set_reads_back_from_the_mirror_before_the_plugin_has_it() {
    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    fake.held.store(true, std::sync::atomic::Ordering::SeqCst);

    s.set(key.param(CUTOFF).to(Normalized::of(0.8))).unwrap();

    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.8));
    assert_eq!(cutoff(&s, key.erase()).normalized(), Normalized::of(0.8));
    assert!(fake.sets().is_empty(), "the plugin has not taken it");
    fake.take_held();
    assert_eq!(fake.sets(), [(CUTOFF, 0.8)]);
}

/// **An id the plugin does not declare is no target, for `set` or `get`**: both
/// fail at the call with `NoSuchTarget` naming the target, and nothing reaches
/// the plugin.
#[test]
fn an_undeclared_id_is_no_target_for_set_or_get() {
    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    let unknown = key.param(ParamId::new(99));
    assert!(no_such_target(s.set(unknown.to(Normalized::of(0.5)))));
    assert!(no_such_target(s.get(unknown).map(drop)));
    assert!(fake.sets().is_empty());
}

/// **A parameter is no target before the declaration is read**, for `set` or
/// `get`; once the read lands it is one.
#[test]
fn a_parameter_is_no_target_before_the_declaration_is_read() {
    let (mut s, key, fake, _) = session_keyed();
    assert!(no_such_target(
        s.set(key.param(CUTOFF).to(Normalized::of(0.5)))
    ));
    assert!(no_such_target(s.get(key.param(CUTOFF)).map(drop)));
    assert!(fake.sets().is_empty());
    let _ = update(&mut s);
    s.set(key.param(CUTOFF).to(Normalized::of(0.5))).unwrap();
    assert_eq!(fake.sets(), [(CUTOFF, 0.5)]);
}

/// **A write to a read-only parameter is refused**, and the mirror keeps its
/// value.
#[test]
fn a_read_only_parameter_is_refused() {
    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    assert_eq!(
        refusal(s.set(key.param(LOCKED).to(Normalized::of(1.0)))),
        PluginParamRefusal::ReadOnly(LOCKED)
    );
    assert_eq!(s.get(key.param(LOCKED)).unwrap(), Normalized::of(0.25));
    assert!(fake.sets().is_empty());
}

/// **A batch over two plugin params and one native param is atomic**: a
/// refused third item lands nothing, and the same batch without it lands all.
#[test]
fn a_batch_over_plugin_and_native_params_lands_nothing_when_one_is_refused() {
    use tutti_nodes::{SvfFilterConfig, SvfFilterNode, SvfType};
    use tutti_units::Q;

    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::from(
        SvfFilterConfig::new(ChannelLayout::MONO, SvfType::LowPass)
            .freq(Hz::new(800.0))
            .q(Q::new(0.707)),
    ));
    s.commit();

    let refused = s.set((
        key.param(CUTOFF).to(Normalized::of(0.1)),
        lp.cutoff().to(Hz::new(300.0)),
        key.param(LOCKED).to(Normalized::of(0.9)),
    ));
    let Err(error) = refused else {
        panic!("the read-only item refuses the batch");
    };
    assert_eq!(error.at, 2);
    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.5));
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(800.0));
    assert!(fake.sets().is_empty());

    s.set((
        key.param(CUTOFF).to(Normalized::of(0.1)),
        lp.cutoff().to(Hz::new(300.0)),
        key.param(LEVEL).to(Normalized::of(0.9)),
    ))
    .unwrap();
    assert_eq!(fake.sets(), [(CUTOFF, 0.1), (LEVEL, 0.9)]);
    assert_eq!(s.get(lp.cutoff()).unwrap(), Hz::new(300.0));
}

/// **A load that moves the plugin's values drops the writes made before it**:
/// the host wrote `0.9`, the plugin's restored state holds `0.1`, and once the
/// re-read lands `get` answers the plugin's value, not the earlier write.
#[test]
fn a_state_load_replaces_the_values_written_before_it() {
    let (mut s, key, fake, refresh) = session_keyed();
    let _ = update(&mut s);
    s.set(key.param(CUTOFF).to(Normalized::of(0.9))).unwrap();
    let _ = update(&mut s);
    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.9));

    fake.values
        .lock_recover()
        .insert(CUTOFF, Normalized::of(0.1));
    refresh.fire(PluginRefresh::ParamValues);
    let _ = update(&mut s);
    let _ = update(&mut s);
    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.1));
}

/// **A plugin whose format has no automation mode has no target for it**;
/// the fake declares no `AUTOMATION_STATE`.
#[test]
fn a_plugin_without_automation_state_has_no_automation_mode_target() {
    let (s, node, _, _) = ready();
    assert_eq!(s.plugin_view(node).unwrap().automation_mode(), None);
}

/// Replaces the plugin at `node` with `fake` as the node's plugin `plugin`, as
/// the graph's replace does: the record takes the new handle, the old one is
/// retired and the value table is told.
fn replace_with(s: &mut DetachedSession, node: NodeKey, fake: &Arc<Fake>, plugin: &str) {
    let handle = PluginHandle::from_backend(
        Arc::clone(fake),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    let (controls, _) =
        PluginControls::new(Samples::new(0), PluginTail::default(), SampleRate::SR_48K);
    let plugin = tutti_plugin::catalog::PluginRef::file(plugin);
    let hosted = s.graph.plugins.get_mut(&node).expect("a hosted plugin");
    let outgoing = hosted.replace_hosted(handle, (*controls).clone(), plugin);
    outgoing.retire().settle(node, &mut s.graph.graveyard);
    s.graph.provide_plugin(node);
}

/// **A replace by the same plugin keeps its parameters targets until the new
/// read lands**: a set made in between is taken by the new handle and `get`
/// reads it back, and the read that lands then replaces the carried values.
#[test]
fn a_replace_by_the_same_plugin_keeps_its_parameters_settable_until_the_read_lands() {
    let (mut s, key, _, _) = session_keyed();
    let _ = update(&mut s);
    let again = Fake::new();
    replace_with(&mut s, key.erase(), &again, "fake.clap");

    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.5));
    s.set(key.param(CUTOFF).to(Normalized::of(0.3))).unwrap();
    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.3));
    assert_eq!(again.sets(), [(CUTOFF, 0.3)], "the new handle took it");
    assert_eq!(
        refusal(s.set(key.param(LOCKED).to(Normalized::of(0.9)))),
        PluginParamRefusal::ReadOnly(LOCKED),
        "the carried declaration keeps its access"
    );

    again
        .values
        .lock_recover()
        .insert(LEVEL, Normalized::of(0.75));
    let _ = update(&mut s);
    assert_eq!(s.get(key.param(LEVEL)).unwrap(), Normalized::of(0.75));
    assert_eq!(s.get(key.param(CUTOFF)).unwrap(), Normalized::of(0.3));
}

/// **A replace by another plugin leaves no target until its read lands.**
#[test]
fn a_replace_by_another_plugin_has_no_target_until_its_read_lands() {
    let (mut s, key, _, _) = session_keyed();
    let _ = update(&mut s);
    let other = Fake::new();
    replace_with(&mut s, key.erase(), &other, "other.clap");

    assert!(no_such_target(
        s.set(key.param(CUTOFF).to(Normalized::of(0.3)))
    ));
    assert!(other.sets().is_empty());
    let _ = update(&mut s);
    s.set(key.param(CUTOFF).to(Normalized::of(0.3))).unwrap();
}

/// **A write to a parameter the plugin dropped is counted as refused, and the
/// re-read makes the id no target.** The plugin drops `cutoff` and the session
/// has not read the new list yet, so the write passes the session's check; the
/// plugin refuses it, the write is counted in the plugin's losses, and after the
/// read lands `set` fails with `NoSuchTarget`.
#[test]
fn a_write_to_a_parameter_a_rescan_dropped_is_counted_and_then_no_target() {
    let (mut s, key, fake, refresh) = session_keyed();
    let _ = update(&mut s);
    let refused = |s: &DetachedSession| s.plugin_view(key).unwrap().losses().refused;
    assert_eq!(refused(&s), 0);

    *fake.infos.lock_recover() =
        vec![ParameterInfo::new(MODE, "Mode").with_normalized_default(0.0)];
    s.set(key.param(CUTOFF).to(Normalized::of(0.8))).unwrap();
    assert_eq!(refused(&s), 1, "the plugin's refusal is counted");
    assert!(fake.sets().is_empty(), "the plugin took nothing");

    refresh.fire(PluginRefresh::ParamTitles);
    let _ = update(&mut s);
    assert!(no_such_target(
        s.set(key.param(CUTOFF).to(Normalized::of(0.8)))
    ));
    assert_eq!(
        refused(&s),
        1,
        "a write that is refused at the call is not counted"
    );
}

/// **One plugin parameter named in two value types in one batch is refused**:
/// a position write and a hertz write to `cutoff`, in either order, fail at the
/// second item with `MixedValueTypes` and land nothing; two writes in one type
/// stay last-wins, landing the analytic position of the last.
#[test]
fn a_batch_naming_one_parameter_in_two_units_is_refused() {
    let (mut s, key, fake, _) = session_keyed();
    let _ = update(&mut s);
    let hz = cutoff(&s, key.erase()).try_unit::<Hz>().unwrap();
    let position = key.param(CUTOFF);

    let Err(error) = s.set((position.to(Normalized::of(0.1)), hz.to(Hz::new(5_015.0)))) else {
        panic!("a mixed pair is refused");
    };
    assert_eq!(
        (error.at, error.kind),
        (1, ValueErrorKind::Refused(SetRefusal::MixedValueTypes))
    );
    let Err(error) = s.set((hz.to(Hz::new(5_015.0)), position.to(Normalized::of(0.1)))) else {
        panic!("a mixed pair is refused in the other order");
    };
    assert_eq!(error.at, 1);
    assert_eq!(s.get(position).unwrap(), Normalized::of(0.5));
    assert!(fake.sets().is_empty(), "a refused batch lands nothing");

    s.set((
        hz.to(Hz::new(100.0)),
        hz.to(Hz::new(20.0 + 19_980.0 * 0.75)),
    ))
    .unwrap();
    assert_eq!(fake.sets(), [(CUTOFF, 0.75)]);
}

/// **A rescan to another range with the unit kept moves a target made before it
/// to the new map once the re-read lands**: `150 Hz` on `100..=200` is position
/// `0.5`.
#[test]
fn a_rescan_to_another_range_converts_a_target_made_before_it_with_the_new_map() {
    let (mut s, key, fake, refresh) = session_keyed();
    let _ = update(&mut s);
    let cutoff = cutoff(&s, key.erase()).try_unit::<Hz>().unwrap();

    *fake.infos.lock_recover() = vec![
        cutoff_info()
            .with_plain_range(100.0, 200.0, 100.0)
            .with_plain_map(PlainMap::Linear(LinearRange::new(100.0, 200.0))),
    ];
    refresh.fire(PluginRefresh::ParamTitles);
    let _ = update(&mut s);

    s.set(cutoff.to(Hz::new(150.0))).unwrap();
    assert_eq!(fake.sets(), [(CUTOFF, 0.5)]);
    assert_eq!(s.get(cutoff).unwrap(), Hz::new(150.0));
}
