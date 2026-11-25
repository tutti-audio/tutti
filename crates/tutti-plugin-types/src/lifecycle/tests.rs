//! State-table tests and properties of [`step`], with no plugin loaded.
//!
//! A [`Shell`] stands in for the interpreter: it performs each effect against
//! a model of the plugin (is it split, where is the Processor) and feeds the
//! reply back, recording what each step did. Properties run arbitrary event
//! sequences through it.

use std::collections::VecDeque;

use proptest::prelude::*;

use super::*;
use crate::{ChannelLayout, PluginBus};
use tutti_plugin_ipc_ffi::FdId;

// ---------------------------------------------------------------- vocabulary

/// An effect with its payload reduced to what the tests compare.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fx {
    Activate(Prepare),
    Deactivate,
    Rebuild,
    Recall,
    Lend,
    SetMode(RenderMode),
    Retune(SampleRate),
    Latency,
    Tail,
    Layout,
    Params(RescanScope),
    Main,
    Timer(u32),
    Fd(i32),
    LatencyNotice(usize),
    TailNotice,
    IoNotice,
    ParamValues,
    ParamTitles,
    MidiMapping,
    MidiMapChangedNotice,
    NoteExprNotice,
    Reloaded,
    StateNotRestoredNotice,
    RefusedNotice,
    ModeRefusedNotice,
    StuckNotice,
    LostNotice,
    TransportNotice(TransportRequest),
}

fn tag(effect: &Effect) -> Fx {
    match effect {
        Effect::CallActivate(p) => Fx::Activate(*p),
        Effect::CallDeactivate => Fx::Deactivate,
        Effect::Rebuild => Fx::Rebuild,
        Effect::Recall => Fx::Recall,
        Effect::Lend => Fx::Lend,
        Effect::SetMode(mode) => Fx::SetMode(*mode),
        Effect::Retune(p) => Fx::Retune(p.rate()),
        Effect::ReadLatency => Fx::Latency,
        Effect::ReadTail => Fx::Tail,
        Effect::ReadLayout => Fx::Layout,
        Effect::ReadParams(s) => Fx::Params(*s),
        Effect::RebuildMidiMapping => Fx::MidiMapping,
        Effect::RunMainThread => Fx::Main,
        Effect::RunTimer(id) => Fx::Timer(id.0),
        Effect::ServiceFd(id) => Fx::Fd(id.get()),
        Effect::Notify(Notice::LatencyChanged(s)) => Fx::LatencyNotice(s.get()),
        Effect::Notify(Notice::TailChanged(_)) => Fx::TailNotice,
        Effect::Notify(Notice::IoChanged) => Fx::IoNotice,
        Effect::Notify(Notice::ParamValuesChanged) => Fx::ParamValues,
        Effect::Notify(Notice::ParamTitlesChanged) => Fx::ParamTitles,
        Effect::Notify(Notice::NoteExpressionChanged) => Fx::NoteExprNotice,
        Effect::Notify(Notice::MidiMapChanged) => Fx::MidiMapChangedNotice,
        Effect::Notify(Notice::Reloaded) => Fx::Reloaded,
        Effect::Notify(Notice::StateNotRestored { .. }) => Fx::StateNotRestoredNotice,
        Effect::Notify(Notice::Refused { .. }) => Fx::RefusedNotice,
        Effect::Notify(Notice::ModeRefused { .. }) => Fx::ModeRefusedNotice,
        Effect::Notify(Notice::Stuck { .. }) => Fx::StuckNotice,
        Effect::Notify(Notice::Lost { .. }) => Fx::LostNotice,
        Effect::Notify(Notice::Transport(asked)) => Fx::TransportNotice(*asked),
    }
}

fn prep(n: u8) -> Prepare {
    let rate = SampleRate::of(if n.is_multiple_of(2) {
        48_000.0
    } else {
        44_100.0
    });
    Prepare::new(rate, MaxBlock::of(Samples::new(64 << (n % 3))))
        .with_format(if n.is_multiple_of(5) {
            FloatWidth::F64
        } else {
            FloatWidth::F32
        })
        .with_mode(if n.is_multiple_of(7) {
            RenderMode::Offline
        } else {
            RenderMode::Realtime
        })
}

fn err() -> PluginError {
    PluginError::Other("test".into())
}

/// One direct step, for the state tables.
fn go(state: State, event: Event) -> (State, Vec<Fx>) {
    let (state, out) = step(state, event);
    (state, out.iter().map(tag).collect())
}

fn reply(state: State, r: Reply) -> (State, Vec<Fx>) {
    go(state, Event::Replied(r))
}

fn stereo() -> Layout {
    Layout {
        inputs: vec![PluginBus::main(ChannelLayout::STEREO)],
        outputs: vec![PluginBus::main(ChannelLayout::STEREO)],
    }
}

/// An active plugin that has answered its first reads, with the Processor
/// lent, reached by the steps a shell takes.
fn active(rules: RestartRules, p: Prepare) -> State {
    let s = State::new(rules);
    let (s, _) = go(s, Event::Op(HostOp::Activate(p)));
    let (s, _) = reply(s, Reply::Activated);
    let (s, _) = reply(s, Reply::Latency(Samples::new(0)));
    let (s, _) = reply(s, Reply::Tail(PluginTail::Unknown));
    let (s, _) = reply(s, Reply::Layout(stereo()));
    let (s, _) = reply(s, Reply::Done);
    assert_eq!(s.stage(), Stage::Active);
    s
}

fn refused(rules: RestartRules, p: Prepare) -> State {
    let (s, _) = go(State::new(rules), Event::Op(HostOp::Activate(p)));
    let (s, _) = reply(s, Reply::Refused(err()));
    assert!(matches!(s.stage(), Stage::Refused { .. }));
    s
}

fn req(s: State, r: Request) -> State {
    go(s, Event::Request(r)).0
}

/// A plugin's transport request is the application's to answer, not the
/// plugin's own affair: whatever the plugin is doing, it comes out as the one
/// notice, with nothing planned and nothing left pending.
/// Spec: clap.ext.transport_control.host.request_start
#[test]
fn a_transport_request_is_passed_on_as_one_notice_and_changes_nothing() {
    let p = prep(0);
    let asked = TransportRequest::Start;
    let (after, fx) = go(
        active(RESTARTS, p),
        Event::Request(Request::Transport(asked)),
    );
    assert_eq!(fx, [Fx::TransportNotice(asked)]);
    assert!(after.is_quiet(), "nothing is left for a tick to do");

    let (_, fx) = go(
        State::new(RESTARTS),
        Event::Request(Request::Transport(asked)),
    );
    assert_eq!(
        fx,
        [Fx::TransportNotice(asked)],
        "also before an activation"
    );
}

const REST: RestartRules = RestartRules::NONE;
const RESTARTS: RestartRules = RestartRules::CLAP;

// -------------------------------------------------------------------- shell

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    ActivateOp,
    ReconfigureOp,
    DeactivateOp,
    RelendOp,
    SetModeOp,
    RetuneOp,
    Answered,
    Request,
    Tick,
    Activated,
    Refused,
    Reply,
    Returned,
    Died,
}

fn kind(event: &Event) -> Kind {
    match event {
        Event::Op(HostOp::Activate(_)) => Kind::ActivateOp,
        Event::Op(HostOp::Reconfigure(_)) => Kind::ReconfigureOp,
        Event::Op(HostOp::Deactivate) => Kind::DeactivateOp,
        Event::Op(HostOp::Relend) => Kind::RelendOp,
        Event::Op(HostOp::SetMode(_)) => Kind::SetModeOp,
        Event::Op(HostOp::Retune) => Kind::RetuneOp,
        Event::Answered(_) => Kind::Answered,
        Event::Request(_) => Kind::Request,
        Event::Tick => Kind::Tick,
        Event::Replied(Reply::Activated) => Kind::Activated,
        Event::Replied(Reply::Refused(_)) => Kind::Refused,
        Event::Replied(_) => Kind::Reply,
        Event::Returned => Kind::Returned,
        Event::Died => Kind::Died,
    }
}

/// What one step did, and what the model of the plugin says about it.
#[derive(Debug)]
struct Rec {
    kind: Kind,
    before: Stage,
    after: Stage,
    fx: Vec<Fx>,
    /// The model had the Processor away when a rejoin was emitted.
    rejoin_while_out: bool,
    /// An activation was emitted for a plugin the model had split, or a
    /// deactivation for one it had not.
    transition_out_of_turn: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Refuse {
    Never,
    Always,
    Choice,
}

/// Where the realtime thread is: gives the Processor back at once (a single
/// thread user) or only when the test calls `return_processor`.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Rt {
    Local,
    Thread,
}

struct Shell {
    state: State,
    /// The model's Processor is on the main thread.
    hand: bool,
    /// A recall was answered `Lent` and the Processor has not come back.
    returning: bool,
    real_active: bool,
    real_latency: usize,
    refuse: Refuse,
    rt: Rt,
    choices: Vec<u8>,
    at: usize,
    during_activate: Vec<Request>,
    /// Host ops raised between an effect and its reply.
    inject: Vec<HostOp>,
    trace: Vec<Rec>,
}

impl Shell {
    fn new(rules: RestartRules, refuse: Refuse, rt: Rt, choices: Vec<u8>) -> Self {
        Self {
            state: State::new(rules),
            hand: true,
            returning: false,
            real_active: false,
            real_latency: 0,
            refuse,
            rt,
            choices,
            at: 0,
            during_activate: Vec::new(),
            inject: Vec::new(),
            trace: Vec::new(),
        }
    }

    /// A shell whose plugin always does as asked, on one thread.
    fn easy(rules: RestartRules) -> Self {
        Self::new(rules, Refuse::Never, Rt::Local, vec![1])
    }

    fn pick(&mut self, n: u8) -> u8 {
        if self.choices.is_empty() {
            return 1;
        }
        let c = self.choices[self.at % self.choices.len()];
        self.at += 1;
        c % n
    }

    fn feed(&mut self, event: Event, q: &mut VecDeque<Effect>, all: &mut Vec<Fx>) {
        let k = kind(&event);
        let before = self.state.stage();
        let (state, out) = step(std::mem::replace(&mut self.state, State::new(REST)), event);
        self.state = state;
        assert!(out.len() <= EFFECT_CAPACITY);
        let mut rec = Rec {
            kind: k,
            before,
            after: self.state.stage(),
            fx: Vec::new(),
            rejoin_while_out: false,
            transition_out_of_turn: false,
        };
        for effect in out {
            match effect {
                Effect::CallDeactivate | Effect::Rebuild if !self.hand => {
                    rec.rejoin_while_out = true;
                }
                Effect::CallActivate(_) if self.real_active => rec.transition_out_of_turn = true,
                Effect::CallDeactivate if !self.real_active => rec.transition_out_of_turn = true,
                _ => {}
            }
            rec.fx.push(tag(&effect));
            all.push(tag(&effect));
            q.push_back(effect);
        }
        self.trace.push(rec);
    }

    fn perform(&mut self, effect: &Effect, q: &mut VecDeque<Effect>, all: &mut Vec<Fx>) -> Reply {
        match effect {
            Effect::CallActivate(_) => {
                for r in std::mem::take(&mut self.during_activate) {
                    self.feed(Event::Request(r), q, all);
                }
                let refuse = match self.refuse {
                    Refuse::Never => false,
                    Refuse::Always => true,
                    Refuse::Choice => self.pick(3) == 0,
                };
                if refuse {
                    Reply::Refused(err())
                } else {
                    self.real_active = true;
                    Reply::Activated
                }
            }
            Effect::CallDeactivate => {
                if self.refuse == Refuse::Choice && self.pick(4) == 0 {
                    Reply::Stuck(err())
                } else {
                    self.real_active = false;
                    Reply::Deactivated
                }
            }
            Effect::Rebuild => {
                if self.refuse == Refuse::Always
                    || (self.refuse == Refuse::Choice && self.pick(3) == 0)
                {
                    Reply::Refused(err())
                } else {
                    Reply::Rebuilt { restored: Ok(()) }
                }
            }
            Effect::Recall => {
                if self.hand {
                    Reply::Returned
                } else if self.rt == Rt::Local {
                    self.hand = true;
                    Reply::Returned
                } else {
                    self.returning = true;
                    Reply::Lent
                }
            }
            Effect::Lend => {
                self.hand = false;
                Reply::Done
            }
            Effect::ReadLatency if self.real_active => {
                Reply::Latency(Samples::new(self.real_latency))
            }
            Effect::ReadTail if self.real_active => Reply::Tail(PluginTail::Unknown),
            Effect::ReadLayout if self.real_active => Reply::Layout(stereo()),
            Effect::ReadLatency | Effect::ReadTail | Effect::ReadLayout => Reply::NotActive,
            _ => Reply::Done,
        }
    }

    /// Steps `event`, then performs every effect it leads to. Returns them all.
    fn send(&mut self, event: Event) -> Vec<Fx> {
        let (mut q, mut all) = (VecDeque::new(), Vec::new());
        self.feed(event, &mut q, &mut all);
        while let Some(effect) = q.pop_front() {
            while !self.inject.is_empty() && self.pick(2) == 0 {
                let op = self.inject.remove(0);
                self.feed(Event::Op(op), &mut q, &mut all);
            }
            let r = self.perform(&effect, &mut q, &mut all);
            self.feed(Event::Replied(r), &mut q, &mut all);
        }
        all
    }

    fn op(&mut self, op: HostOp) -> Vec<Fx> {
        self.send(Event::Op(op))
    }

    /// The realtime thread hands the Processor back.
    fn return_processor(&mut self) -> Vec<Fx> {
        assert!(self.returning);
        self.hand = true;
        self.returning = false;
        self.send(Event::Returned)
    }
}

// ------------------------------------------------------------- strategies

#[derive(Debug, Clone)]
enum Ev {
    Activate(u8),
    Reconfigure(u8),
    Deactivate,
    Req(Request),
    Tick,
    Return,
    Died,
}

fn scope() -> impl Strategy<Value = RescanScope> {
    prop_oneof![
        Just(RescanScope::Values),
        Just(RescanScope::Text),
        Just(RescanScope::Info),
        Just(RescanScope::All),
    ]
}

fn request() -> impl Strategy<Value = Request> {
    prop_oneof![
        Just(Request::Restart),
        Just(Request::IoChanged),
        scope().prop_map(Request::RescanParams),
        Just(Request::LatencyChanged),
        Just(Request::TailChanged),
        Just(Request::Reload),
        Just(Request::Callback),
        Just(Request::MidiCcAssignmentChanged),
        Just(Request::NoteExpressionChanged),
        Just(Request::Transport(TransportRequest::TogglePlay)),
        (0u32..6).prop_map(|i| Request::Timer(TimerId(i))),
        (0i32..6).prop_map(|i| Request::Fd(FdId::new(i).unwrap())),
    ]
}

fn ev() -> impl Strategy<Value = Ev> {
    prop_oneof![
        2 => any::<u8>().prop_map(Ev::Activate),
        2 => any::<u8>().prop_map(Ev::Reconfigure),
        2 => Just(Ev::Deactivate),
        6 => request().prop_map(Ev::Req),
        6 => Just(Ev::Tick),
        4 => Just(Ev::Return),
        1 => Just(Ev::Died),
    ]
}

fn rules() -> impl Strategy<Value = RestartRules> {
    (any::<bool>(), any::<bool>()).prop_map(|(io, latency)| RestartRules { io, latency })
}

/// A strategy for a value that is not `Clone`.
fn lazy<T: std::fmt::Debug>(make: fn() -> T) -> impl Strategy<Value = T> {
    Just(()).prop_map(move |()| make())
}

fn any_reply() -> impl Strategy<Value = Reply> {
    prop_oneof![
        lazy(|| Reply::Activated),
        lazy(|| Reply::Refused(err())),
        lazy(|| Reply::Deactivated),
        lazy(|| Reply::Rebuilt { restored: Ok(()) }),
        (0usize..3).prop_map(|n| Reply::Latency(Samples::new(n))),
        lazy(|| Reply::Tail(PluginTail::Unknown)),
        lazy(|| Reply::Tail(PluginTail::None)),
        lazy(|| Reply::Layout(stereo())),
        lazy(|| Reply::Layout(Layout::default())),
        lazy(|| Reply::NotActive),
        lazy(|| Reply::Returned),
        lazy(|| Reply::Lent),
        lazy(|| Reply::Stuck(err())),
        lazy(|| Reply::Done),
    ]
}

/// Any event at all, whether or not an effect is waiting for it.
fn any_event() -> impl Strategy<Value = Event> {
    prop_oneof![
        any::<u8>().prop_map(|n| Event::Op(HostOp::Activate(prep(n)))),
        any::<u8>().prop_map(|n| Event::Op(HostOp::Reconfigure(prep(n)))),
        lazy(|| Event::Op(HostOp::Deactivate)),
        request().prop_map(Event::Request),
        lazy(|| Event::Tick),
        any_reply().prop_map(Event::Replied),
        lazy(|| Event::Returned),
        lazy(|| Event::Died),
    ]
}

fn shell_run(shell: &mut Shell, events: &[Ev]) {
    for e in events {
        match e {
            Ev::Activate(n) => drop(shell.op(HostOp::Activate(prep(*n)))),
            Ev::Reconfigure(n) => drop(shell.op(HostOp::Reconfigure(prep(*n)))),
            Ev::Deactivate => drop(shell.op(HostOp::Deactivate)),
            Ev::Req(r) => drop(shell.send(Event::Request(*r))),
            Ev::Tick => drop(shell.send(Event::Tick)),
            Ev::Return => {
                if shell.returning {
                    shell.return_processor();
                }
            }
            Ev::Died => drop(shell.send(Event::Died)),
        }
    }
}

fn count(fx: &[Fx], f: impl Fn(&Fx) -> bool) -> usize {
    fx.iter().filter(|x| f(x)).count()
}

// ---------------------------------------------------------------- properties

proptest! {
    /// Every `Replied(Activated)` is answered, in the same step, by the three
    /// reads and then one `Lend`, whatever preceded it and whatever the format.
    #[test]
    fn an_activation_is_answered_by_the_reads_then_one_lend(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
        local in any::<bool>(),
    ) {
        let rt = if local { Rt::Local } else { Rt::Thread };
        let mut shell = Shell::new(rules, Refuse::Choice, rt, choices);
        shell_run(&mut shell, &events);
        for rec in shell.trace.iter().filter(|r| r.kind == Kind::Activated) {
            prop_assert_eq!(&rec.fx[..4], &[Fx::Latency, Fx::Tail, Fx::Layout, Fx::Lend]);
            prop_assert_eq!(count(&rec.fx, |f| *f == Fx::Lend), 1);
        }
    }

    /// The plugin renders only in `Active`: a `Lend` is emitted only into it,
    /// and `Refused` is left only by emitting an activation (or by dying).
    #[test]
    fn only_an_active_plugin_renders(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
        local in any::<bool>(),
    ) {
        let rt = if local { Rt::Local } else { Rt::Thread };
        let mut shell = Shell::new(rules, Refuse::Choice, rt, choices);
        shell_run(&mut shell, &events);
        for rec in &shell.trace {
            if rec.fx.contains(&Fx::Lend) {
                prop_assert_eq!(rec.after, Stage::Active);
            }
            if matches!(rec.before, Stage::Refused { .. })
                && !matches!(rec.after, Stage::Refused { .. })
            {
                let activates = rec.fx.iter().any(|f| matches!(f, Fx::Activate(_)));
                prop_assert!(activates || rec.kind == Kind::Died);
            }
        }
    }

    /// From `Refused`, nothing but an activation reaches an effect that
    /// renders, whatever the events (replies included).
    #[test]
    fn a_refused_plugin_stays_silent_until_asked_to_activate(
        rules in rules(),
        events in prop::collection::vec(any_event(), 0..32),
        after_rollback in any::<bool>(),
    ) {
        let mut s = if after_rollback {
            let (s, _) = refused_reconfigure(rules, prep(1), prep(2));
            reply(s, Reply::Refused(err())).0
        } else {
            refused(rules, prep(0))
        };
        let stage = s.stage();
        let is_refused = matches!(stage, Stage::Refused { .. });
        prop_assert!(is_refused);
        for e in events {
            if matches!(e, Event::Op(HostOp::Activate(_) | HostOp::Reconfigure(_))) {
                continue;
            }
            let (next, fx) = go(s, e);
            s = next;
            prop_assert!(!fx.contains(&Fx::Lend));
            let silent = matches!(s.stage(), Stage::Refused { .. } | Stage::Poisoned);
            prop_assert!(silent);
            prop_assert!(!s.renders());
        }
    }

    /// `Poisoned` absorbs: nothing after `Died` emits an effect or changes it.
    #[test]
    fn a_poisoned_plugin_absorbs_every_event(
        rules in rules(),
        before in prop::collection::vec(ev(), 0..24),
        after in prop::collection::vec(any_event(), 1..24),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Thread, vec![1, 0, 2]);
        shell_run(&mut shell, &before);
        let (mut s, _) = go(std::mem::replace(&mut shell.state, State::new(REST)), Event::Died);
        prop_assert_eq!(s.stage(), Stage::Poisoned);
        prop_assert!(!s.is_busy());
        for e in after {
            let (next, fx) = go(s, e);
            s = next;
            prop_assert!(fx.is_empty());
            prop_assert_eq!(s.stage(), Stage::Poisoned);
        }
    }

    /// A latency or tail change raised while the plugin activates is answered
    /// by the reads after it: the next tick restarts nothing.
    #[test]
    fn a_change_raised_inside_an_activation_does_not_restart_it(
        io in any::<bool>(),
        inside in prop::collection::vec(
            prop_oneof![Just(Request::LatencyChanged), Just(Request::TailChanged)], 1..6),
        n in any::<u8>(),
    ) {
        let rules = RestartRules { io, latency: true };
        let mut shell = Shell::easy(rules);
        shell.op(HostOp::Activate(prep(n)));
        shell.during_activate = inside;
        let fx = shell.op(HostOp::Reconfigure(prep(n.wrapping_add(1))));
        prop_assert!(fx.contains(&Fx::Lend));
        let tick = shell.send(Event::Tick);
        prop_assert_eq!(count(&tick, |f| matches!(f, Fx::Deactivate | Fx::Activate(_))), 0);
    }

    /// A quiet state's tick plans nothing, after any sequence of events: what
    /// lets a shell skip the tick of an idle pass.
    #[test]
    fn a_quiet_state_ticks_to_nothing(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Local, choices);
        for event in &events {
            shell_run(&mut shell, std::slice::from_ref(event));
            if shell.state.is_quiet() {
                let (_, fx) = go(shell.state.clone(), Event::Tick);
                prop_assert!(fx.is_empty(), "{fx:?}");
            }
        }
    }

    /// Activations over a sequence are at most one per activating op, one per
    /// tick that planned a restart and one per refusal that rolled back.
    #[test]
    fn activations_are_bounded_by_ops_and_planned_restarts(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..48),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Local, choices);
        shell_run(&mut shell, &events);
        let activations: usize = shell.trace.iter()
            .map(|r| count(&r.fx, |f| matches!(f, Fx::Activate(_)))).sum();
        let ops = shell.trace.iter()
            .filter(|r| matches!(r.kind, Kind::ActivateOp | Kind::ReconfigureOp)).count();
        let planned = shell.trace.iter()
            .filter(|r| matches!(r.kind, Kind::Tick | Kind::Reply) && r.fx.contains(&Fx::Recall))
            .count();
        // A rollback is the activation a refusal answers with, before it tells
        // the host (a replayed op follows the notice). One is allowed for a
        // refused activation that is not itself a rollback.
        let mut rollbacks = 0;
        let mut in_flight_is_rollback = false;
        for r in &shell.trace {
            if !r.fx.iter().any(|f| matches!(f, Fx::Activate(_))) {
                continue;
            }
            let rollback = r.kind == Kind::Refused && !r.fx.contains(&Fx::RefusedNotice);
            prop_assert!(!(rollback && in_flight_is_rollback), "a refused rollback rolls back again");
            rollbacks += usize::from(rollback);
            in_flight_is_rollback = rollback;
        }
        prop_assert!(activations <= ops + planned + rollbacks);
    }

    /// A reload subsumes every other request: one rebuild, one rejoin, one
    /// activation, one notice, however many finer requests came with it.
    #[test]
    fn a_reload_subsumes_the_finer_requests(
        rules in rules(),
        others in prop::collection::vec(request(), 0..8),
        n in any::<u8>(),
    ) {
        let mut shell = Shell::easy(rules);
        shell.op(HostOp::Activate(prep(n)));
        shell.send(Event::Request(Request::Reload));
        for r in others {
            shell.send(Event::Request(r));
        }
        let fx = shell.send(Event::Tick);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Rebuild), 1);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Deactivate), 1);
        prop_assert_eq!(count(&fx, |f| matches!(f, Fx::Activate(_))), 1);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Reloaded), 1);
        while shell.state.has_backlog() {
            shell.send(Event::Tick);
        }
        prop_assert!(shell.send(Event::Tick).is_empty());
    }

    /// A rejoin never meets a Processor that is away: `CallDeactivate` and
    /// `Rebuild` come only after a `Recall` was answered `Returned` or
    /// followed by `Event::Returned`.
    #[test]
    fn a_rejoin_never_meets_a_processor_that_is_away(
        rules in rules(),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        events in prop::collection::vec(ev(), 0..64),
    ) {
        let mut shell = Shell::new(rules, Refuse::Choice, Rt::Thread, choices);
        shell_run(&mut shell, &events);
        for rec in &shell.trace {
            prop_assert!(!rec.rejoin_while_out);
            prop_assert!(!rec.transition_out_of_turn);
        }
    }

    /// A restart that ends in `Refused` notifies the refusal once and never a
    /// latency change, and renders nothing after.
    #[test]
    fn a_restart_that_ends_refused_says_so_once(
        rules in rules(),
        extra in prop::collection::vec(request(), 0..6),
        n in any::<u8>(),
    ) {
        let mut shell = Shell::easy(rules);
        shell.op(HostOp::Activate(prep(n)));
        shell.refuse = Refuse::Always;
        shell.real_latency = 99;
        shell.send(Event::Request(Request::Restart));
        shell.send(Event::Request(Request::LatencyChanged));
        for r in extra {
            if !matches!(r, Request::Callback) {
                shell.send(Event::Request(r));
            }
        }
        let fx = shell.send(Event::Tick);
        prop_assert_eq!(count(&fx, |f| *f == Fx::RefusedNotice), 1);
        prop_assert_eq!(count(&fx, |f| matches!(f, Fx::LatencyNotice(_))), 0);
        prop_assert_eq!(count(&fx, |f| *f == Fx::Lend), 0);
        prop_assert_eq!(count(&fx, |f| matches!(f, Fx::Latency | Fx::Tail | Fx::Layout)), 0);
        let stage = shell.state.stage();
        let refused = matches!(stage, Stage::Refused { .. });
        prop_assert!(refused);
    }

    /// `step` is total and bounded: no event in any state panics or emits more
    /// than `EFFECT_CAPACITY` effects.
    #[test]
    fn step_is_total_and_bounded(
        rules in rules(),
        n in any::<u8>(),
        timers in prop::collection::vec(0u32..40, 0..40),
        fds in prop::collection::vec(0i32..40, 0..40),
        events in prop::collection::vec(any_event(), 0..64),
    ) {
        let mut s = active(rules, prep(n));
        for t in timers {
            s = req(s, Request::Timer(TimerId(t)));
        }
        for f in fds {
            s = req(s, Request::Fd(FdId::new(f).unwrap()));
        }
        for e in std::iter::once(Event::Tick).chain(events) {
            let (next, out) = step(s, e);
            s = next;
            prop_assert!(out.len() <= EFFECT_CAPACITY);
        }
    }

    /// A parked plan runs once, with the latest `Prepare`: `Returned` emits
    /// the rest of it, a second `Returned` emits nothing.
    #[test]
    fn a_parked_plan_runs_once_with_the_last_prepare(
        rules in rules(),
        ns in prop::collection::vec(any::<u8>(), 1..6),
    ) {
        let mut shell = Shell::new(rules, Refuse::Never, Rt::Thread, vec![1]);
        shell.op(HostOp::Activate(prep(0)));
        for n in &ns {
            shell.op(HostOp::Reconfigure(prep(*n)));
        }
        prop_assert!(shell.state.is_parked());
        let fx = shell.return_processor();
        prop_assert_eq!(count(&fx, |f| *f == Fx::Deactivate), 1);
        let acts: Vec<Fx> = fx.iter().copied().filter(|f| matches!(f, Fx::Activate(_))).collect();
        prop_assert_eq!(acts, vec![Fx::Activate(prep(*ns.last().unwrap()))]);
        let (_, none) = go(shell.state.clone(), Event::Returned);
        prop_assert!(none.is_empty());
    }
}

// -------------------------------------------------------------- state tables

#[test]
fn rule_1_activate_and_reconfigure() {
    let p = prep(1);
    let q = prep(2);
    // Loaded and Refused: Activate and Reconfigure both activate.
    for op in [HostOp::Activate(p), HostOp::Reconfigure(p)] {
        let (_, fx) = go(State::new(REST), Event::Op(op));
        assert_eq!(fx, [Fx::Activate(p)]);
        let (_, fx) = go(refused(REST, q), Event::Op(op));
        assert_eq!(fx, [Fx::Activate(p)]);
    }
    // Active: Activate does nothing, Reconfigure recalls first and then
    // deactivates and activates as the replies come.
    let s = active(REST, p);
    let (s, fx) = go(s, Event::Op(HostOp::Activate(q)));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(q)));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
    assert_eq!(s.applied(), Some(p), "accepted only when the activation is");
    let (s, _) = reply(s, Reply::Activated);
    assert_eq!(s.applied(), Some(q));
    // Poisoned: nothing.
    let (dead, _) = go(State::new(REST), Event::Died);
    for op in [
        HostOp::Activate(p),
        HostOp::Reconfigure(p),
        HostOp::Deactivate,
    ] {
        assert!(go(dead.clone(), Event::Op(op)).1.is_empty());
    }
    // Deactivate on an active plugin recalls, rejoins and stays loaded.
    let (s, fx) = go(active(REST, p), Event::Op(HostOp::Deactivate));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    assert_eq!(s.stage(), Stage::Loaded);
    // On a loaded or refused one it does nothing.
    assert!(
        go(State::new(REST), Event::Op(HostOp::Deactivate))
            .1
            .is_empty()
    );
    let (s, fx) = go(refused(REST, p), Event::Op(HostOp::Deactivate));
    assert!(fx.is_empty());
    assert!(matches!(s.stage(), Stage::Refused { .. }));
}

#[test]
fn rule_2_a_change_raised_before_the_deactivation_is_answered_by_the_reads() {
    let p = prep(1);
    let s = active(RESTARTS, p);
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(p)));
    let (s, _) = reply(s, Reply::Returned);
    let s = req(s, Request::LatencyChanged);
    let s = req(s, Request::TailChanged);
    let (s, _) = reply(s, Reply::Deactivated);
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(fx, [Fx::Latency, Fx::Tail, Fx::Layout, Fx::Lend]);
    assert!(go(s, Event::Tick).1.is_empty());
}

#[test]
fn rule_3_a_refusal_reads_nothing_and_a_later_activation_retries() {
    let p = prep(3);
    let (s, _) = go(State::new(REST), Event::Op(HostOp::Activate(p)));
    let (s, out) = step(s, Event::Replied(Reply::Refused(err())));
    assert_eq!(out.iter().map(tag).collect::<Vec<_>>(), [Fx::RefusedNotice]);
    assert!(
        matches!(
            out.last(),
            Some(Effect::Notify(Notice::Refused {
                recovery: Recovery::Inactive,
                ..
            }))
        ),
        "a first activation has nothing to go back to"
    );
    assert_eq!(s.applied(), None);
    assert_eq!(s.stage(), Stage::Refused { rate: p.rate() });
    assert!(!s.renders());
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(prep(4))));
    assert_eq!(fx, [Fx::Activate(prep(4))]);
    let (s, _) = reply(s, Reply::Activated);
    assert!(s.renders());
}

/// The plugin accepted `p`; asked for `q`, it refuses, and the effects that
/// follow are returned with the state.
fn refused_reconfigure(rules: RestartRules, p: Prepare, q: Prepare) -> (State, Effects) {
    let s = active(rules, p);
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = reply(s, Reply::Returned);
    let (s, _) = reply(s, Reply::Deactivated);
    step(s, Event::Replied(Reply::Refused(err())))
}

/// **A refused reconfigure activates again at what the plugin accepted, and
/// the refusal says the plugin runs there.**
#[test]
fn rule_3_a_refused_reconfigure_rolls_back_to_the_accepted_setting_and_says_so() {
    let (p, q) = (prep(1), prep(2));
    let (s, out) = refused_reconfigure(REST, p, q);
    assert_eq!(out.iter().map(tag).collect::<Vec<_>>(), [Fx::Activate(p)]);
    assert!(!s.renders(), "the rollback is in flight");
    assert_eq!(s.applied(), Some(p));
    let (s, out) = step(s, Event::Replied(Reply::Activated));
    assert_eq!(
        out.iter().map(tag).collect::<Vec<_>>(),
        [
            Fx::Latency,
            Fx::Tail,
            Fx::Layout,
            Fx::Lend,
            Fx::RefusedNotice
        ]
    );
    assert!(matches!(
        out.last(),
        Some(Effect::Notify(Notice::Refused {
            rate,
            recovery: Recovery::RolledBack { running },
            ..
        })) if *rate == q.rate() && *running == p
    ));
    assert!(s.renders());
    assert_eq!(s.applied(), Some(p), "the refused setting is not accepted");
    let (_, fx) = reply(s, Reply::Layout(stereo()));
    assert_eq!(
        fx,
        [Fx::IoNotice],
        "a revived plugin tells its layout again"
    );
}

/// **A refused rollback leaves the plugin not active, and the refusal says
/// so.**
#[test]
fn rule_3_a_refused_rollback_leaves_the_plugin_inactive_and_says_so() {
    let (p, q) = (prep(1), prep(2));
    let (s, _) = refused_reconfigure(REST, p, q);
    let (s, out) = step(s, Event::Replied(Reply::Refused(err())));
    assert_eq!(out.iter().map(tag).collect::<Vec<_>>(), [Fx::RefusedNotice]);
    assert!(matches!(
        out.last(),
        Some(Effect::Notify(Notice::Refused {
            rate,
            recovery: Recovery::RollbackRefused { .. },
            ..
        })) if *rate == q.rate()
    ));
    assert_eq!(s.stage(), Stage::Refused { rate: q.rate() });
    assert!(!s.renders());
    assert_eq!(s.applied(), Some(p));
}

/// **A restart refused at the setting the plugin already runs at has nothing
/// to go back to: it is told as inactive and nothing is activated again.**
#[test]
fn rule_3_a_restart_refused_at_the_running_setting_is_inactive_and_does_not_loop() {
    let s = req(active(RESTARTS, prep(1)), Request::Restart);
    let (s, _) = go(s, Event::Tick);
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(prep(1))]);
    let (s, out) = step(s, Event::Replied(Reply::Refused(err())));
    assert_eq!(out.iter().map(tag).collect::<Vec<_>>(), [Fx::RefusedNotice]);
    assert!(matches!(
        out.last(),
        Some(Effect::Notify(Notice::Refused {
            recovery: Recovery::Inactive,
            ..
        }))
    ));
    assert_eq!(
        s.stage(),
        Stage::Refused {
            rate: prep(1).rate()
        }
    );
}

/// **A plugin lost while its refusal is being rolled back still tells the
/// refusal, as inactive, and is poisoned.**
#[test]
fn rule_3_a_plugin_lost_during_a_rollback_tells_the_refusal_as_inactive() {
    let (s, _) = refused_reconfigure(REST, prep(1), prep(2));
    let (s, out) = step(s, Event::Died);
    assert_eq!(out.iter().map(tag).collect::<Vec<_>>(), [Fx::RefusedNotice]);
    assert!(matches!(
        out.last(),
        Some(Effect::Notify(Notice::Refused {
            recovery: Recovery::Inactive,
            ..
        }))
    ));
    assert_eq!(s.stage(), Stage::Poisoned);
}

/// **A retuning Processor is told its rate, not restarted, and its answer is
/// what the plugin has**: the rate, block ceiling and callback size are not
/// restarts for a Processor that takes them live, a taken rate applies what the
/// retune takes, and a difference in the width still needs an activation.
#[test]
fn rule_3_a_taken_retune_applies_what_it_takes_and_a_difference_in_another_field_remains() {
    let p = prep(1);
    let live = p
        .with_rate(SampleRate::SR_96K)
        .with_quantum(MaxBlock::of(Samples::new(32)));
    let mut s = active(REST, p);
    s.want(live);
    assert_eq!(
        s.behind(false, RenderModeSupport::Unsupported),
        Behind::Restart
    );
    assert_eq!(
        s.behind(true, RenderModeSupport::Unsupported),
        Behind::Retune
    );
    let (s, _) = go(s, Event::Answered(Told::RateTook(SampleRate::SR_96K)));
    assert_eq!(s.applied(), Some(live));

    let other_width = live.with_format(match p.format() {
        FloatWidth::F32 => FloatWidth::F64,
        FloatWidth::F64 => FloatWidth::F32,
    });
    let mut s = s;
    s.want(other_width.with_rate(SampleRate::SR_48K));
    let (s, _) = go(s, Event::Answered(Told::RateTook(SampleRate::SR_48K)));
    assert_eq!(s.applied().map(|a| a.rate()), Some(SampleRate::SR_48K));
    assert_eq!(
        s.applied().map(|a| a.format()),
        Some(p.format()),
        "not taken"
    );
    assert_eq!(
        s.behind(true, RenderModeSupport::Unsupported),
        Behind::Restart,
        "the width still needs an activation"
    );
}

/// **A retune is never dropped**: a Processor that retunes live and was not in
/// hand when the host wanted another rate is brought back, told, lent again, and
/// the rate is in flight, not asked again, until the plugin answers.
#[test]
fn rule_3_a_wanted_rate_the_processor_has_not_been_told_is_told_by_a_plan() {
    let p = prep(1);
    let mut s = active(REST, p);
    s.want(p.with_rate(SampleRate::SR_96K));
    assert_eq!(
        s.behind(true, RenderModeSupport::Unsupported),
        Behind::Retune
    );
    let (s, fx) = go(s, Event::Op(HostOp::Retune));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Lent);
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Retune(SampleRate::SR_96K), Fx::Lend]);
    assert_eq!(
        s.behind(true, RenderModeSupport::Unsupported),
        Behind::No,
        "told, and not told again while the answer is out"
    );
    let (s, _) = go(s, Event::Answered(Told::RateTook(SampleRate::SR_96K)));
    assert_eq!(s.applied().map(|a| a.rate()), Some(SampleRate::SR_96K));
    assert_eq!(s.behind(true, RenderModeSupport::Unsupported), Behind::No);
}

/// **A callback size alone restarts nothing, and no other field escapes**: a
/// plugin whose Processor does not retune is behind in a rate, a block ceiling
/// or a width, and not in a quantum or a smaller block ceiling.
#[test]
fn rule_3_only_the_callback_size_is_not_an_activation() {
    let p = prep(1);
    let mut s = active(REST, p);
    s.want(p.with_quantum(MaxBlock::of(Samples::new(32))));
    assert_eq!(s.behind(false, RenderModeSupport::Unsupported), Behind::No);
    s.want(p.with_max_block(MaxBlock::of(Samples::new(32))));
    assert_eq!(
        s.behind(false, RenderModeSupport::Unsupported),
        Behind::No,
        "a block ceiling at or below the one it was told restarts nothing"
    );
    s.want(p.with_max_block(MaxBlock::of(Samples::new(512))));
    assert_eq!(
        s.behind(false, RenderModeSupport::Unsupported),
        Behind::Restart
    );
}

/// **A refused retune gives the rate up and tells the plugin's standing**: the
/// plugin has what it had, the host wants it again, and the notice names the
/// refused rate with the rate the plugin runs at.
#[test]
fn rule_3_a_refused_retune_rolls_the_wanted_rate_back_and_tells_the_refusal() {
    let p = prep(1);
    let mut s = active(REST, p);
    s.want(p.with_rate(SampleRate::SR_96K));
    let cause = RateRefusalCause::RolledBack {
        running: p.rate(),
        reason: "no".into(),
    };
    let refusal = RateRefusal::new(SampleRate::SR_96K, cause);
    let (s, out) = step(s, Event::Answered(Told::RateRefused(refusal)));
    assert_eq!(s.applied(), Some(p));
    assert_eq!(s.wanted(), Some(p));
    assert!(matches!(
        out.as_slice(),
        [Effect::Notify(Notice::Refused {
            recovery: Recovery::RolledBack { running },
            ..
        })] if *running == p
    ));
}

/// **A plugin's own restart after a refused rate activates with the rate it
/// accepted, not the one it refused.**
#[test]
fn rule_3_a_restart_after_a_refused_rate_uses_the_accepted_rate() {
    let (p, q) = (prep(1), prep(2));
    let (s, _) = refused_reconfigure(RESTARTS, p, q);
    let (s, _) = step(s, Event::Replied(Reply::Activated));
    let s = req(s, Request::Restart);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Recall]);
    let (s, _) = reply(s, Reply::Returned);
    let (_, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(p)]);
}

#[test]
fn rule_4_a_figure_is_notified_when_it_moved_or_was_announced() {
    // The first read after load only records the figure.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(prep(1))));
    let (s, _) = reply(s, Reply::Activated);
    let (s, fx) = reply(s, Reply::Latency(Samples::new(7)));
    assert!(fx.is_empty());
    assert_eq!(s.latency(), Some(Samples::new(7)));
    let (s, fx) = reply(s, Reply::Tail(PluginTail::Unbounded));
    assert!(fx.is_empty());
    assert_eq!(s.tail(), Some(PluginTail::Unbounded));
    let s = active(RESTARTS, prep(1));
    // Same figure, not announced: silent.
    let (s, fx) = reply(s, Reply::Latency(Samples::new(0)));
    assert!(fx.is_empty());
    // Moved: notified.
    let (_, fx) = reply(s, Reply::Latency(Samples::new(32)));
    assert_eq!(fx, [Fx::LatencyNotice(32)]);
    // Announced by a re-read format, same figure: notified.
    let s = active(REST, prep(1));
    let s = req(s, Request::LatencyChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Latency]);
    let (_, fx) = reply(s, Reply::Latency(Samples::new(0)));
    assert_eq!(fx, [Fx::LatencyNotice(0)]);
    // Tail likewise.
    let s = active(REST, prep(1));
    let (s, fx) = reply(s, Reply::Tail(PluginTail::Unknown));
    assert!(fx.is_empty());
    let (s, fx) = reply(s, Reply::Tail(PluginTail::None));
    assert_eq!(fx, [Fx::TailNotice]);
    let s = req(s, Request::TailChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Tail]);
    let (_, fx) = reply(s, Reply::Tail(PluginTail::None));
    assert_eq!(fx, [Fx::TailNotice]);
}

#[test]
fn rule_4_a_layout_is_notified_when_it_differs_or_the_stage_changed() {
    let s = active(REST, prep(1));
    let (s, fx) = reply(s, Reply::Layout(stereo()));
    assert!(fx.is_empty());
    let (_, fx) = reply(s, Reply::Layout(Layout::default()));
    assert_eq!(fx, [Fx::IoNotice]);
    // A plugin revived from a refusal: the same layout still tells the host.
    let mut shell = Shell::easy(REST);
    shell.op(HostOp::Activate(prep(1)));
    shell.refuse = Refuse::Always;
    shell.op(HostOp::Reconfigure(prep(2)));
    assert!(matches!(shell.state.stage(), Stage::Refused { .. }));
    shell.refuse = Refuse::Never;
    let fx = shell.op(HostOp::Activate(prep(2)));
    assert!(fx.contains(&Fx::IoNotice));
}

#[test]
fn rule_5_requests_set_bits_and_a_tick_plans() {
    let p = prep(1);
    for (request, restarts) in [
        (Request::Restart, true),
        (Request::Reload, true),
        (Request::RescanParams(RescanScope::All), true),
        (Request::IoChanged, true),
        (Request::LatencyChanged, true),
    ] {
        let s = req(active(RESTARTS, p), request);
        let (_, fx) = go(s, Event::Tick);
        assert_eq!(fx.first() == Some(&Fx::Recall), restarts, "{request:?}");
    }
    // A format that re-reads: the same requests restart nothing.
    let s = req(active(REST, p), Request::IoChanged);
    assert_eq!(go(s, Event::Tick).1, [Fx::Layout]);
    let s = req(active(REST, p), Request::LatencyChanged);
    assert_eq!(go(s, Event::Tick).1, [Fx::Latency]);
    let s = req(active(REST, p), Request::TailChanged);
    assert_eq!(go(s, Event::Tick).1, [Fx::Tail]);
    // Several collapse into one recall.
    let mut s = active(RESTARTS, p);
    for r in [
        Request::Restart,
        Request::IoChanged,
        Request::LatencyChanged,
    ] {
        s = req(s, r);
    }
    assert_eq!(go(s, Event::Tick).1, [Fx::Recall]);
}

#[test]
fn rule_5_a_callback_runs_first_because_it_may_ask_for_the_rest() {
    let s = req(active(RESTARTS, prep(1)), Request::Callback);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Main]);
    // The callback asks for a restart; the plan follows its reply.
    let s = req(s, Request::Restart);
    let (_, fx) = reply(s, Reply::Done);
    assert_eq!(fx, [Fx::Recall]);
    // A callback that asks for another callback does not loop.
    let s = req(active(RESTARTS, prep(1)), Request::Callback);
    let (s, _) = go(s, Event::Tick);
    let s = req(s, Request::Callback);
    let (_, fx) = reply(s, Reply::Done);
    assert!(fx.is_empty());
}

#[test]
fn rule_5_a_rescan_restarts_only_when_it_must() {
    let p = prep(1);
    for scope in [RescanScope::Values, RescanScope::Text] {
        let s = req(active(RESTARTS, p), Request::RescanParams(scope));
        assert_eq!(go(s, Event::Tick).1, [Fx::ParamValues]);
    }
    let s = req(
        active(RESTARTS, p),
        Request::RescanParams(RescanScope::Info),
    );
    assert_eq!(
        go(s, Event::Tick).1,
        [Fx::Params(RescanScope::Info), Fx::ParamTitles]
    );
    // `All` restarts and reads after the activation.
    let s = req(active(RESTARTS, p), Request::RescanParams(RescanScope::All));
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Recall]);
    let (s, _) = reply(s, Reply::Returned);
    let (s, _) = reply(s, Reply::Deactivated);
    let (_, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx,
        [
            Fx::Latency,
            Fx::Tail,
            Fx::Layout,
            Fx::Lend,
            Fx::Params(RescanScope::All),
            Fx::ParamTitles,
            Fx::ParamValues
        ]
    );
}

#[test]
fn rule_6_a_request_during_an_activation_is_kept_except_latency_and_tail() {
    let p = prep(1);
    let s = active(RESTARTS, p);
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(p)));
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(p)]);
    let s = req(s, Request::LatencyChanged);
    let s = req(s, Request::TailChanged);
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
    // Anything else raised inside the activation is acted on after it.
    let (s, _) = go(active(RESTARTS, p), Event::Op(HostOp::Reconfigure(p)));
    let (s, _) = reply(s, Reply::Returned);
    let (s, _) = reply(s, Reply::Deactivated);
    let s = req(s, Request::IoChanged);
    let (s, _) = reply(s, Reply::Activated);
    assert_eq!(go(s, Event::Tick).1, [Fx::Recall]);
}

#[test]
fn rule_7_a_request_on_a_plugin_that_is_not_running_changes_nothing() {
    // A loaded plugin answers a parameter or port rescan
    // (`rule_7_a_rescan_on_a_loaded_plugin_is_answered`); the rest it ignores.
    let all = [
        Request::Restart,
        Request::IoChanged,
        Request::LatencyChanged,
        Request::TailChanged,
        Request::Reload,
        Request::RescanParams(RescanScope::All),
        Request::MidiCcAssignmentChanged,
        Request::NoteExpressionChanged,
    ];
    let loaded_all = [
        Request::Restart,
        Request::LatencyChanged,
        Request::TailChanged,
        Request::Reload,
        Request::MidiCcAssignmentChanged,
        Request::NoteExpressionChanged,
    ];
    let (poisoned, _) = go(State::new(REST), Event::Died);
    for (s, name) in [
        (State::new(RESTARTS), "loaded"),
        (refused(RESTARTS, prep(1)), "refused"),
        (poisoned, "poisoned"),
    ] {
        let mut s = s;
        for r in if name == "loaded" {
            &loaded_all[..]
        } else {
            &all[..]
        } {
            s = req(s, *r);
        }
        let (s, fx) = go(s, Event::Tick);
        assert!(fx.is_empty(), "{name}");
        if name != "poisoned" {
            // Nothing was kept for the activation that follows.
            let (s, _) = go(s, Event::Op(HostOp::Activate(prep(1))));
            let (s, _) = reply(s, Reply::Activated);
            assert!(go(s, Event::Tick).1.is_empty(), "{name} then active");
        }
    }
    // A loaded plugin still runs its callback, timers and descriptors.
    let mut s = State::new(RESTARTS);
    s = req(s, Request::Timer(TimerId(4)));
    s = req(s, Request::Fd(FdId::new(5).unwrap()));
    s = req(s, Request::Callback);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Main]);
    let (_, fx) = reply(s, Reply::Done);
    assert_eq!(fx, [Fx::Timer(4), Fx::Fd(5)]);
}

/// `params.h`: `params.rescan` is legal whenever the plugin is loaded, and a
/// plugin that changes its parameters after `state.load` while not yet active
/// must have the host read them again. A loaded plugin answers each rescan with
/// the read and the notices, restarts nothing, and leaves nothing pending to be
/// misread as a restart once it activates.
#[test]
fn rule_7_a_rescan_on_a_loaded_plugin_is_answered() {
    let (s, fx) = go(
        req(
            State::new(RESTARTS),
            Request::RescanParams(RescanScope::All),
        ),
        Event::Tick,
    );
    assert_eq!(
        fx,
        [
            Fx::Params(RescanScope::All),
            Fx::ParamTitles,
            Fx::ParamValues
        ]
    );
    let (s, fx) = go(
        req(s, Request::RescanParams(RescanScope::Info)),
        Event::Tick,
    );
    assert_eq!(fx, [Fx::Params(RescanScope::Info), Fx::ParamTitles]);
    let (s, fx) = go(
        req(s, Request::RescanParams(RescanScope::Values)),
        Event::Tick,
    );
    assert_eq!(fx, [Fx::ParamValues]);
    let (s, fx) = go(
        req(s, Request::RescanParams(RescanScope::Text)),
        Event::Tick,
    );
    assert_eq!(fx, [Fx::ParamValues]);
    let (s, fx) = go(req(s, Request::IoChanged), Event::Tick);
    assert_eq!(fx, [Fx::IoNotice]);
    assert_eq!(s.stage(), Stage::Loaded);
    // Nothing is left for the activation to read as a restart.
    let (s, _) = go(s, Event::Op(HostOp::Activate(prep(1))));
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
}

#[test]
fn rule_8_dying_drops_a_parked_plan() {
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(prep(2))));
    let (s, _) = reply(s, Reply::Lent);
    assert!(s.is_parked());
    let (s, fx) = go(s, Event::Died);
    assert!(fx.is_empty());
    assert!(!s.is_busy());
    assert_eq!(s.stage(), Stage::Poisoned);
    assert!(go(s, Event::Returned).1.is_empty());
}

#[test]
fn rule_9_a_plan_parks_behind_a_lent_processor_and_runs_once() {
    let (p, q, r) = (prep(1), prep(2), prep(3));
    let s = active(RESTARTS, p);
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(q)));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Lent);
    assert!(fx.is_empty());
    assert!(s.is_parked());
    // A second reconfigure rewrites the parked prepare.
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(r)));
    assert!(fx.is_empty());
    // A request only sets its bit, and a tick runs only what needs no restart.
    let s = req(s, Request::Restart);
    let s = req(s, Request::Timer(TimerId(1)));
    let s = req(s, Request::TailChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Timer(1), Fx::Tail]);
    // The Processor is back: the tail runs.
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    // A second `Returned` before the reply emits nothing.
    let (s, fx) = go(s, Event::Returned);
    assert!(fx.is_empty());
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(r)]);
    // The restart raised meanwhile is answered by the plan itself.
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
}

#[test]
fn rule_9_a_deactivate_replaces_a_parked_plan() {
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(prep(2))));
    let (s, _) = reply(s, Reply::Lent);
    let (s, fx) = go(s, Event::Op(HostOp::Deactivate));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    assert_eq!(s.stage(), Stage::Loaded);
}

#[test]
fn rule_10_a_plugin_that_will_not_stop_stays_active_and_is_lent_again() {
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(prep(2))));
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Stuck(err()));
    assert_eq!(fx, [Fx::StuckNotice, Fx::Lend]);
    assert_eq!(s.stage(), Stage::Active);
    assert!(!s.is_busy());
    assert!(go(s, Event::Tick).1.is_empty());
}

#[test]
fn a_reply_that_answers_nothing_is_ignored() {
    let s = active(RESTARTS, prep(1));
    for r in [
        Reply::Deactivated,
        Reply::Rebuilt { restored: Ok(()) },
        Reply::Activated,
        Reply::Refused(err()),
        Reply::Stuck(err()),
        Reply::Returned,
        Reply::Lent,
    ] {
        let (next, fx) = reply(s.clone(), r);
        assert!(fx.is_empty());
        assert_eq!(next.stage(), Stage::Active);
        assert!(!next.is_busy());
    }
}

#[test]
fn a_request_raised_while_a_plan_waits_is_answered_by_the_plan() {
    let q = prep(2);
    // Parks behind a lent Processor, then takes `request` while it waits.
    let park = |request| {
        let s = active(RESTARTS, prep(1));
        let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
        let (s, _) = reply(s, Reply::Lent);
        let s = req(s, request);
        let (s, fx) = go(s, Event::Returned);
        assert_eq!(fx, [Fx::Deactivate]);
        s
    };
    let finish = |s: State| {
        let (s, _) = reply(s, Reply::Activated);
        assert!(go(s, Event::Tick).1.is_empty());
    };
    for request in [
        Request::Restart,
        Request::IoChanged,
        Request::LatencyChanged,
    ] {
        let (s, fx) = reply(park(request), Reply::Deactivated);
        assert_eq!(fx, [Fx::Activate(q)], "{request:?}");
        finish(s);
    }
    // A rescan of everything re-reads the parameters after the activation.
    let (s, fx) = reply(
        park(Request::RescanParams(RescanScope::All)),
        Reply::Deactivated,
    );
    assert_eq!(fx, [Fx::Activate(q)]);
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx[4..],
        [
            Fx::Params(RescanScope::All),
            Fx::ParamTitles,
            Fx::ParamValues
        ]
    );
    assert!(go(s, Event::Tick).1.is_empty());
    // A reload is not lost: the rebuild joins the plan.
    let (s, fx) = reply(park(Request::Reload), Reply::Deactivated);
    assert_eq!(fx, [Fx::Rebuild]);
    let (s, fx) = reply(s, Reply::Rebuilt { restored: Ok(()) });
    assert_eq!(fx, [Fx::Reloaded, Fx::Activate(q)]);
    finish(s);
}

/// **A rebuilt instance that did not take the saved state is told to the host
/// after the reload**, and one that did is not.
#[test]
fn a_rebuild_that_restored_nothing_is_told_after_the_reload() {
    let q = prep(2);
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = reply(s, Reply::Lent);
    let s = req(s, Request::Reload);
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Rebuild]);
    let refusal = PluginError::Other("setState refused".into());
    let (_, fx) = reply(
        s,
        Reply::Rebuilt {
            restored: Err(refusal),
        },
    );
    assert_eq!(
        fx,
        [Fx::Reloaded, Fx::StateNotRestoredNotice, Fx::Activate(q)]
    );
}

#[test]
fn an_activate_raised_while_a_plan_waits_is_the_latest_intent() {
    let (p, q) = (prep(1), prep(2));
    let s = active(RESTARTS, p);
    let (s, _) = go(s, Event::Op(HostOp::Deactivate));
    let (s, _) = reply(s, Reply::Lent);
    let (s, fx) = go(s, Event::Op(HostOp::Activate(q)));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
    let (s, _) = reply(s, Reply::Activated);
    assert_eq!(s.applied(), Some(q));
}

#[test]
fn an_op_raised_during_an_activation_runs_when_it_ends() {
    let (p, q) = (prep(1), prep(2));
    // The latest of several wins.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, fx) = go(s, Event::Op(HostOp::Reconfigure(q)));
    assert!(fx.is_empty());
    let (s, fx) = go(s, Event::Op(HostOp::Deactivate));
    assert!(fx.is_empty());
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx,
        [Fx::Latency, Fx::Tail, Fx::Layout, Fx::Lend, Fx::Recall]
    );
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    assert_eq!(s.stage(), Stage::Loaded);
    // A reconfigure runs the same way and lands on its prepare.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(fx.last(), Some(&Fx::Recall));
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
    let (s, _) = reply(s, Reply::Activated);
    assert_eq!(s.applied(), Some(q));
    // An activate raised during a refused activation retries it.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Activate(q)));
    let (_, fx) = reply(s, Reply::Refused(err()));
    assert_eq!(fx, [Fx::RefusedNotice, Fx::Activate(q)]);
}

#[test]
fn timers_and_descriptors_over_the_cap_wait_for_the_next_tick() {
    let mut s = State::new(REST);
    for i in 0..5 {
        s = req(s, Request::Timer(TimerId(i)));
    }
    for i in 0..4 {
        s = req(s, Request::Fd(FdId::new(i).unwrap()));
    }
    // A repeated id is one entry.
    s = req(s, Request::Timer(TimerId(4)));
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(
        fx,
        [
            Fx::Timer(0),
            Fx::Timer(1),
            Fx::Timer(2),
            Fx::Fd(0),
            Fx::Fd(1),
            Fx::Fd(2)
        ]
    );
    assert!(s.has_backlog());
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Timer(3), Fx::Timer(4), Fx::Fd(3)]);
    assert!(!s.has_backlog());
}

#[test]
fn the_busiest_tick_emits_exactly_the_documented_maximum() {
    let mut s = active(REST, prep(1));
    for i in 0..TIMERS_PER_TICK as u32 {
        s = req(s, Request::Timer(TimerId(i)));
    }
    for i in 0..FDS_PER_TICK as i32 {
        s = req(s, Request::Fd(FdId::new(i).unwrap()));
    }
    for r in [
        Request::LatencyChanged,
        Request::TailChanged,
        Request::IoChanged,
        Request::RescanParams(RescanScope::Values),
        Request::RescanParams(RescanScope::Info),
        Request::MidiCcAssignmentChanged,
        Request::NoteExpressionChanged,
    ] {
        s = req(s, r);
    }
    let (_, fx) = go(s, Event::Tick);
    assert_eq!(fx.len(), MAX_STEP_EFFECTS);
    assert_eq!(MAX_STEP_EFFECTS, EFFECT_CAPACITY);
}

#[test]
fn a_refusal_forgets_what_was_pending() {
    let p = prep(1);
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let s = req(s, Request::Timer(TimerId(1)));
    let s = req(s, Request::Fd(FdId::new(2).unwrap()));
    let s = req(s, Request::Callback);
    let (s, _) = reply(s, Reply::Refused(err()));
    let (s, _) = go(s, Event::Op(HostOp::Activate(p)));
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
}

#[test]
fn a_refused_rebuild_is_retried_before_the_activation() {
    let p = prep(1);
    let s = req(active(RESTARTS, p), Request::Reload);
    let (s, _) = go(s, Event::Tick);
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Rebuild]);
    let (s, fx) = reply(s, Reply::Refused(err()));
    assert_eq!(fx, [Fx::RefusedNotice]);
    assert!(matches!(s.stage(), Stage::Refused { .. }));
    let (s, fx) = go(s, Event::Op(HostOp::Activate(p)));
    assert_eq!(fx, [Fx::Rebuild]);
    let (s, fx) = reply(s, Reply::Rebuilt { restored: Ok(()) });
    assert_eq!(fx, [Fx::Reloaded, Fx::Activate(p)]);
    // Once rebuilt, a later refusal retries the activation alone.
    let (s, _) = reply(s, Reply::Refused(err()));
    let (_, fx) = go(s, Event::Op(HostOp::Activate(p)));
    assert_eq!(fx, [Fx::Activate(p)]);
}

#[test]
fn only_the_answer_to_the_main_thread_work_continues_the_tick() {
    let s = req(active(RESTARTS, prep(1)), Request::Callback);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::Main]);
    // The work is answered with something other than `Done`.
    let (s, fx) = reply(s, Reply::NotActive);
    assert!(fx.is_empty());
    let s = req(s, Request::Restart);
    let (_, fx) = reply(s, Reply::Done);
    assert!(fx.is_empty());
}

proptest! {
    /// The host's latest intent wins: with ops raised between any effect and
    /// its reply, the plugin settles in the state the ops ask for, idle. The
    /// intent is a final stage and `Prepare`: `Reconfigure(p)` asks for an
    /// active plugin at `p`, `Deactivate` for a loaded one, and `Activate(p)`
    /// for an active one at `p` only when the intent is not active already.
    /// When the plugin may refuse or stick, nothing is promised about the end
    /// state, only that it settles without breaking a transition.
    #[test]
    fn the_latest_op_wins_whenever_it_is_raised(
        rules in rules(),
        ops in prop::collection::vec((0u8..3, any::<u8>()), 1..8),
        choices in prop::collection::vec(any::<u8>(), 1..16),
        local in any::<bool>(),
        flaky in any::<bool>(),
    ) {
        let rt = if local { Rt::Local } else { Rt::Thread };
        let refuse = if flaky { Refuse::Choice } else { Refuse::Never };
        let mut shell = Shell::new(rules, refuse, rt, choices);
        let op = |(kind, n): (u8, u8)| match kind {
            0 => HostOp::Reconfigure(prep(n)),
            1 => HostOp::Deactivate,
            _ => HostOp::Activate(prep(n)),
        };
        shell.inject = ops[1..].iter().copied().map(op).collect();
        shell.op(op(ops[0]));
        while !shell.inject.is_empty() {
            let next = shell.inject.remove(0);
            shell.op(next);
        }
        while shell.returning {
            shell.return_processor();
        }
        prop_assert!(!shell.state.is_busy());
        for rec in &shell.trace {
            prop_assert!(!rec.rejoin_while_out);
            prop_assert!(!rec.transition_out_of_turn);
        }
        if !flaky {
            let mut want = None;
            for o in ops {
                match op(o) {
                    HostOp::Reconfigure(p) => want = Some(p),
                    HostOp::Deactivate => want = None,
                    HostOp::Activate(p) => want = want.or(Some(p)),
                    HostOp::Relend | HostOp::SetMode(_) | HostOp::Retune => {}
                }
            }
            match want {
                Some(p) => {
                    prop_assert_eq!(shell.state.stage(), Stage::Active);
                    prop_assert_eq!(shell.state.applied(), Some(p));
                }
                None => prop_assert_eq!(shell.state.stage(), Stage::Loaded),
            }
        }
    }
}

/// **A relend of an active plugin recalls and lends again, and calls nothing
/// on the plugin;** on a plugin that is not active, or one still activating, it
/// asks for nothing.
#[test]
fn a_relend_recalls_and_lends_with_no_deactivation_or_activation() {
    let p = prep(1);
    let (s, fx) = go(active(RESTARTS, p), Event::Op(HostOp::Relend));
    assert_eq!(fx, [Fx::Recall]);
    let (s, fx) = reply(s, Reply::Returned);
    assert_eq!(fx, [Fx::Lend]);
    assert_eq!(s.stage(), Stage::Active);
    assert!(!s.is_busy());
    assert!(go(State::new(REST), Event::Op(HostOp::Relend)).1.is_empty());
    assert!(go(refused(REST, p), Event::Op(HostOp::Relend)).1.is_empty());
    let (s, _) = go(State::new(REST), Event::Op(HostOp::Activate(p)));
    assert!(
        go(s, Event::Op(HostOp::Relend)).1.is_empty(),
        "left to the activation"
    );
}

#[test]
fn an_activate_leaves_an_active_plugins_restart_as_it_is() {
    let (q, r) = (prep(2), prep(3));
    let s = active(RESTARTS, prep(1));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = reply(s, Reply::Lent);
    let (s, fx) = go(s, Event::Op(HostOp::Activate(r)));
    assert!(fx.is_empty());
    let (s, _) = go(s, Event::Returned);
    let (_, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(q)]);
}

#[test]
fn an_activate_does_not_displace_a_kept_op_that_leaves_the_plugin_active() {
    let (p, q, r) = (prep(1), prep(2), prep(3));
    let settle = |s: State| {
        let (s, _) = reply(s, Reply::Activated);
        let (s, _) = reply(s, Reply::Returned);
        let (s, fx) = reply(s, Reply::Deactivated);
        (s, fx)
    };
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = go(s, Event::Op(HostOp::Activate(r)));
    let (_, fx) = settle(s);
    assert_eq!(fx, [Fx::Activate(q)]);
    // After a kept deactivate, an activate asks for an activation at its own
    // `Prepare`.
    let (s, _) = go(State::new(RESTARTS), Event::Op(HostOp::Activate(p)));
    let (s, _) = go(s, Event::Op(HostOp::Deactivate));
    let (s, _) = go(s, Event::Op(HostOp::Activate(r)));
    let (_, fx) = settle(s);
    assert_eq!(fx, [Fx::Activate(r)]);
}

#[test]
fn a_rescan_raised_before_a_deactivation_is_read_after_the_next_activation() {
    let q = prep(2);
    let s = req(
        active(RESTARTS, prep(1)),
        Request::RescanParams(RescanScope::All),
    );
    let (s, fx) = go(s, Event::Op(HostOp::Deactivate));
    assert_eq!(fx, [Fx::Recall]);
    let (s, _) = reply(s, Reply::Returned);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert!(fx.is_empty());
    let (s, _) = go(s, Event::Op(HostOp::Activate(q)));
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx[4..],
        [
            Fx::Params(RescanScope::All),
            Fx::ParamTitles,
            Fx::ParamValues
        ]
    );
    // It is read once.
    let (s, _) = go(s, Event::Op(HostOp::Reconfigure(q)));
    let (s, _) = reply(s, Reply::Returned);
    let (s, _) = reply(s, Reply::Deactivated);
    let (_, fx) = reply(s, Reply::Activated);
    assert_eq!(fx.len(), 4);
}

#[test]
fn a_callback_raised_on_a_refused_plugin_is_dropped() {
    let s = req(refused(RESTARTS, prep(1)), Request::Callback);
    assert!(go(s.clone(), Event::Tick).1.is_empty());
    // Nor is it kept for the activation that follows.
    let (s, _) = go(s, Event::Op(HostOp::Activate(prep(1))));
    let (s, _) = reply(s, Reply::Activated);
    assert!(go(s, Event::Tick).1.is_empty());
}

/// A MIDI-mapping change is rebuilt in place, once, with no restart; a
/// note-expression change is told to the host once.
#[test]
fn a_midi_mapping_or_note_expression_change_needs_no_restart() {
    for rules in [REST, RESTARTS] {
        let s = req(active(rules, prep(1)), Request::MidiCcAssignmentChanged);
        let s = req(s, Request::NoteExpressionChanged);
        let (s, fx) = go(s, Event::Tick);
        assert_eq!(
            fx,
            [
                Fx::MidiMapping,
                Fx::MidiMapChangedNotice,
                Fx::NoteExprNotice
            ]
        );
        assert!(go(s, Event::Tick).1.is_empty());
    }
}

/// Raised beside a restart, the mapping is rebuilt once after the activation
/// and the note-expression notice is told once.
#[test]
fn a_mapping_change_beside_a_restart_is_rebuilt_after_the_activation() {
    let s = req(active(RESTARTS, prep(1)), Request::Restart);
    let s = req(s, Request::MidiCcAssignmentChanged);
    let s = req(s, Request::NoteExpressionChanged);
    let (s, fx) = go(s, Event::Tick);
    assert_eq!(fx, [Fx::NoteExprNotice, Fx::Recall]);
    let (s, fx) = reply(s, Reply::Returned);
    assert_eq!(fx, [Fx::Deactivate]);
    let (s, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(prep(1))]);
    let (s, fx) = reply(s, Reply::Activated);
    assert_eq!(
        fx,
        [
            Fx::Latency,
            Fx::Tail,
            Fx::Layout,
            Fx::Lend,
            Fx::MidiMapping,
            Fx::MidiMapChangedNotice
        ]
    );
    assert!(go(s, Event::Tick).1.is_empty());
}

/// **A live mode is told once, and the answer is what the plugin runs in**: an
/// active plugin asked for a mode is behind in it until told, is told by one
/// effect, is not behind while the answer is out, and a taken answer applies
/// the mode without planning a restart; a later restart activates in it.
#[test]
fn a_live_mode_is_told_once_and_a_taken_answer_applies_it_without_a_restart() {
    let p = prep(1);
    let offline = p.with_mode(RenderMode::Offline);
    let mut s = active(RESTARTS, p);
    s.want(offline);
    assert_eq!(
        s.behind(false, RenderModeSupport::Live),
        Behind::Mode(RenderMode::Offline)
    );
    let (s, fx) = go(s, Event::Op(HostOp::SetMode(RenderMode::Offline)));
    assert_eq!(fx, [Fx::SetMode(RenderMode::Offline)]);
    assert_eq!(
        s.behind(false, RenderModeSupport::Live),
        Behind::No,
        "told, and not asked again while the answer is out"
    );
    let (s, fx) = go(s, Event::Answered(Told::ModeTook(RenderMode::Offline)));
    assert!(fx.is_empty());
    assert_eq!(s.applied().map(|p| p.mode()), Some(RenderMode::Offline));
    assert_eq!(s.stage(), Stage::Active);
    assert!(
        go(s.clone(), Event::Tick).1.is_empty(),
        "nothing is planned"
    );

    let s = req(s, Request::Restart);
    let (s, _) = go(s, Event::Tick);
    let (s, _) = reply(s, Reply::Returned);
    let (_, fx) = reply(s, Reply::Deactivated);
    assert_eq!(fx, [Fx::Activate(offline)]);
}

/// **A refused live mode is given up**: the wanted mode goes back to the one
/// the plugin runs in, the host is told, and the plugin is not behind.
#[test]
fn a_refused_live_mode_rolls_the_wanted_mode_back_and_is_told() {
    let p = prep(1);
    let mut s = active(RESTARTS, p);
    s.want(p.with_mode(RenderMode::Offline));
    let (s, _) = go(s, Event::Op(HostOp::SetMode(RenderMode::Offline)));
    let (s, fx) = go(s, Event::Answered(Told::ModeRefused(RenderMode::Offline)));
    assert_eq!(fx, [Fx::ModeRefusedNotice]);
    assert_eq!(s.wanted(), Some(p));
    assert_eq!(s.behind(false, RenderModeSupport::Live), Behind::No);
}

/// **A mode read at activation restarts the plugin, and a plugin with no mode
/// is never behind in it**: the wanted mode is only kept.
#[test]
fn a_mode_read_at_activation_restarts_and_no_mode_is_never_behind() {
    let p = prep(1);
    let mut s = active(RESTARTS, p);
    s.want(p.with_mode(RenderMode::Offline));
    assert_eq!(
        s.behind(false, RenderModeSupport::OnActivation),
        Behind::Restart
    );
    assert_eq!(s.behind(false, RenderModeSupport::Unsupported), Behind::No);
}

/// **A `Prepare` carries its rate and a block of at least one frame**: both
/// types refuse the rest, so a host is never asked to activate at one.
#[test]
fn a_prepare_carries_a_rate_and_a_block() {
    let block = MaxBlock::of(Samples::new(64));
    for rate in [48_000.0, 44_100.0, f64::MIN_POSITIVE, f64::MAX] {
        let prepare = Prepare::new(SampleRate::of(rate), block);
        assert_eq!(prepare.rate(), SampleRate::of(rate));
        assert_eq!(prepare.max_block(), block);
        assert_eq!(prepare.format(), FloatWidth::F32);
        assert_eq!(prepare.mode(), RenderMode::Realtime);
    }
    assert_eq!(
        prep(0).with_rate(SampleRate::SR_96K).rate(),
        SampleRate::SR_96K
    );
    assert!(MaxBlock::new(Samples::new(0)).is_none());
}

/// `MaxBlock` keeps the range contract, serde included.
#[cfg(feature = "serde")]
#[test]
fn a_max_block_keeps_the_range_contract() {
    let c = tutti_fixtures::RangeContract {
        new: MaxBlock::new,
        get: MaxBlock::samples,
        of: Some(MaxBlock::of),
        valid: &[
            Samples::new(1),
            Samples::new(64),
            Samples::new(4096),
            Samples::new(u32::MAX as usize),
        ],
        // The bound is `u32::MAX` frames; a `usize` wider than that is refused.
        #[cfg(target_pointer_width = "64")]
        invalid: &[
            Samples::new(0),
            Samples::new(u32::MAX as usize + 1),
            Samples::new(usize::MAX),
        ],
        #[cfg(not(target_pointer_width = "64"))]
        invalid: &[Samples::new(0)],
    };
    tutti_fixtures::check_range_contract(&c);
    tutti_fixtures::check_range_serde(&c);
}

/// `BeatsPerBar` keeps the range contract.
#[cfg(feature = "serde")]
#[test]
fn a_beats_per_bar_keeps_the_range_contract() {
    use crate::BeatsPerBar;
    let c = tutti_fixtures::RangeContract {
        new: BeatsPerBar::new,
        get: BeatsPerBar::get,
        of: Some(BeatsPerBar::of),
        valid: &[1, 2, 7, 63, 64],
        invalid: &[0, 65, 999, u32::MAX],
    };
    tutti_fixtures::check_range_contract(&c);
    tutti_fixtures::check_range_serde(&c);
}

/// `NoteValue` keeps the range contract: only powers of two from 1 to 64.
#[cfg(feature = "serde")]
#[test]
fn a_note_value_keeps_the_range_contract() {
    use crate::NoteValue;
    let c = tutti_fixtures::RangeContract {
        new: NoteValue::new,
        get: NoteValue::get,
        of: Some(NoteValue::of),
        valid: &[1, 2, 4, 8, 16, 32, 64],
        invalid: &[0, 3, 5, 6, 7, 12, 63, 65, 128, 1 << 31, u32::MAX],
    };
    tutti_fixtures::check_range_contract(&c);
    tutti_fixtures::check_range_serde(&c);
}

/// **The standing of a refused rate says where the plugin is**: the rate it runs at while
/// active, "deactivated" otherwise. A refusal that leaves a plugin deactivated is reachable
/// only after a failed rollback, which no probe forces, so the sentence is pinned here, where
/// every loader gets it.
#[test]
fn a_standing_says_running_or_deactivated() {
    assert_eq!(
        crate::standing(true, SampleRate::SR_48K),
        "running at 48000"
    );
    assert_eq!(crate::standing(false, SampleRate::SR_48K), "deactivated");
}
