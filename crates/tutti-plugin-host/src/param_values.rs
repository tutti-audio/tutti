//! A loaded plugin's parameters, mirrored on the control thread: what it
//! declares ([`ParameterInfo`]) and the value of each.
//!
//! The mirror is filled once, by a read the plugin's replies feed (each value is
//! a question to the plugin, answered later), and kept current from
//! the values the plugin reports afterwards (an edit in its GUI, a preset
//! load), through [`PluginHandle::subscribe`], and from the values the host
//! sets ([`report`](PluginParamValues::report)). A value reads as the latest of
//! those at once, before the control thread drains them.
//!
//! **The read is a value, and nothing here spawns it.** [`PluginParamValues::start`]
//! installs the report callback and hands back the mirror plus a `Send`
//! [`ParamRead`]; awaiting it ([`ReadingParams`]) gives the snapshot the host
//! hands to [`PluginParamValues::land`]. The host spawns the future on its
//! executor: no thread waits for the plugin, because each answer wakes the task
//! (see [`ReadingParams`] for what fills them). A plugin's rescan is the same
//! read again: landing it replaces the declarations and the values together,
//! so a value never outlives the declaration it was read against.
//!
//! **Every format reports**: each plugin's own edits reach
//! the parameter callback as [`ParamEdit`](tutti_plugin_types::ParamEdit)s, in
//! process through `Control::take_reported_edits` and through the plugin-server
//! as `News::ParameterChanged`.
//!
//! **An echo is not an edit.** A plugin that reports the value a host just
//! set (from inside `setParameter`) would make that value an edit. A format
//! whose parameter word is `f32` (VST2, AU) drops a report equal to the `f32`
//! narrowing of the `f64` the host last wrote
//! ([`WrittenValues`](tutti_plugin_types::WrittenValues)), so none reaches here
//! and the `f64` the host set stays the value read; a format that reports
//! values in future needs the same filter.

use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tutti_rt::Mutex;

use crate::{PluginHandle, PluginSignal, Subscription};
use tutti_plugin_types::{Answer, ParameterInfo, PluginParamRange};
use tutti_units::Normalized;
use tutti_units::ParamId;

/// Edits the plugin reported or the host set, in order, waiting for
/// [`PluginParamValues::drain_edits`].
type Edits = Arc<Mutex<Vec<(ParamId, Normalized)>>>;

/// A cloneable way to report values into a mirror from anywhere, as the host
/// sets them ([`PluginParamValues::reporter`]): it outlives a borrow of the
/// mirror and keeps working while the mirror lives (a replaced mirror's queue
/// is never drained).
#[derive(Debug, Clone)]
pub struct ParamReporter(Edits);

impl ParamReporter {
    /// Queues a value as if the plugin had reported it.
    pub fn report(&self, id: ParamId, value: Normalized) {
        self.0.lock().push((id, value));
    }
}

/// What a plugin declares and the value of each parameter, as one read found
/// them: the descriptors, and a value aligned with each.
///
/// Every declared parameter has a value: one the plugin did not answer reads
/// as its declared default.
#[derive(Debug)]
pub struct ParamSnapshot {
    descriptors: Vec<ParameterInfo>,
    values: Vec<Normalized>,
}

/// The read of a plugin's parameters: every descriptor and its current value,
/// asked of the plugin through its handle. Awaiting it ([`IntoFuture`]) is
/// [`ReadingParams`].
///
/// `Send`. It holds the handle and nothing of the read itself, so it is also
/// what an unanswered read gives back to be awaited again.
#[must_use = "a mirror whose read is dropped never becomes ready; a read that \
              was not answered is given back to be awaited again"]
#[derive(Debug)]
pub struct ParamRead {
    handle: PluginHandle,
}

impl ParamRead {
    /// The read of `handle`'s parameters, to await again after the plugin
    /// announced a rescan.
    pub fn of(handle: &PluginHandle) -> Self {
        Self {
            handle: handle.clone(),
        }
    }
}

impl IntoFuture for ParamRead {
    type Output = Result<ParamSnapshot, Box<ParamRead>>;
    type IntoFuture = ReadingParams;

    fn into_future(self) -> ReadingParams {
        ReadingParams {
            handle: self.handle,
            stage: Stage::Unstarted,
        }
    }
}

/// A [`ParamRead`] being awaited: every parameter the plugin lists, with its
/// value. A parameter whose value the plugin does not answer reads as its
/// declared default; a parameter listed twice keeps its first entry; a plugin
/// that lists none gives `Ok` of an empty snapshot.
///
/// **What feeds it.** The plugin's hosting answers each question into a oneshot
/// ([`AnyThread::parameter_descriptors`](tutti_plugin_types::AnyThread::parameter_descriptors) and its siblings): a served plugin's
/// bridge control lane thread fills it with the server's reply, or with `None`
/// at the query deadline; an in-process plugin's main thread fills it when it
/// runs the work the question queued (at once, when the future is polled on the
/// main thread). **A poll never blocks** and needs no `update` of the
/// session: the answer wakes the task. A plugin is asked one parameter after
/// another (the lane serves its queries in order), the next when the last was
/// answered, so at most one query of a read is in flight.
///
/// Dropping it mid-read cancels it: the question in flight is answered into a
/// dropped receiver and discarded, and no later read sees it.
///
/// # Errors
///
/// The output is the [`ParamRead`] itself, given back to be awaited again, when
/// the plugin's list is unavailable: the plugin is lost, the backend cannot list,
/// the query's deadline passed, or the main thread an in-process plugin waits on
/// ended before it answered (the sender dropped unanswered). That is not "lists
/// none": landing an empty snapshot would make the mirror ready with nothing in
/// it, for good. A *value* the plugin does not answer in the same ways reads as
/// the declared default instead.
///
/// # Panics
///
/// When polled after it resolved.
#[must_use = "a read does nothing unless it is awaited"]
pub struct ReadingParams {
    handle: PluginHandle,
    stage: Stage,
}

impl std::fmt::Debug for ReadingParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadingParams").finish_non_exhaustive()
    }
}

enum Stage {
    Unstarted,
    Listing(Answer<Option<Vec<ParameterInfo>>>),
    Reading(Box<Reading>),
    Finished,
}

/// The parameters listed, read one after another.
struct Reading {
    waiting: std::vec::IntoIter<ParameterInfo>,
    seen: std::collections::HashSet<ParamId>,
    descriptors: Vec<ParameterInfo>,
    values: Vec<Normalized>,
    /// The parameter being read and its question in flight.
    asked: Option<(ParameterInfo, Question)>,
}

enum Question {
    /// The plugin's current value.
    Value(Answer<Option<Normalized>>),
    /// The declared default of a plain-range parameter, converted by the plugin.
    Default(Answer<Option<Normalized>>),
}

impl Future for ReadingParams {
    type Output = Result<ParamSnapshot, Box<ParamRead>>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        let params = this.handle.any_thread();
        if matches!(this.stage, Stage::Unstarted) {
            this.stage = Stage::Listing(params.parameter_descriptors());
        }
        if let Stage::Listing(listing) = &mut this.stage {
            // A canceled answer is as unavailable as a `None` one.
            let listed = std::task::ready!(Pin::new(listing).poll(cx)).flatten();
            let Some(listed) = listed else {
                this.stage = Stage::Finished;
                return Poll::Ready(Err(Box::new(ParamRead::of(&this.handle))));
            };
            this.stage = Stage::Reading(Box::new(Reading {
                descriptors: Vec::with_capacity(listed.len()),
                values: Vec::with_capacity(listed.len()),
                waiting: listed.into_iter(),
                seen: std::collections::HashSet::new(),
                asked: None,
            }));
        }
        let Stage::Reading(reading) = &mut this.stage else {
            panic!("a parameter read is polled after it resolved");
        };
        loop {
            let Some((info, question)) = &mut reading.asked else {
                let Some(info) = reading.waiting.next() else {
                    let snapshot = ParamSnapshot {
                        descriptors: std::mem::take(&mut reading.descriptors),
                        values: std::mem::take(&mut reading.values),
                    };
                    this.stage = Stage::Finished;
                    return Poll::Ready(Ok(snapshot));
                };
                if reading.seen.insert(info.id()) {
                    let question = Question::Value(params.parameter_value(info.id()));
                    reading.asked = Some((info, question));
                }
                continue;
            };
            let (Question::Value(answer) | Question::Default(answer)) = question;
            let answered = std::task::ready!(Pin::new(answer).poll(cx)).flatten();
            let value = match (&*question, answered) {
                (_, Some(value)) => value,
                (Question::Value(_), None) => match info.range() {
                    PluginParamRange::Normalized { default } => Normalized::clamped(default),
                    PluginParamRange::Plain { default, .. } => {
                        let id = info.id();
                        *question = Question::Default(params.parameter_normalized(id, default));
                        continue;
                    }
                },
                (Question::Default(_), None) => Normalized::default(),
            };
            let (info, _) = reading.asked.take().expect("a question was in flight");
            reading.descriptors.push(info);
            reading.values.push(value);
        }
    }
}

/// What a landed read holds: the declarations and their values, with the
/// position of each id.
#[derive(Debug)]
struct Declared {
    descriptors: Vec<ParameterInfo>,
    values: Vec<Normalized>,
    index: HashMap<ParamId, usize>,
}

/// A loaded plugin's parameters, by id: read once by a [`ParamRead`] (after its
/// saved state is restored), then kept current from the values the plugin
/// reports (`PluginHandle::subscribe`) and the host sets.
///
/// Ids are the `ParamId` a `ParamAddr::Id` names: a CLAP, VST3 or AU parameter
/// id, or a VST2 index.
#[derive(Debug)]
pub struct PluginParamValues {
    declared: Option<Declared>,
    edits: Edits,
    /// The handle's subscription that fills `edits`; the mirror stops
    /// following the plugin when it drops.
    _subscription: Subscription,
}

impl PluginParamValues {
    /// Installs `handle`'s parameter callback, which queues every value the
    /// plugin reports, and returns the mirror (not yet
    /// [ready](Self::is_ready)) with the [`ParamRead`] that fills it. Await the
    /// read on the host's executor and pass its result to
    /// [`land`](Self::land).
    ///
    /// The mirror holds its subscription to the handle, so it follows the
    /// plugin until it drops.
    #[must_use = "await the `ParamRead` and `land` its values, or the mirror never becomes ready"]
    pub fn start(handle: &PluginHandle) -> (Self, ParamRead) {
        let edits: Edits = Arc::default();
        let queue = Arc::clone(&edits);
        // The mirror follows the changes; a gesture's ends carry no value.
        let subscription = handle.subscribe(move |notice| {
            if let PluginSignal::Param(edit) = notice
                && let Some(value) = edit.value()
            {
                queue.lock().push((edit.id(), value));
            }
        });
        let mirror = Self {
            declared: None,
            edits,
            _subscription: subscription,
        };
        let read = ParamRead {
            handle: handle.clone(),
        };
        (mirror, read)
    }

    /// Lands a finished [`ParamRead`]: the mirror holds its declarations and
    /// values, replacing any it held, and is [ready](Self::is_ready). Edits
    /// reported meanwhile stay queued for [`drain_edits`](Self::drain_edits),
    /// which applies them over these.
    pub fn land(&mut self, read: ParamSnapshot) {
        let ParamSnapshot {
            descriptors,
            values,
        } = read;
        let index = descriptors
            .iter()
            .enumerate()
            .map(|(at, info)| (info.id(), at))
            .collect();
        self.declared = Some(Declared {
            descriptors,
            values,
            index,
        });
    }

    /// Queues a value as if the plugin had reported it: the host set it, and
    /// reads of it answer it at once.
    pub fn report(&self, id: ParamId, value: Normalized) {
        self.edits.lock().push((id, value));
    }

    /// A handle that reports into this mirror without borrowing it.
    #[must_use]
    pub fn reporter(&self) -> ParamReporter {
        ParamReporter(Arc::clone(&self.edits))
    }

    /// Every parameter the plugin declares, in the order it listed them; empty
    /// until the read has landed.
    #[must_use]
    pub fn descriptors(&self) -> &[ParameterInfo] {
        self.declared.as_ref().map_or(&[], |d| &d.descriptors)
    }

    /// Where parameter `id` is in [`descriptors`](Self::descriptors); `None`
    /// for one the plugin does not declare (or before the read lands).
    #[must_use]
    pub fn position(&self, id: ParamId) -> Option<usize> {
        self.declared.as_ref()?.index.get(&id).copied()
    }

    /// The value of the parameter at `position` in
    /// [`descriptors`](Self::descriptors): the latest the plugin reported or the
    /// host set, else the last one drained.
    ///
    /// # Panics
    ///
    /// If `position` is not one [`position`](Self::position) returned for the
    /// current read: a landed read replaces the positions, so a position is
    /// never kept across one.
    #[must_use]
    pub fn value_at(&self, position: usize) -> Normalized {
        let declared = self
            .declared
            .as_ref()
            .expect("a position comes from a landed read");
        let id = declared.descriptors[position].id();
        self.latest(id).unwrap_or_else(|| declared.values[position])
    }

    /// The newest value queued for `id`, if one is.
    fn latest(&self, id: ParamId) -> Option<Normalized> {
        self.edits
            .lock()
            .iter()
            .rev()
            .find_map(|&(at, value)| (at == id).then_some(value))
    }

    /// Every value held, by id; empty until the read has landed.
    pub fn values(&self) -> impl Iterator<Item = (ParamId, Normalized)> + '_ {
        self.declared.iter().flat_map(|d| {
            d.descriptors
                .iter()
                .zip(&d.values)
                .map(|(info, &value)| (info.id(), value))
        })
    }

    /// Whether the load-time read has landed.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.declared.is_some()
    }

    /// Parameter `id`'s value: `None` while the read is in flight, `Some(None)`
    /// for a parameter the plugin did not list.
    pub fn get(&self, id: ParamId) -> Option<Option<Normalized>> {
        let declared = self.declared.as_ref()?;
        Some(
            declared
                .index
                .get(&id)
                .map(|&at| self.latest(id).unwrap_or(declared.values[at])),
        )
    }

    /// Applies the values the plugin reported or the host set since the last
    /// call, in order: each goes into the mirror and then to `apply` (a host
    /// moves the knob of the parameter's modulation lane with it). A value for a
    /// parameter the plugin does not declare reaches `apply` and is not kept.
    ///
    /// Does nothing until the read has landed, and the edits wait: one
    /// applied before it would be overwritten by a value read earlier. The
    /// other order has a window of its own: an edit reported while the read
    /// is running, before the read reaches its parameter, is applied after it
    /// and so replaces the value read, though the read is newer. Only a value
    /// changed again within that window without a report is affected (each
    /// change the plugin reports arrives here in order), and the next
    /// report corrects it.
    pub fn drain_edits(&mut self, mut apply: impl FnMut(ParamId, Normalized)) {
        let Some(declared) = self.declared.as_mut() else {
            return;
        };
        let edits = std::mem::take(&mut *self.edits.lock());
        for (id, value) in edits {
            if let Some(&at) = declared.index.get(&id) {
                declared.values[at] = value;
            }
            apply(id, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn n(v: f64) -> Normalized {
        Normalized::clamped(v)
    }

    use crate::ParameterChangeSink;
    use tutti_plugin_types::StateError;
    use tutti_plugin_types::{
        AnyThread, AutomationMode, AutomationModeError, Health, PluginFailure, RenderMode,
        TerminationReason,
    };
    use tutti_plugin_types::{ParameterInfo, PluginClass, PluginDescriptor};
    use tutti_units::Normalized;
    use tutti_units::ParamId;

    /// A backend listing `params` (address, value); a value of `None` is a
    /// parameter it lists but does not answer.
    struct FakeParams {
        params: Vec<(ParamId, Option<f32>)>,
        mode: Mode,
        /// The normalized default every listed parameter declares.
        default: f64,
    }

    /// How the fake answers: at once, not at all, or when the test says.
    enum Mode {
        /// Every answer is filled before the question returns.
        Ready,
        /// The list is unavailable (`None`).
        Unlisted,
        /// A value question's sender is dropped unanswered: the main thread the
        /// answer waited on is gone.
        ValuesCanceled,
        /// The list question's sender is dropped unanswered.
        ListCanceled,
        /// Each answer waits in the queue for the test to fill it.
        Held(Parked),
    }

    /// The answers a held fake owes, oldest first.
    type Parked = Arc<Mutex<Vec<Box<dyn Owed>>>>;

    /// A question asked and not answered: the sender and what it will carry.
    trait Owed: Send {
        fn canceled(&self) -> bool;
        fn fill(self: Box<Self>);
    }

    impl<T: Send> Owed for (tutti_plugin_types::AnswerSender<T>, T) {
        fn canceled(&self) -> bool {
            self.0.is_canceled()
        }
        fn fill(self: Box<Self>) {
            // A canceled receiver has no one to tell.
            drop(self.0.send(self.1));
        }
    }

    impl FakeParams {
        fn deliver<T: Send + 'static>(&self, value: T, canceled: bool) -> Answer<T> {
            if canceled {
                return Answer::channel().1;
            }
            match &self.mode {
                Mode::Held(parked) => {
                    let (tx, rx) = Answer::channel();
                    parked.lock().push(Box::new((tx, value)));
                    rx
                }
                _ => tutti_plugin_types::Answer::ready(value),
            }
        }
    }

    impl FakeParams {
        /// What the plugin lists, with no delivery in between.
        fn listed(&self) -> Option<Vec<ParameterInfo>> {
            if matches!(self.mode, Mode::Unlisted) {
                return None;
            }
            Some(
                self.params
                    .iter()
                    .map(|&(id, _)| {
                        ParameterInfo::new(id, "p").with_normalized_default(self.default)
                    })
                    .collect(),
            )
        }
    }

    impl AnyThread for FakeParams {
        fn parameter_descriptors(&self) -> Answer<Option<Vec<ParameterInfo>>> {
            self.deliver(self.listed(), matches!(self.mode, Mode::ListCanceled))
        }
        fn parameter_value(&self, id: ParamId) -> Answer<Option<Normalized>> {
            let value = self
                .params
                .iter()
                .find(|(a, _)| *a == id)
                .and_then(|(_, v)| *v)
                .map(|v| Normalized::clamped(f64::from(v)));
            self.deliver(value, matches!(self.mode, Mode::ValuesCanceled))
        }
        fn parameter_normalized(&self, _id: ParamId, _plain: f64) -> Answer<Option<Normalized>> {
            self.deliver(None, matches!(self.mode, Mode::ValuesCanceled))
        }
        fn set_parameter_value(
            &self,
            _id: ParamId,
            _value: Normalized,
        ) -> Result<(), tutti_plugin_types::UnknownParam> {
            Ok(())
        }
        fn bypassed(&self, _site: tutti_plugin_types::OwnBypass) -> Option<bool> {
            None
        }
        fn parameter_plain(&self, _id: ParamId, _value: Normalized) -> Option<f64> {
            None
        }
        fn parameter_text(&self, _id: ParamId, _value: Normalized) -> Option<String> {
            None
        }
        fn parameter_value_from_text(&self, _id: ParamId, _text: &str) -> Option<Normalized> {
            None
        }
        fn save_state(&self, _: tutti_plugin_types::StateUse) -> Result<Vec<u8>, StateError> {
            Err(StateError::NoStateRoute)
        }
        fn set_render_mode(&self, _mode: RenderMode) -> bool {
            false
        }
        fn set_automation_mode(&self, _mode: AutomationMode) -> Result<(), AutomationModeError> {
            Err(AutomationModeError::PluginDead)
        }
    }

    impl Health for FakeParams {
        fn is_crashed(&self) -> bool {
            false
        }
        fn crash_cause(&self) -> Option<PluginFailure> {
            None
        }
        fn rate_refusal(&self) -> Option<tutti_plugin_types::RateRefusal> {
            None
        }
        fn block_counts(&self) -> tutti_plugin_types::PipelineCounts {
            tutti_plugin_types::PipelineCounts::default()
        }
        fn refused_edits(&self) -> u64 {
            0
        }

        fn terminate(&self, _reason: TerminationReason) {}
    }

    /// A handle over [`FakeParams`], and the sink its reports go through
    /// (firing it is the plugin reporting a value).
    fn handle(params: Vec<(ParamId, Option<f32>)>) -> (PluginHandle, ParameterChangeSink) {
        handle_with(params, Mode::Ready)
    }

    fn handle_with(
        params: Vec<(ParamId, Option<f32>)>,
        mode: Mode,
    ) -> (PluginHandle, ParameterChangeSink) {
        handle_declaring(params, mode, 0.0)
    }

    /// [`handle_with`], whose parameters declare the normalized `default`.
    fn handle_declaring(
        params: Vec<(ParamId, Option<f32>)>,
        mode: Mode,
        default: f64,
    ) -> (PluginHandle, ParameterChangeSink) {
        let sink = ParameterChangeSink::default();
        let handle = PluginHandle::from_backend(
            Arc::new(FakeParams {
                params,
                mode,
                default,
            }),
            None,
            PluginDescriptor::new("", PluginClass::Unknown),
            Default::default(),
            sink.clone(),
        );
        (handle, sink)
    }

    /// Awaiting a `ParamRead` returns each listed parameter's descriptor and value by
    /// its id, and `land` makes the mirror ready with them.
    ///
    /// The fake lists two parameters and one it does not answer: the first two
    /// come back with their values, and the third reads as its declared default
    /// (`0.0`, the normalized default the fake's descriptor declares), so every
    /// declared parameter has a value.
    #[test]
    fn a_read_returns_the_descriptors_values_and_landing_it_makes_the_mirror_ready() {
        use tutti_units::ParamId;
        let opaque = ParamId::new(7);
        let index = ParamId::new(3);
        let silent = ParamId::new(9);
        let unlisted = ParamId::new(11);
        let (plugin, _) = handle(vec![
            (opaque, Some(0.25)),
            (index, Some(0.75)),
            (silent, None),
        ]);

        let (mut mirror, read) = PluginParamValues::start(&plugin);
        assert!(!mirror.is_ready(), "the mirror waits for its read");
        assert_eq!(
            mirror.get(opaque),
            None,
            "nothing is known before the read lands"
        );
        assert!(mirror.descriptors().is_empty());

        // On another thread: the read is `Send`.
        let snapshot = std::thread::spawn(move || {
            futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
        })
        .join()
        .unwrap()
        .expect("the plugin lists its parameters");

        mirror.land(snapshot);
        assert!(mirror.is_ready());
        let listed: Vec<ParamId> = mirror.descriptors().iter().map(|p| p.id()).collect();
        assert_eq!(listed, [opaque, index, silent], "in the plugin's order");
        assert_eq!(mirror.get(opaque), Some(Some(n(0.25))));
        assert_eq!(mirror.get(index), Some(Some(n(0.75))));
        assert_eq!(
            mirror.get(silent),
            Some(Some(n(0.0))),
            "an unanswered parameter reads as its declared default"
        );
        assert_eq!(mirror.get(unlisted), Some(None), "not declared: not listed");
        assert_eq!(mirror.position(index), Some(1));
        assert_eq!(mirror.position(unlisted), None);
    }

    /// A value the plugin reports reaches the mirror through the callback
    /// `start` installs, and waits for the read: drained before it lands it
    /// is held, not lost and not applied.
    #[test]
    fn a_reported_edit_waits_for_the_read_then_reaches_the_mirror() {
        use tutti_plugin_types::ParamEdit;
        use tutti_units::ParamId;
        let (plugin, sink) = handle(vec![(ParamId::new(1), Some(0.1))]);
        let (mut mirror, read) = PluginParamValues::start(&plugin);

        sink.fire(ParamEdit::change(ParamId::new(1), n(0.9)));
        let mut seen = Vec::new();
        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert!(seen.is_empty(), "an edit before the read lands waits");

        mirror.land(
            futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
                .expect("the plugin lists its parameters"),
        );
        assert_eq!(
            mirror.get(ParamId::new(1)),
            Some(Some(n(0.9))),
            "a queued report reads at once, before it is drained"
        );
        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert_eq!(seen, [(ParamId::new(1), n(0.9))]);
        assert_eq!(
            mirror.get(ParamId::new(1)),
            Some(Some(n(0.9))),
            "the edit replaces the value read"
        );

        mirror.drain_edits(|id, v| seen.push((id, v)));
        assert_eq!(
            seen,
            [(ParamId::new(1), n(0.9))],
            "a drained edit is consumed, not applied again"
        );
    }

    /// A value the host sets reads back at once and, once drained, stays; a
    /// second read landing replaces the declarations and the values together.
    #[test]
    fn a_host_set_reads_back_and_a_landed_read_replaces_everything() {
        use tutti_units::ParamId;
        let id = ParamId::new(5);
        let (plugin, _) = handle(vec![(id, Some(0.2))]);
        let (mut mirror, read) = PluginParamValues::start(&plugin);
        mirror.land(
            futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
                .expect("lists"),
        );

        mirror.report(id, n(0.6));
        assert_eq!(mirror.get(id), Some(Some(n(0.6))), "read-your-writes");
        let at = mirror.position(id).expect("declared");
        assert_eq!(mirror.value_at(at), n(0.6));
        mirror.drain_edits(|_, _| {});
        assert_eq!(mirror.get(id), Some(Some(n(0.6))), "kept after the drain");

        let (other, _) = handle(vec![(ParamId::new(8), Some(0.5))]);
        let (_, read) = PluginParamValues::start(&other);
        mirror.land(
            futures_lite::future::block_on(std::future::IntoFuture::into_future(read))
                .expect("lists"),
        );
        assert_eq!(mirror.get(id), Some(None), "the old declaration is gone");
        assert_eq!(mirror.get(ParamId::new(8)), Some(Some(n(0.5))));
    }

    /// Polls `read` once with a waker that does nothing: whether it is done.
    fn poll_once<F: Future + Unpin>(read: &mut F) -> Poll<F::Output> {
        Pin::new(read).poll(&mut Context::from_waker(std::task::Waker::noop()))
    }

    /// **A list the plugin cannot give is not "lists none".** The read comes back,
    /// to be awaited again, and the mirror it was started for is still not ready:
    /// landing an empty snapshot would make it ready with nothing in it for good.
    #[test]
    fn an_unavailable_list_gives_the_read_back_not_an_empty_snapshot() {
        let (plugin, _) = handle_with(vec![(ParamId::new(1), Some(0.5))], Mode::Unlisted);
        let (mirror, read) = PluginParamValues::start(&plugin);

        let again = futures_lite::future::block_on(read.into_future())
            .expect_err("the list is unavailable: the read is given back");
        assert!(!mirror.is_ready(), "nothing landed");
        // Given back whole: awaiting it asks the plugin again.
        assert!(futures_lite::future::block_on(again.into_future()).is_err());
    }

    /// **A main thread that ended before it answered is an unavailable list,
    /// and an unavailable value.** The dropped sender resolves the read; it never
    /// hangs. The list comes back for a retry, a value reads as its declared
    /// default.
    #[test]
    fn a_sender_dropped_unanswered_resolves_the_read_and_never_hangs() {
        let id = ParamId::new(4);
        let (plugin, _) = handle_with(vec![(id, Some(0.8))], Mode::ListCanceled);
        let read = ParamRead::of(&plugin);
        assert!(
            futures_lite::future::block_on(read.into_future()).is_err(),
            "a canceled list is an unavailable one"
        );

        let (plugin, _) = handle_declaring(vec![(id, Some(0.8))], Mode::ValuesCanceled, 0.25);
        let (mut mirror, read) = PluginParamValues::start(&plugin);
        mirror.land(
            futures_lite::future::block_on(read.into_future()).expect("the list was answered"),
        );
        assert_eq!(
            mirror.get(id),
            Some(Some(n(0.25))),
            "a canceled value reads as the declared default (0.25), not the plugin's 0.8 or 0"
        );
    }

    /// **A parameter listed twice keeps its first entry.**
    #[test]
    fn a_parameter_listed_twice_keeps_its_first_entry() {
        let id = ParamId::new(2);
        let (plugin, _) = handle(vec![(id, Some(0.25)), (id, Some(0.75))]);
        let (mut mirror, read) = PluginParamValues::start(&plugin);
        mirror.land(futures_lite::future::block_on(read.into_future()).expect("lists"));
        assert_eq!(mirror.descriptors().len(), 1);
        assert_eq!(mirror.get(id), Some(Some(n(0.25))));
    }

    /// **An awaited read holds no thread while the plugin is slow to answer.** The
    /// fake holds every answer. A single-threaded executor driven here polls the read
    /// to its first question and goes on: a task spawned after it runs to its end
    /// while the read is still unanswered, and each answer the test fills wakes
    /// the read one step on, to the snapshot. A read that blocked the thread would
    /// never let `run_until_stalled` return. No clock: the order is the proof.
    #[test]
    fn an_awaited_read_leaves_the_executor_thread_free_while_the_plugin_is_slow() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let id = ParamId::new(6);
        let parked = Parked::default();
        let (plugin, _) = handle_with(vec![(id, Some(0.5))], Mode::Held(Arc::clone(&parked)));
        let executor = tutti_platform::Executor::new();
        let read = executor.spawn(ParamRead::of(&plugin).into_future());

        executor.run_until_stalled();
        assert_eq!(parked.lock().len(), 1, "asked for the list, waiting");

        let free = Arc::new(AtomicBool::new(false));
        let other = Arc::clone(&free);
        executor
            .spawn(async move { other.store(true, Ordering::SeqCst) })
            .detach();
        executor.run_until_stalled();
        assert!(
            free.load(Ordering::SeqCst),
            "another task ran while the read waited on the plugin"
        );
        assert!(!read.is_finished(), "the read is still waiting");

        parked.lock().remove(0).fill();
        executor.run_until_stalled();
        assert_eq!(
            parked.lock().len(),
            1,
            "the list woke it: the value is asked"
        );
        assert!(!read.is_finished());

        parked.lock().remove(0).fill();
        executor.run_until_stalled();
        assert!(read.is_finished(), "the value woke it: the read ended");
        let mut read = read;
        let Poll::Ready(snapshot) = poll_once(&mut read) else {
            panic!("finished")
        };
        let snapshot = snapshot.expect("the plugin listed its parameters");
        assert_eq!(snapshot.values, [n(0.5)]);
    }

    /// **A read dropped mid-way cancels its question, and no later read sees the late
    /// reply.** The question in flight is a canceled sender (nothing holds it open), the
    /// plugin's late answer to it is discarded, and a second read of the same plugin
    /// resolves to its own answers.
    #[test]
    fn a_dropped_read_cancels_its_question_and_a_later_read_gets_its_own_answers() {
        let id = ParamId::new(3);
        let parked = Parked::default();
        let (plugin, _) = handle_with(vec![(id, Some(0.25))], Mode::Held(Arc::clone(&parked)));

        let mut first = ParamRead::of(&plugin).into_future();
        assert!(poll_once(&mut first).is_pending());
        assert_eq!(parked.lock().len(), 1);
        drop(first);
        assert!(
            parked.lock()[0].canceled(),
            "dropping the read cancels the question in flight"
        );

        let mut second = ParamRead::of(&plugin).into_future();
        assert!(poll_once(&mut second).is_pending());
        // The plugin answers the first read's question late: discarded quietly.
        let late = parked.lock().remove(0);
        late.fill();
        assert!(
            poll_once(&mut second).is_pending(),
            "not the second read's answer"
        );
        // Its own answers.
        parked.lock().remove(0).fill();
        assert!(
            poll_once(&mut second).is_pending(),
            "the value is asked next"
        );
        parked.lock().remove(0).fill();
        let Poll::Ready(Ok(snapshot)) = poll_once(&mut second) else {
            panic!("the second read ends with its own answers")
        };
        assert_eq!(snapshot.values, [n(0.25)]);
    }

    /// **A read polled after it resolved panics**, like every future here.
    #[test]
    #[should_panic(expected = "polled after it resolved")]
    fn a_read_polled_after_it_resolved_panics() {
        let (plugin, _) = handle(vec![]);
        let mut read = ParamRead::of(&plugin).into_future();
        assert!(matches!(poll_once(&mut read), Poll::Ready(Ok(_))));
        let _ = poll_once(&mut read);
    }
}
