//! A published value a config gives is a value of the running node: a patch
//! that declares another one writes it through the published target, which a
//! watcher sees, and a patch that declares the same one writes nothing.

use std::sync::Arc;

use tutti_buffers::ChannelLayout;
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status, declare_params};
use tutti_midi_runtime::{ClipEvents, ClipEventsKind, TimedMidiEvent};
use tutti_session::{ChangedBy, DetachedSession, NodeDecl, Session};
use tutti_ump::MidiChannel;
use tutti_units::{Beat, SampleRate};

/// A silent source holding a clip.
#[derive(Clone)]
pub struct Player {
    cells: PlayerCells,
}

declare_params!([] Player, PlayerConfig, PlayerCells {
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

fn session() -> DetachedSession {
    Session::builder()
        .outputs_floor(ChannelLayout::MONO)
        .build_detached(SampleRate::SR_48K)
        .expect("builds")
}

/// **A changed published initial is written to the running node and seen by a
/// watcher as an app change; the same value written again changes nothing.**
#[test]
fn a_changed_published_initial_is_written_and_the_same_one_is_not() {
    let mut s = session();
    let first = clip(2);
    s.patch(NodeDecl::new(
        "p",
        PlayerConfig::new().events(first.clone()),
    ))
    .unwrap();
    let key = s.key::<PlayerConfig>(&"p").unwrap();
    let mut watcher = s.watch(key.events()).unwrap();
    assert!(watcher.changes().unwrap().is_none());

    s.patch(NodeDecl::new("p", PlayerConfig::new().events(first)))
        .unwrap();
    assert!(
        watcher.changes().unwrap().is_none(),
        "the same clip writes nothing"
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
