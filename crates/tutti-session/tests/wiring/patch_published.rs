//! A published value a config gives is a value of the running node: a patch
//! that declares another one writes it through the published target, which a
//! watcher sees, and a patch that declares the same one writes nothing.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_midi_runtime::{ClipEvents, ClipEventsKind, TimedMidiEvent};
use tutti_session::{ChangedBy, NodeDecl, Session};
use tutti_ump::MidiChannel;
use tutti_units::{Beat, Hz, SampleRate};

/// A silent source holding a clip.
#[derive(Clone)]
pub struct Player {
    cells: PlayerCells,
}

declare_params!([] Player, PlayerConfig, PlayerCells {
    params { Cutoff => cutoff: Hz = Hz::new(1_000.0) }
    published { events: ClipEventsKind = Arc::new(ClipEvents::default()) }
});

impl From<PlayerConfig> for Player {
    fn from(config: PlayerConfig) -> Self {
        Self {
            cells: PlayerCells::from_config(&config),
        }
    }
}

impl Node for Player {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO).param_ports(self.declared_ports())
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
    fn param_base(&self, port: usize) -> Option<f64> {
        self.declared_base(port)
    }
}

fn clip(notes: u8) -> Arc<ClipEvents> {
    Arc::new(
        (0..notes)
            .map(|n| {
                let on = tutti_fixtures::midi::note_on(MidiChannel::FIRST, 60 + n, 0xFFFF);
                TimedMidiEvent::new(Beat::new(f64::from(n)), on).unwrap()
            })
            .collect(),
    )
}

fn session() -> Session {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .outputs(ChannelLayout::MONO)
        .sample_rate(SampleRate::SR_48K)
        .block_dates(tutti_session::BlockDates::Frames)
        .build()
        .expect("builds")
}

/// **A changed published initial is written to the running node and seen by a
/// watcher as an app change; an equal value, built afresh, changes nothing**:
/// the initials compare by content, not by the `Arc`.
#[test]
fn a_changed_published_initial_is_written_and_an_equal_one_is_not() {
    let mut s = session();
    s.patch(NodeDecl::new("p", PlayerConfig::new().events(clip(2))))
        .unwrap();
    let key = s.key::<PlayerConfig>(&"p").unwrap();
    let mut watcher = s.watch(key.events()).unwrap();
    assert!(watcher.changes().unwrap().is_none());

    s.patch(NodeDecl::new("p", PlayerConfig::new().events(clip(2))))
        .unwrap();
    assert!(
        watcher.changes().unwrap().is_none(),
        "an equal clip writes nothing"
    );

    s.patch(NodeDecl::new("p", PlayerConfig::new().events(clip(3))))
        .unwrap();
    let change = watcher
        .changes()
        .unwrap()
        .expect("the patch moved the clip");
    assert_eq!(change.by, ChangedBy::App);
    assert_eq!(change.value.len(), 3);
    assert_eq!(s.get(key.events()).unwrap().len(), 3);
}

/// **The hand-written `MidiClipConfig` lists its clip as an initial**: a
/// re-patch with another clip writes it to the running clip node and a watcher
/// sees it, and the same clip writes nothing.
#[test]
fn a_midi_clip_config_declares_its_clip_as_a_published_initial() {
    use tutti_midi_runtime::MidiClipConfig;

    let mut s = session();
    let first = clip(1);
    s.patch(NodeDecl::new(
        "clip",
        MidiClipConfig::default().events(first.clone()),
    ))
    .unwrap();
    let key = s.key::<MidiClipConfig>(&"clip").unwrap();
    assert_eq!(s.get(key.events()).unwrap().len(), 1);
    let mut watcher = s.watch(key.events()).unwrap();

    s.patch(NodeDecl::new(
        "clip",
        MidiClipConfig::default().events(first),
    ))
    .unwrap();
    assert!(
        watcher.changes().unwrap().is_none(),
        "the same clip writes nothing"
    );

    s.patch(NodeDecl::new(
        "clip",
        MidiClipConfig::default().events(clip(4)),
    ))
    .unwrap();
    let change = watcher
        .changes()
        .unwrap()
        .expect("the patch moved the clip");
    assert_eq!(change.by, ChangedBy::App);
    assert_eq!(change.value.len(), 4);
}

/// **A patch that changes a param and a published value writes both as one
/// gesture**: the published write joins the params' batch.
#[test]
fn a_changed_param_and_published_value_are_one_gesture() {
    let mut s = session();
    s.patch(NodeDecl::new("p", PlayerConfig::new().events(clip(1))))
        .unwrap();
    let key = s.key::<PlayerConfig>(&"p").unwrap();
    let mut watcher = s.watch((key.cutoff(), key.events())).unwrap();

    s.patch(NodeDecl::new(
        "p",
        PlayerConfig::new().cutoff(Hz::new(300.0)).events(clip(3)),
    ))
    .unwrap();

    let (cutoff, events) = watcher.changes().unwrap();
    let (cutoff, events) = (cutoff.unwrap(), events.unwrap());
    assert_eq!(cutoff.value, Hz::new(300.0));
    assert_eq!(events.value.len(), 3);
    assert_eq!(cutoff.gesture.unwrap().id, events.gesture.unwrap().id);
}
