//! The judgement of a patch's sections: which are carried whole and which run,
//! and the records the session keeps of them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::wire::Resolved;
use super::{Kept, PatchError, Patched};
#[cfg(feature = "plugin")]
use crate::patch::HoldsPlugin;
use crate::patch::PatchFloat;
use crate::patch::{
    Body, Declaration, Declarations, Deps, PendingSection, WireDecl, WireEnds, WireId,
};
use crate::{AppKey, Graph};

/// What the last patch made of one section: its deps while it converged, and
/// the keys, wires and sections its body declared itself (not those of the
/// sections inside it).
pub(super) struct KeptSection<K> {
    /// `None` when a key of the section's subtree was left unconverged or a
    /// wire dormant: the next patch runs the body.
    pub(super) deps: Option<Box<dyn Deps>>,
    pub(super) nodes: Vec<K>,
    pub(super) wires: Vec<WireId<K>>,
    pub(super) sections: Vec<K>,
}

/// The record of `key` and of the sections inside it, outermost first.
fn subtree<'a, K: Ord>(
    sections: &'a BTreeMap<K, KeptSection<K>>,
    key: &K,
) -> Vec<(&'a K, &'a KeptSection<K>)> {
    let mut found = Vec::new();
    let mut stack = vec![key];
    while let Some(key) = stack.pop() {
        if let Some((name, record)) = sections.get_key_value(key) {
            found.push((name, record));
            stack.extend(&record.sections);
        }
    }
    found
}

impl<K: Ord + Clone, F: PatchFloat> Kept<K, F> {
    /// Replaces the section records: the carried subtrees keep theirs, the
    /// evaluated sections get theirs (in the order they ran, so a section's
    /// inner ones are judged first), and a section the patch does not declare
    /// loses its record.
    pub(super) fn keep_sections(
        &mut self,
        carried: &[K],
        evaluated: Vec<(K, KeptSection<K>)>,
        patched: &Patched<K>,
    ) {
        let mut old = std::mem::take(&mut self.sections);
        let unconverged: BTreeSet<&K> = patched.unconverged.iter().map(|(key, _)| key).collect();
        let dormant: BTreeSet<&WireEnds<K>> =
            patched.dormant.iter().map(|(ends, _)| ends).collect();
        for key in carried {
            let names: Vec<K> = subtree(&old, key)
                .into_iter()
                .map(|(name, _)| name.clone())
                .collect();
            for name in names {
                if let Some(record) = old.remove(&name) {
                    self.sections.insert(name, record);
                }
            }
        }
        for (key, mut record) in evaluated.into_iter().rev() {
            let converged = record.nodes.iter().all(|key| !unconverged.contains(key))
                && record.wires.iter().all(|id| !dormant.contains(&id.ends))
                && record
                    .sections
                    .iter()
                    .all(|inner| self.sections.get(inner).is_some_and(|r| r.deps.is_some()));
            if !converged {
                record.deps = None;
            }
            self.sections.insert(key, record);
        }
    }
}

/// A patch's parts with its sections decided: the parts of the sections that
/// ran joined to the patch's own, and the sections carried whole.
pub(super) struct Decided<K, F: PatchFloat> {
    /// The declarations, each with what it does to its key's node.
    pub(super) nodes: Vec<(K, Declaration<Graph<F>>, Kind)>,
    pub(super) wires: Vec<WireDecl<K>>,
    /// The carried sections, the outermost of each carried subtree.
    pub(super) carried: Vec<K>,
    /// The keys and wires the carried subtrees declare.
    pub(super) carried_nodes: Vec<K>,
    pub(super) carried_wires: Vec<WireId<K>>,
    /// The records of the sections that ran, in the order they ran.
    pub(super) evaluated: Vec<(K, KeptSection<K>)>,
    /// Sections met and not yet judged.
    queue: VecDeque<PendingSection<K, Graph<F>>>,
    /// The section keys met.
    names: BTreeSet<K>,
    /// The sections that are carried unless a key they mention fails.
    candidates: BTreeMap<K, PendingSection<K, Graph<F>>>,
    /// The node keys of the candidates' subtrees.
    standing: BTreeSet<K>,
    /// The index in `nodes` of each key a section that ran declares.
    declared_at: BTreeMap<K, usize>,
}

impl<K: AppKey, F: PatchFloat> Decided<K, F> {
    fn new(patch: Declarations<K, Graph<F>>, judge: &Judge<'_, K, F>) -> Self {
        let nodes: Vec<_> = patch
            .nodes
            .into_iter()
            .map(|(key, declaration)| {
                let kind = kind(judge.graph, judge.kept, &key, &declaration);
                (key, declaration, kind)
            })
            .collect();
        let declared_at = nodes
            .iter()
            .enumerate()
            .map(|(at, (key, ..))| (key.clone(), at))
            .collect();
        Self {
            nodes,
            wires: patch.wires,
            carried: Vec::new(),
            carried_nodes: Vec::new(),
            carried_wires: Vec::new(),
            evaluated: Vec::new(),
            queue: patch.sections.into(),
            names: BTreeSet::new(),
            candidates: BTreeMap::new(),
            standing: BTreeSet::new(),
            declared_at,
        }
    }

    /// Runs `section`'s body: what it declares joins the patch, and the
    /// sections inside it wait to be judged.
    fn run(&mut self, section: PendingSection<K, Graph<F>>, judge: &Judge<'_, K, F>) {
        let PendingSection { key, deps, body } = section;
        let inner = body(&*deps);
        let record = KeptSection {
            deps: Some(deps),
            nodes: inner.nodes.iter().map(|(key, _)| key.clone()).collect(),
            wires: inner.wires.iter().map(WireDecl::id).collect(),
            sections: inner.sections.iter().map(|s| s.key.clone()).collect(),
        };
        for (key, declaration) in inner.nodes {
            self.declared_at.insert(key.clone(), self.nodes.len());
            let kind = kind(judge.graph, judge.kept, &key, &declaration);
            self.nodes.push((key, declaration, kind));
        }
        self.wires.extend(inner.wires);
        self.queue.extend(inner.sections);
        self.evaluated.push((key, record));
    }
}

/// What a declaration does to the node under its key, judged before any node is
/// built.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Kind {
    Add,
    Readd,
    Values,
    Swap,
}

/// What `declaration` does to the node under `key`, with nothing built.
pub(super) fn kind<F: PatchFloat, K: AppKey>(
    graph: &Graph<F>,
    kept: &Kept<K, F>,
    key: &K,
    declaration: &Declaration<Graph<F>>,
) -> Kind {
    let Some(old) = kept.nodes.get(key) else {
        return Kind::Add;
    };
    // What the two declarations are, and whether the old node is still the
    // declared one: a plugin whose record retired is spent, so its key gets a
    // new node.
    let (same_type, same_structure, standing) = match (&old.declaration.body, &declaration.body) {
        (Body::Node(old), Body::Node(new)) => (
            old.config_type() == new.config_type()
                && old.played().is_some() == new.played().is_some(),
            old.same_structure(&**new),
            true,
        ),
        #[cfg(feature = "plugin")]
        (Body::Plugin(old), Body::Plugin(new)) => {
            let (old, new) = (old.held(), new.held());
            (true, old.is_plugin_of(new), !old.is_closed())
        }
        #[cfg(feature = "plugin")]
        _ => (false, false, true),
    };
    if !graph.contains(old.node) || !same_type || !standing {
        Kind::Readd
    } else if same_structure {
        Kind::Values
    } else {
        Kind::Swap
    }
}

/// What the last patch kept, and the graph a patch is judged against.
struct Judge<'a, K, F: PatchFloat> {
    kept: &'a Kept<K, F>,
    graph: &'a Graph<F>,
}

/// Decides each section of `patch` against what the last patch kept.
pub(super) fn decide<K: AppKey, F: PatchFloat>(
    graph: &Graph<F>,
    kept: &Kept<K, F>,
    patch: Declarations<K, Graph<F>>,
) -> Result<Decided<K, F>, PatchError<K>> {
    Judge { kept, graph }.judge(patch)
}

impl<K: AppKey, F: PatchFloat> Judge<'_, K, F> {
    /// Decides each section of `patch`: carried whole, or run with the parts it
    /// declares joining the patch. A section is carried only while the keys it
    /// mentions stand, and running one can make another's keys fall, so the
    /// judgement repeats until none changes; each section runs at most once.
    fn judge(&self, patch: Declarations<K, Graph<F>>) -> Result<Decided<K, F>, PatchError<K>> {
        let mut decided = Decided::new(patch, self);
        self.settle(&mut decided)?;
        loop {
            let falling: Vec<K> = decided
                .candidates
                .keys()
                .filter(|key| !self.stands(&decided, key))
                .cloned()
                .collect();
            if falling.is_empty() {
                break;
            }
            // Running one can restore what another mentions: judge again.
            for key in falling {
                if self.stands(&decided, &key) {
                    continue;
                }
                let section = decided
                    .candidates
                    .remove(&key)
                    .expect("a falling section is a candidate");
                for (_, record) in subtree(&self.kept.sections, &key) {
                    for node in &record.nodes {
                        decided.standing.remove(node);
                    }
                }
                decided.run(section, self);
                self.settle(&mut decided)?;
            }
        }
        for key in decided.candidates.keys() {
            decided.carried.push(key.clone());
            for (name, record) in subtree(&self.kept.sections, key) {
                if name != key && !decided.names.insert(name.clone()) {
                    return Err(PatchError::DuplicateSection(name.clone()));
                }
                decided.carried_nodes.extend(record.nodes.iter().cloned());
                decided.carried_wires.extend(record.wires.iter().cloned());
            }
        }
        Ok(decided)
    }

    /// Judges the sections waiting in `decided`: a section whose record does
    /// not match runs, one that does becomes a candidate to be carried.
    fn settle(&self, decided: &mut Decided<K, F>) -> Result<(), PatchError<K>> {
        while let Some(section) = decided.queue.pop_front() {
            if !decided.names.insert(section.key.clone()) {
                return Err(PatchError::DuplicateSection(section.key));
            }
            if self.matches(&section) {
                for (_, record) in subtree(&self.kept.sections, &section.key) {
                    decided.standing.extend(record.nodes.iter().cloned());
                }
                decided.candidates.insert(section.key.clone(), section);
            } else {
                decided.run(section, self);
            }
        }
        Ok(())
    }

    /// Whether the last patch made `section` from equal deps and the graph still
    /// holds what it made: every node, every wire, and no key unconverged.
    fn matches(&self, section: &PendingSection<K, Graph<F>>) -> bool {
        let kept = &self.kept;
        let Some(record) = kept.sections.get(&section.key) else {
            return false;
        };
        let equal = record
            .deps
            .as_deref()
            .is_some_and(|last| last.same(&*section.deps));
        equal
            && subtree(&kept.sections, &section.key)
                .into_iter()
                .all(|(_, record)| {
                    let nodes = record.nodes.iter().all(|key| {
                        kept.nodes
                            .get(key)
                            .is_some_and(|s| self.graph.contains(s.node))
                    });
                    nodes
                        && record.wires.iter().all(|id| {
                            let known = [Some(id.source()), id.sink()]
                                .into_iter()
                                .flatten()
                                .all(|key| kept.nodes.contains_key(key));
                            known
                                && Resolved::of(id, |key, role| kept.at(key, role)).held(self.graph)
                        })
                })
    }

    /// Whether every key the candidate `section`'s wires mention outside it
    /// is declared by the rest of the patch, with the node it had.
    fn stands(&self, decided: &Decided<K, F>, section: &K) -> bool {
        let records = subtree(&self.kept.sections, section);
        let own: BTreeSet<&K> = records
            .iter()
            .flat_map(|(_, record)| &record.nodes)
            .collect();
        records
            .iter()
            .flat_map(|(_, record)| &record.wires)
            .flat_map(|id| [Some(id.source()), id.sink()])
            .flatten()
            .filter(|key| !own.contains(key))
            .all(|key| {
                decided.standing.contains(key)
                    || decided
                        .declared_at
                        .get(key)
                        .is_some_and(|&at| decided.nodes[at].2 == Kind::Values)
            })
    }
}
