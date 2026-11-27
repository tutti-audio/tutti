//! A patch's sections: a `Section` whose deps equal the last patch's is carried,
//! its body not run and nothing of it compared; one whose deps differ, whose
//! swap was refused, whose node was removed by hand or whose mentioned key fell
//! runs like any other part.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tutti_buffers::ChannelLayout;
use tutti_session::{
    Declarations, Graph, IntoPatch, Mention, NodeDecl, PatchError, ReplaceRefused, Section,
    Session, WireStart,
};
use tutti_units::Hz;

use super::patch_wires::{PassConfig, SrcConfig};

const SRC: [&str; 2] = ["src0", "src1"];
const PASS: [&str; 2] = ["pass0", "pass1"];

fn session() -> Session {
    Session::builder().build().expect("builds")
}

fn src() -> SrcConfig {
    SrcConfig::new(ChannelLayout::MONO)
}

fn pass() -> PassConfig {
    PassConfig::new(ChannelLayout::MONO)
}

fn runs() -> Arc<AtomicUsize> {
    Arc::default()
}

fn count(runs: &Arc<AtomicUsize>) -> usize {
    runs.load(Ordering::SeqCst)
}

/// The section `"t<id>"`: a source and, when `with_pass`, a pass it feeds, from
/// deps `(level, with_pass)`.
fn track(
    id: usize,
    deps: (u64, bool),
    ran: &Arc<AtomicUsize>,
) -> impl IntoPatch<&'static str, Graph<f32>> + use<> {
    let ran = Arc::clone(ran);
    Section::new(["t0", "t1"][id], deps, move |&(level, with_pass)| {
        ran.fetch_add(1, Ordering::SeqCst);
        let source = NodeDecl::new(SRC[id], src().level(level));
        let tail = with_pass.then(|| {
            let sink = NodeDecl::new(PASS[id], pass());
            let wire = (&source).connect(&sink);
            (sink, wire)
        });
        (source, tail)
    })
}

fn nothing() -> Vec<NodeDecl<&'static str, SrcConfig>> {
    Vec::new()
}

fn node(s: &Session, key: &'static str) -> tutti_graph::NodeKey {
    s.key::<SrcConfig>(&key).unwrap().erase()
}

/// **Equal deps carry the section**: its body does not run, nothing is
/// reported, the node keeps its key and a value set on it.
#[test]
fn equal_deps_carry_the_section() {
    let (mut s, ran) = (session(), runs());
    s.patch(track(0, (4, true), &ran)).unwrap();
    let key = s.key::<SrcConfig>(&"src0").unwrap();
    s.set(key.cutoff().to(Hz::new(800.0))).unwrap();

    let patched = s.patch(track(0, (4, true), &ran)).unwrap();

    assert_eq!(count(&ran), 1);
    assert!(patched.rebuilt.is_empty() && patched.removed.is_empty());
    assert!(patched.readded.is_empty() && patched.unconverged.is_empty());
    assert!(patched.dormant.is_empty());
    assert_eq!(node(&s, "src0"), key.erase());
    assert_eq!(s.get(key.cutoff()).unwrap(), Hz::new(800.0));
}

/// **Unequal deps run the body once, and the diff is the ordinary one**: a
/// changed structure is rebuilt, a key the body drops is removed.
#[test]
fn unequal_deps_run_and_diff_as_any_patch() {
    let (mut s, ran) = (session(), runs());
    s.patch(track(0, (4, true), &ran)).unwrap();

    let patched = s.patch(track(0, (8, false), &ran)).unwrap();

    assert_eq!(count(&ran), 2);
    assert_eq!(patched.rebuilt, ["src0"]);
    assert_eq!(patched.removed, ["pass0"]);
}

/// **Deps of another type are unequal**: the body runs.
#[test]
fn deps_of_another_type_run_the_body() {
    let (mut s, ran) = (session(), runs());
    let section = |deps: u64, ran: &Arc<AtomicUsize>| {
        let ran = Arc::clone(ran);
        Section::new("t", deps, move |_| {
            ran.fetch_add(1, Ordering::SeqCst);
            NodeDecl::new("n", src())
        })
    };
    s.patch(section(1, &ran)).unwrap();
    s.patch(section(1, &ran)).unwrap();
    assert_eq!(count(&ran), 1);

    let other = {
        let ran = Arc::clone(&ran);
        Section::new("t", 1_u32, move |_| {
            ran.fetch_add(1, Ordering::SeqCst);
            NodeDecl::new("n", src())
        })
    };
    s.patch(other).unwrap();

    assert_eq!(count(&ran), 2);
}

/// **A node removed by hand runs the body of a section with equal deps**, and
/// the key is readded.
#[test]
fn a_node_removed_by_hand_runs_the_body_and_is_readded() {
    let (mut s, ran) = (session(), runs());
    s.patch(track(0, (4, false), &ran)).unwrap();
    let _ = s.remove(node(&s, "src0")).unwrap().unfaded();

    let patched = s.patch(track(0, (4, false), &ran)).unwrap();

    assert_eq!(count(&ran), 2);
    assert_eq!(patched.readded, ["src0"]);
    assert!(s.key::<SrcConfig>(&"src0").is_ok());
}

/// **A refused swap retries**: the section's key is `unconverged`, its record
/// keeps no deps, and the next patch with equal deps runs the body and lands the
/// swap.
#[test]
fn a_refused_swap_runs_the_body_again() {
    let (mut s, ran, cut) = (session(), runs(), Arc::new(AtomicBool::new(false)));
    let section = |wide: bool| {
        let (ran, cut) = (Arc::clone(&ran), Arc::clone(&cut));
        Section::new("t", wide, move |&wide| {
            ran.fetch_add(1, Ordering::SeqCst);
            let layout = if wide {
                ChannelLayout::STEREO
            } else {
                ChannelLayout::MONO
            };
            let decl = NodeDecl::new("n", SrcConfig::new(layout));
            if cut.load(Ordering::SeqCst) {
                decl.unfaded()
            } else {
                decl
            }
        })
    };
    s.patch(section(false)).unwrap();
    let _ = s.update();

    let refused = s.patch(section(true)).unwrap();
    assert!(matches!(
        refused.unconverged.as_slice(),
        [("n", ReplaceRefused::ShapeChanged { .. })]
    ));
    cut.store(true, Ordering::SeqCst);
    let retried = s.patch(section(true)).unwrap();

    assert_eq!(count(&ran), 3);
    assert_eq!(retried.rebuilt, ["n"]);
    s.patch(section(true)).unwrap();
    assert_eq!(count(&ran), 3, "a converged section is carried again");
}

/// Sections `"a"` (the source, when `has_src`) and `"b"` (a pass, wired from
/// the source when `wired`, a flag the body reads).
fn pair(
    has_src: bool,
    level: u64,
    wired: &Arc<AtomicBool>,
    ran: &Arc<AtomicUsize>,
) -> impl IntoPatch<&'static str, Graph<f32>> + use<> {
    let wired = Arc::clone(wired);
    let ran = Arc::clone(ran);
    (
        Section::new("a", (has_src, level), |&(has_src, level)| {
            has_src.then(|| NodeDecl::new("src0", src().level(level)))
        }),
        Section::new("b", (), move |()| {
            ran.fetch_add(1, Ordering::SeqCst);
            let sink = NodeDecl::new("pass0", pass());
            let wire = wired
                .load(Ordering::SeqCst)
                .then(|| Mention::new("src0").connect(&sink));
            (sink, wire)
        }),
    )
}

/// **A section whose mentioned key is no longer declared runs**: a body that
/// reads a moved flag and drops the wire patches, and a pure one is refused as
/// any patch with the wire is.
#[test]
fn a_removed_mention_runs_the_section_that_wires_to_it() {
    let (mut s, ran, wired) = (session(), runs(), Arc::new(AtomicBool::new(true)));
    s.patch(pair(true, 4, &wired, &ran)).unwrap();
    wired.store(false, Ordering::SeqCst);
    s.patch(pair(false, 4, &wired, &ran)).unwrap();
    assert_eq!(count(&ran), 2);

    let (mut s, ran, wired) = (session(), runs(), Arc::new(AtomicBool::new(true)));
    s.patch(pair(true, 4, &wired, &ran)).unwrap();
    let refused = s.patch(pair(false, 4, &wired, &ran)).unwrap_err();
    assert_eq!(refused, PatchError::Undeclared("src0"));
    assert_eq!(count(&ran), 2);
    assert!(s.key::<SrcConfig>(&"src0").is_ok(), "nothing changed");
}

/// **A section whose mentioned key is reshaped runs**: the wire is made again
/// to the swapped node.
#[test]
fn a_reshaped_mention_runs_the_section_that_wires_to_it() {
    let (mut s, ran, wired) = (session(), runs(), Arc::new(AtomicBool::new(true)));
    s.patch(pair(true, 4, &wired, &ran)).unwrap();
    s.patch(pair(true, 4, &wired, &ran)).unwrap();
    assert_eq!(count(&ran), 1);

    let patched = s.patch(pair(true, 8, &wired, &ran)).unwrap();

    assert_eq!(patched.rebuilt, ["src0"]);
    assert_eq!(count(&ran), 2);
}

/// **A section key absent from the patch removes its nodes**, and the next patch
/// that declares it again adds them: its record did not outlive it.
#[test]
fn an_absent_section_removes_its_nodes_and_runs_when_declared_again() {
    let (mut s, ran) = (session(), runs());
    s.patch(track(0, (4, true), &ran)).unwrap();

    let patched = s.patch(nothing()).unwrap();
    assert_eq!(patched.removed.len(), 2);
    assert!(s.key::<SrcConfig>(&"src0").is_err());

    let patched = s.patch(track(0, (4, true), &ran)).unwrap();
    assert_eq!(count(&ran), 2);
    assert!(patched.readded.is_empty());
    assert!(s.key::<SrcConfig>(&"src0").is_ok());
}

/// **A section key names one section; a node key of a carried section is still
/// its own**: a second section is `DuplicateSection`, a second node under a
/// carried key is `DuplicateKey`.
#[test]
fn duplicate_keys_are_refused_for_sections_and_carried_nodes() {
    let (mut s, ran) = (session(), runs());
    let twice = s.patch((track(0, (4, false), &ran), track(0, (8, false), &ran)));
    assert_eq!(twice.unwrap_err(), PatchError::DuplicateSection("t0"));

    s.patch(track(0, (4, false), &ran)).unwrap();
    let again = s.patch((track(0, (4, false), &ran), NodeDecl::new("src0", src())));
    assert_eq!(again.unwrap_err(), PatchError::DuplicateKey("src0"));
}

/// **An inner section is carried while its outer one runs, and a carried outer
/// carries its inner record**: a later patch still carries the inner.
#[test]
fn inner_sections_are_judged_alone_and_carried_with_their_outer() {
    let (mut s, outer, inner) = (session(), runs(), runs());
    let patch = |rev: u32| {
        let (outer, inner) = (Arc::clone(&outer), Arc::clone(&inner));
        Section::new("o", rev, move |_| {
            outer.fetch_add(1, Ordering::SeqCst);
            track(0, (4, true), &inner)
        })
    };

    s.patch(patch(1)).unwrap();
    s.patch(patch(2)).unwrap();
    assert_eq!(
        (count(&outer), count(&inner)),
        (2, 1),
        "outer ran, inner carried"
    );

    s.patch(patch(2)).unwrap();
    assert_eq!((count(&outer), count(&inner)), (2, 1), "both carried");

    s.patch(patch(3)).unwrap();
    assert_eq!(
        (count(&outer), count(&inner)),
        (3, 1),
        "inner carried again"
    );
}

/// **A refused patch leaves the records**: the next patch with the first deps
/// carries.
#[test]
fn a_refused_patch_leaves_the_records() {
    let (mut s, ran) = (session(), runs());
    s.patch(track(0, (4, false), &ran)).unwrap();

    let refused = s.patch((
        track(0, (8, false), &ran),
        Mention::new("src0").connect(Mention::new("nowhere")),
    ));
    assert_eq!(refused.unwrap_err(), PatchError::Undeclared("nowhere"));
    assert_eq!(count(&ran), 2);

    s.patch(track(0, (4, false), &ran)).unwrap();
    assert_eq!(count(&ran), 2, "the record of the first patch carries");
}

/// **Declarations are `Send`**: a patch built on one thread is applied on
/// another.
#[test]
fn declarations_built_on_one_thread_are_applied_on_another() {
    let ran = runs();
    let built = std::thread::spawn({
        let ran = Arc::clone(&ran);
        move || {
            [track(0, (4, true), &ran)]
                .into_iter()
                .collect::<Declarations<_, _>>()
        }
    })
    .join()
    .unwrap();
    let mut s = session();

    s.patch(built).unwrap();

    assert_eq!(count(&ran), 1);
    assert!(s.key::<PassConfig>(&"pass0").is_ok());
}
