//! [`Lifecycle`]: one plugin in whichever state it is in, and the interpreter
//! of [`Effect`]s against it.
//!
//! The pure core ([`step`]) decides; `Lifecycle` performs. It is written once
//! for every format that implements [`Loaded`]: [`drive`](Lifecycle::drive)
//! steps an event, applies each effect it emits, and feeds each reply back
//! until none remain.
//!
//! Everything here runs on the main thread. The Processor reaches the realtime
//! thread through a [`Hand`]; a [`Hand::local`] has no realtime thread, so the
//! Processor stays in the [`Seat`] and a single-thread user renders through
//! [`Lifecycle::processor`].

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{
    Control, Controller, Effect, Event, HostOp, Layout, Loaded, MainThreadMarker, ModeTold, Notice,
    PluginError, PluginWiring, Prepare, Rebuilt, Refused, Reply, SampleRate, State, Stuck, Told,
    step,
};

/// The main thread's end of the Processor's trip to the realtime thread.
///
/// [`local`](Self::local) is the end for a user with no realtime thread:
/// [`Effect::Lend`] does nothing and [`Effect::Recall`] answers
/// [`Reply::Returned`] at once. [`threaded`](Self::threaded) pairs it with the
/// realtime thread's [`tutti_rt::Rt`]: `Lend` moves the
/// Processor into the handoff, `Recall` asks for it back, and
/// [`Lifecycle::drive`] collects it on a tick and steps [`Event::Returned`].
#[derive(Debug)]
pub struct Hand<P> {
    /// `None` for a local hand.
    end: Option<tutti_rt::Handoff<P>>,
    /// The control end [`install`](Self::install) named: it replaces `end` the
    /// next time the Processor is held here.
    successor: Option<tutti_rt::Handoff<P>>,
    _processor: PhantomData<fn() -> P>,
}

impl<P> Hand<P> {
    /// The end for a user whose Processor never leaves the main thread.
    #[must_use]
    pub fn local() -> Self {
        Self {
            end: None,
            successor: None,
            _processor: PhantomData,
        }
    }

    /// The end for a user that renders on a realtime thread, and that
    /// thread's end, which polls between blocks ([`tutti_rt::Rt::poll`], with
    /// [`Processor::stop`](crate::Processor::stop) as its last step).
    #[must_use]
    pub fn threaded() -> (Self, tutti_rt::Rt<P>)
    where
        P: Send,
    {
        let (end, rt) = tutti_rt::handoff();
        (
            Self {
                end: Some(end),
                successor: None,
                _processor: PhantomData,
            },
            rt,
        )
    }

    /// Names `successor` as the realtime end the Processor moves to: the end of
    /// the node that replaces the one holding the current end, with other
    /// ports over the same plugin. [`HostOp::Relend`] then moves the Processor
    /// without restarting the plugin.
    ///
    /// The Processor does not move at this call. It stays where it is until the main
    /// thread holds it (recalled, or returned and collected), and the next
    /// [`Effect::Lend`] then lends it to `successor` instead of the old
    /// end. So:
    ///
    /// - A Processor never drops on the realtime thread because of an
    ///   `install`: the swap happens only while the main thread holds the
    ///   Processor, and the old end is dropped then, empty, on this thread.
    /// - Once the Processor is lent to the successor the old [`tutti_rt::Rt`]
    ///   holds none and renders without one; [`pending`](Self::pending) is
    ///   `false` from the swap on.
    /// - A second `install` before the swap replaces the first: the end of
    ///   the replaced successor is never lent to and polls nothing, as one whose
    ///   node was dropped does.
    /// - The old [`tutti_rt::Rt`] dropped **before** the swap, while it still
    ///   holds the Processor, drops the Processor with it, on the thread that
    ///   drops it. The seat then stays lent: nothing returns the Processor, the
    ///   restart stays parked and the plugin cannot be activated again until
    ///   the [`Lifecycle`] is dropped. A user whose end is dropped while a recall
    ///   is raised returns the Processor first (`tutti-plugin-host`'s node does,
    ///   in its `Drop`).
    ///
    /// A local hand becomes a threaded one at the swap.
    ///
    /// `tutti-plugin-host`'s commit is the only caller: it installs once the
    /// node over the successor's end is in its graph.
    pub fn install(&mut self, successor: Successor<P>) {
        self.successor = Some(successor.0);
    }

    /// Whether an [`install`](Self::install) has not yet moved the Processor's
    /// way to its new realtime end: the Processor may still be in the old one.
    #[must_use]
    pub fn pending(&self) -> bool {
        self.successor.is_some()
    }

    /// Makes the successor the end, while the Processor is held on the main
    /// thread: the old end is empty, and drops here.
    fn settle(&mut self) {
        if let Some(next) = self.successor.take() {
            self.end = Some(next);
        }
    }
}

/// The control end of a new realtime end, made with that end by
/// [`Successor::new`] and named to a [`Hand`] by [`Hand::install`].
///
/// Dropping one nobody installed does nothing: the realtime end it was made
/// with is empty and polls nothing.
#[derive(Debug)]
pub struct Successor<P>(tutti_rt::Handoff<P>);

impl<P: Send> Successor<P> {
    /// A new realtime end, and the control end that lends to it. The end stays
    /// empty, and lent nothing, until a [`Hand`] it is installed in lends its
    /// Processor to it; a node over it renders silence until then.
    #[must_use]
    pub fn new() -> (Self, tutti_rt::Rt<P>) {
        let (end, rt) = tutti_rt::handoff();
        (Self(end), rt)
    }
}

/// Where the Processor of an active plugin is.
#[derive(Debug)]
pub enum Seat<P> {
    /// On the main thread: single-thread use, between a rejoin and a lend, or
    /// after a recall.
    Held(P),
    /// On the realtime thread, which returns it only between blocks.
    Lent,
}

/// A plugin in whichever state it is in.
///
/// The typestate of [`Loaded`] stays in the enum: a transition takes the
/// plugin out, consumes it, and puts back what the result names.
#[derive(Debug)]
pub enum Lifecycle<L: Loaded> {
    /// Configured, not rendering.
    Loaded(L),
    /// Split. The Controller is here; the Processor is held or lent.
    ///
    /// `seat` is declared before `controller` so a dropped `Lifecycle` drops a
    /// held Processor first: the Controller destroys the plugin, and a
    /// Processor must not outlive it. A Processor that is `Lent` is not here to
    /// drop; the main thread recalls it before dropping the `Lifecycle` (the
    /// threaded `Hand` enforces that, and a local hand never lends).
    Active {
        /// The realtime half, or where it is.
        seat: Seat<L::Processor>,
        /// The main-thread half.
        controller: L::Controller,
    },
    /// The plugin refused to run; it is loaded and renders nothing.
    Refused {
        /// The plugin.
        plugin: L,
        /// The rate refused, or `None` when it was a rebuild that was refused.
        rate: Option<SampleRate>,
    },
    /// A transition left no plugin, or the process died. [`Lifecycle::drive`]
    /// sends [`Event::Died`] for it, and nothing leaves this state.
    Poisoned,
}

fn panicked(what: &str) -> PluginError {
    PluginError::Other(format!("{what} panicked"))
}

impl<L: Loaded> Lifecycle<L> {
    /// A freshly loaded plugin.
    #[must_use]
    pub fn new(plugin: L) -> Self {
        Self::Loaded(plugin)
    }

    /// The core's state for a freshly loaded plugin of this format.
    #[must_use]
    pub fn initial_state() -> State {
        State::new(L::RESTARTS)
    }

    /// A lifecycle over a plugin whose shell activated it itself, with the
    /// core's state as if [`Lifecycle::drive`] had: for a hosting whose
    /// activation needs no main thread (a served plugin, `tutti-plugin-ipc`'s
    /// `Ipc::activate_anywhere`), so a node over it is built on any thread.
    ///
    /// `wiring` is what the plugin reports now: its layout, latency and tail
    /// are the figures the core records. The Processor is lent through `hand`
    /// as an activation lends it. Nothing here calls into the plugin.
    ///
    /// The state is stepped through the pure core with the replies an
    /// activation gets, so it cannot differ from the driven one
    /// (`an_adopted_plugin_is_in_the_state_a_driven_activation_leaves`).
    ///
    /// # Panics
    ///
    /// If the core plans an activation other than as one call, plans an
    /// effect an activation does not answer, or is left anywhere but active
    /// with nothing planned: each a change to the core that `adopt` has not
    /// been taught, never a property of the plugin.
    #[must_use]
    pub fn adopt(
        processor: L::Processor,
        controller: L::Controller,
        prepare: Prepare,
        wiring: &PluginWiring,
        hand: &mut Hand<L::Processor>,
    ) -> (Self, State) {
        let mut lifecycle = Self::Active {
            seat: Seat::Held(processor),
            controller,
        };
        // The op plans the activation, which the shell has done; each effect
        // the activation's answer plans is answered from `wiring`.
        let (mut state, planned) =
            step(Self::initial_state(), Event::Op(HostOp::Activate(prepare)));
        assert!(
            matches!(planned.as_slice(), [Effect::CallActivate(_)]),
            "an activation is planned as one call, which the shell made: {planned:?}"
        );
        let mut replies = VecDeque::from([Reply::Activated]);
        while let Some(reply) = replies.pop_front() {
            let (next, effects) = step(state, Event::Replied(reply));
            state = next;
            for effect in effects {
                // Every variant named, so a new one does not compile until it
                // is decided here (a silent skip could leave the core short of
                // active).
                let reply = match effect {
                    Effect::ReadLatency => Reply::Latency(wiring.latency_samples),
                    Effect::ReadTail => Reply::Tail(wiring.tail),
                    Effect::ReadLayout => Reply::Layout(Layout::from(wiring)),
                    // The Processor is lent below, as `lend` answers.
                    Effect::Lend => Reply::Done,
                    // Delivered by the driver, which answers it so.
                    Effect::Notify(_) => Reply::Done,
                    effect @ (Effect::CallActivate(_)
                    | Effect::CallDeactivate
                    | Effect::Rebuild
                    | Effect::Recall
                    | Effect::SetMode(_)
                    | Effect::ReadParams(_)
                    | Effect::RebuildMidiMapping
                    | Effect::RunMainThread
                    | Effect::RunTimer(_)
                    | Effect::ServiceFd(_)) => {
                        unreachable!("an activation does not plan {effect:?}")
                    }
                };
                replies.push_back(reply);
            }
        }
        assert!(
            state.renders() && !state.is_busy(),
            "an adopted plugin is active with nothing planned, as a driven activation leaves it"
        );
        lifecycle.lend(hand);
        (lifecycle, state)
    }

    /// Whether a transition has left no plugin.
    #[must_use]
    pub fn is_poisoned(&self) -> bool {
        matches!(self, Self::Poisoned)
    }

    /// The control surface, in any state that has a plugin, lent or not.
    pub fn control(&mut self) -> Option<&mut dyn Control> {
        match self {
            Self::Loaded(plugin) | Self::Refused { plugin, .. } => Some(plugin),
            Self::Active { controller, .. } => Some(controller),
            Self::Poisoned => None,
        }
    }

    /// The Processor, while it is held. A single-thread user renders through
    /// this.
    pub fn processor(&mut self) -> Option<&mut L::Processor> {
        match self {
            Self::Active {
                seat: Seat::Held(processor),
                ..
            } => Some(processor),
            _ => None,
        }
    }

    /// [`control`](Self::control), shared: for the reads that take `&self`.
    #[must_use]
    pub fn control_ref(&self) -> Option<&dyn Control> {
        match self {
            Self::Loaded(plugin) | Self::Refused { plugin, .. } => Some(plugin),
            Self::Active { controller, .. } => Some(controller),
            Self::Poisoned => None,
        }
    }

    /// Interprets one effect against the plugin. Control thread.
    ///
    /// [`Effect::Notify`] is the driver's: it is delivered by
    /// [`drive`](Self::drive), and here answers [`Reply::Done`].
    ///
    /// A panic out of the plugin leaves [`Lifecycle::Poisoned`], not a
    /// half-moved plugin, whichever call it came from. `hand` is the
    /// Processor's way to the realtime thread: a local one needs nothing from
    /// the effects that move it.
    pub fn apply(
        &mut self,
        effect: Effect,
        hand: &mut Hand<L::Processor>,
        main: MainThreadMarker,
    ) -> Reply {
        match effect {
            Effect::CallActivate(prepare) => self.activate(main, prepare),
            Effect::CallDeactivate => self.deactivate(main),
            Effect::Rebuild => self.rebuild(main),
            Effect::Lend => self.lend(hand),
            Effect::Recall => self.recall(hand),
            other => self
                .guarded(|this| this.apply_in_place(other, main))
                .unwrap_or(Reply::NotActive),
        }
    }

    /// Runs `f`; a panic out of it leaves `Poisoned` and `None`.
    fn guarded<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        match catch_unwind(AssertUnwindSafe(|| f(self))) {
            Ok(result) => Some(result),
            Err(_) => {
                let lost = std::mem::replace(self, Self::Poisoned);
                // Destroying a plugin that just panicked may panic too.
                let _ = catch_unwind(AssertUnwindSafe(|| drop(lost)));
                None
            }
        }
    }

    /// The effects that act on a plugin where it is.
    fn apply_in_place(&mut self, effect: Effect, main: MainThreadMarker) -> Reply {
        match effect {
            Effect::CallActivate(_)
            | Effect::CallDeactivate
            | Effect::Rebuild
            | Effect::Lend
            | Effect::Recall => {
                unreachable!("transitions and moves are applied by `apply`")
            }
            Effect::SetMode(mode) => match self.control() {
                Some(control) => match control.set_render_mode(main, mode) {
                    ModeTold::Took => Reply::Answered(Told::ModeTook(mode)),
                    ModeTold::Refused => Reply::Answered(Told::ModeRefused(mode)),
                    // No mode to refuse: what the plugin runs in, and whether
                    // it renders, is as it was.
                    ModeTold::Unsupported => Reply::Done,
                    ModeTold::Asked => Reply::Asked,
                },
                None => Reply::NotActive,
            },
            Effect::ReadLatency => match self {
                Self::Active { controller, .. } => Reply::Latency(controller.latency(main)),
                _ => Reply::NotActive,
            },
            Effect::ReadTail => match self {
                Self::Active { controller, .. } => Reply::Tail(controller.tail(main)),
                _ => Reply::NotActive,
            },
            Effect::ReadLayout => match self {
                Self::Active { controller, .. } => {
                    Reply::Layout(Layout::from(&controller.wiring(main)))
                }
                _ => Reply::NotActive,
            },
            Effect::ReadParams(scope) => self.with_control(|c| c.rescan_parameters(main, scope)),
            Effect::RebuildMidiMapping => self.with_control(|c| c.rebuild_midi_mapping(main)),
            Effect::RunMainThread => self.with_control(|c| c.run_main_thread(main)),
            Effect::RunTimer(id) => self.with_control(|c| c.run_timer(main, id)),
            Effect::ServiceFd(id) => self.with_control(|c| c.service_fd(main, id)),
            Effect::Notify(_) => Reply::Done,
        }
    }

    /// [`Effect::Lend`]: a threaded hand takes the held Processor. A lend the
    /// handoff refuses (a recall still raised, a slot not yet emptied) keeps it
    /// held, and [`drive`](Self::drive) retries on the next tick.
    fn lend(&mut self, hand: &mut Hand<L::Processor>) -> Reply {
        let Self::Active { seat, .. } = self else {
            return Reply::NotActive;
        };
        // The Processor is held here, so the old end holds none.
        if matches!(seat, Seat::Held(_)) {
            hand.settle();
        }
        // A local hand has no realtime thread: the seat keeps it.
        let (Some(end), Seat::Held(_)) = (hand.end.as_mut(), &seat) else {
            return Reply::Done;
        };
        let Seat::Held(processor) = std::mem::replace(seat, Seat::Lent) else {
            unreachable!("matched held above");
        };
        if let Err(processor) = end.lend(processor) {
            *seat = Seat::Held(processor);
        }
        Reply::Done
    }

    /// [`Effect::Recall`]: a held Processor is here; a lent one is withdrawn
    /// at once if the realtime thread has not taken it, or asked back
    /// ([`Reply::Lent`]) and collected by a later [`drive`](Self::drive).
    fn recall(&mut self, hand: &mut Hand<L::Processor>) -> Reply {
        match self {
            Self::Active {
                seat: Seat::Held(_),
                ..
            } => Reply::Returned,
            Self::Active { seat, .. } => {
                match hand.end.as_mut().and_then(tutti_rt::Handoff::recall) {
                    Some(processor) => {
                        *seat = Seat::Held(processor);
                        Reply::Returned
                    }
                    None => Reply::Lent,
                }
            }
            _ => Reply::NotActive,
        }
    }

    /// On a tick: a Processor the realtime thread returned is collected
    /// (`true`, and the caller steps [`Event::Returned`]), and one a refused
    /// lend kept held is offered again.
    fn trade(&mut self, hand: &mut Hand<L::Processor>, state: &State) -> bool {
        let Self::Active { seat, .. } = self else {
            return false;
        };
        if matches!(seat, Seat::Held(_)) {
            hand.settle();
        }
        let Some(end) = hand.end.as_mut() else {
            return false;
        };
        match seat {
            Seat::Lent => match end.take() {
                // Held again either way. One that came back because its node
                // was dropped was not stopped, and is not lent again: the
                // audio end is gone, so the lend below is refused.
                Some(
                    tutti_rt::Handback::Recalled(processor)
                    | tutti_rt::Handback::Dropped(processor),
                ) => {
                    *seat = Seat::Held(processor);
                    true
                }
                None => false,
            },
            // Held while active, with no plan parked on it: a lend was
            // refused, so offer it again.
            Seat::Held(_) if !state.is_parked() && state.renders() => {
                let Seat::Held(processor) = std::mem::replace(seat, Seat::Lent) else {
                    unreachable!("matched held above");
                };
                if let Err(processor) = end.lend(processor) {
                    *seat = Seat::Held(processor);
                }
                false
            }
            Seat::Held(_) => false,
        }
    }

    fn with_control(&mut self, f: impl FnOnce(&mut dyn Control)) -> Reply {
        match self.control() {
            Some(control) => {
                f(control);
                Reply::Done
            }
            None => Reply::NotActive,
        }
    }

    fn activate(&mut self, main: MainThreadMarker, prepare: Prepare) -> Reply {
        let plugin = match std::mem::replace(self, Self::Poisoned) {
            Self::Loaded(plugin) | Self::Refused { plugin, .. } => plugin,
            other => {
                *self = other;
                return Reply::NotActive;
            }
        };
        match catch_unwind(AssertUnwindSafe(|| plugin.activate(main, prepare))) {
            Ok(Ok((processor, controller))) => {
                *self = Self::Active {
                    seat: Seat::Held(processor),
                    controller,
                };
                Reply::Activated
            }
            Ok(Err(Refused { plugin, reason })) => {
                if let Some(plugin) = plugin {
                    *self = Self::Refused {
                        plugin,
                        rate: Some(prepare.rate()),
                    };
                }
                Reply::Refused(reason)
            }
            Err(_) => Reply::Refused(panicked("activate")),
        }
    }

    fn deactivate(&mut self, main: MainThreadMarker) -> Reply {
        let (controller, processor) = match std::mem::replace(self, Self::Poisoned) {
            Self::Active {
                seat: Seat::Held(processor),
                controller,
            } => (controller, processor),
            other => {
                *self = other;
                return Reply::NotActive;
            }
        };
        match catch_unwind(AssertUnwindSafe(|| {
            L::deactivate(main, processor, controller)
        })) {
            Ok(Ok(plugin)) => {
                *self = Self::Loaded(plugin);
                Reply::Deactivated
            }
            Ok(Err(Stuck {
                processor,
                controller,
                reason,
            })) => {
                *self = Self::Active {
                    seat: Seat::Held(processor),
                    controller,
                };
                Reply::Stuck(reason)
            }
            Err(_) => Reply::Refused(panicked("deactivate")),
        }
    }

    fn rebuild(&mut self, main: MainThreadMarker) -> Reply {
        let (plugin, rate) = match std::mem::replace(self, Self::Poisoned) {
            Self::Loaded(plugin) => (plugin, None),
            Self::Refused { plugin, rate } => (plugin, rate),
            other => {
                *self = other;
                return Reply::NotActive;
            }
        };
        match catch_unwind(AssertUnwindSafe(|| plugin.rebuild(main))) {
            Ok(Ok(Rebuilt { plugin, restored })) => {
                *self = Self::Loaded(plugin);
                Reply::Rebuilt { restored }
            }
            Ok(Err(Refused { plugin, reason })) => {
                if let Some(plugin) = plugin {
                    *self = Self::Refused { plugin, rate };
                }
                Reply::Refused(reason)
            }
            Err(_) => Reply::Refused(panicked("rebuild")),
        }
    }

    /// Steps `event`, applies every effect it emits, and feeds each reply back
    /// until none remain. `notify` receives the [`Notice`]s in the order they
    /// happened. Returns the new state.
    ///
    /// A transition that leaves no plugin ([`Refused::plugin`] of `None`, a
    /// panic) poisons, and [`Event::Died`] is stepped for it, so the core's
    /// `Poisoned` is reached the one way the design allows. A panic out of any
    /// other call (a read, the idle flush, main-thread work) is told with
    /// [`Notice::Lost`]; a transition's loss is told by its [`Notice::Refused`].
    ///
    /// Call it on every idle pass of the host's loop, with [`Event::Tick`]: the
    /// Controller's retry of refused parameter edits runs there only.
    ///
    /// While the Processor is lent, a plan that needs both halves is parked in
    /// the state by the core; this returns with it parked, and a later `drive`
    /// runs it once [`Event::Returned`] arrives.
    ///
    /// With a threaded [`Hand`], a tick also collects a Processor the realtime
    /// thread returned and steps [`Event::Returned`] for it first, and offers
    /// again one a refused lend kept here.
    pub fn drive(
        &mut self,
        mut state: State,
        event: Event,
        hand: &mut Hand<L::Processor>,
        main: MainThreadMarker,
        mut notify: impl FnMut(Notice),
    ) -> State {
        if matches!(event, Event::Tick) && self.trade(hand, &state) {
            // The realtime thread returned the Processor: a parked plan runs.
            state = self.run(state, Event::Returned, hand, main, &mut notify);
        }
        self.run(state, event, hand, main, &mut notify)
    }

    /// [`drive`](Self::drive)'s one step and the effects it emits.
    fn run(
        &mut self,
        state: State,
        event: Event,
        hand: &mut Hand<L::Processor>,
        main: MainThreadMarker,
        notify: &mut dyn FnMut(Notice),
    ) -> State {
        if matches!(event, Event::Tick) {
            // The idle pass is the Controller's tick to retry what a full edit
            // ring refused.
            self.guarded(|this| {
                if let Some(control) = this.control() {
                    control.flush_edits(main);
                }
            });
            if self.is_poisoned() {
                notify(Notice::Lost {
                    reason: panicked("a call into the plugin"),
                });
                return step(state, Event::Died).0;
            }
        }
        let (mut state, effects) = step(state, event);
        let mut queue: VecDeque<Effect> = effects.into_iter().collect();
        while let Some(effect) = queue.pop_front() {
            if let Effect::Notify(notice) = effect {
                notify(notice);
                continue;
            }
            // A transition that loses the plugin answers `Refused`, which the
            // core turns into its own notice; any other call tells the host here.
            let transition = matches!(
                effect,
                Effect::CallActivate(_) | Effect::CallDeactivate | Effect::Rebuild
            );
            let reply = self.apply(effect, hand, main);
            let (next, effects) = step(state, Event::Replied(reply));
            state = next;
            for effect in effects.into_iter().rev() {
                queue.push_front(effect);
            }
            if self.is_poisoned() && state.stage() != crate::Stage::Poisoned {
                let (next, died) = step(state, Event::Died);
                state = next;
                // A refusal the loss cut short is told here, before the loss.
                for effect in died {
                    if let Effect::Notify(notice) = effect {
                        notify(notice);
                    }
                }
                if !transition {
                    notify(Notice::Lost {
                        reason: panicked("a call into the plugin"),
                    });
                }
                // What already happened is still told; the rest of the plan
                // has no plugin to run on.
                for effect in queue.drain(..) {
                    if let Effect::Notify(notice) = effect {
                        notify(notice);
                    }
                }
            }
        }
        state
    }
}

impl From<&PluginWiring> for Layout {
    fn from(wiring: &PluginWiring) -> Self {
        Self {
            inputs: wiring.inputs.clone(),
            outputs: wiring.outputs.clone(),
        }
    }
}

#[cfg(test)]
mod tests;
