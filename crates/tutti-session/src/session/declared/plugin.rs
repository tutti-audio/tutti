//! The plugin arm of a patch: a [`PluginDecl`](crate::PluginDecl) is a node over
//! a loaded plugin, which a patch checks before anything changes, and installs,
//! swaps or leaves as the plugin's identity says.
//!
//! The plan builds nothing: the plugin's node is bound over an instance that is
//! taken once, at the commit, through the one entry a plugin has
//! (`Session::install_plugin`). A refused patch leaves the install in its slot.

use std::collections::BTreeMap;

use thiserror::Error;
use tutti_graph::{NodeKey, Shape};
use tutti_plugin_types::{ParamAccess, ParameterInfo};
use tutti_units::{Normalized, ParamId};

use super::section::Kind;
use super::{Fate, Kept, PatchError, Patched, PlannedNode, Stored};
use crate::apply::Cut;
use crate::format::Sealed;
use crate::patch::{Body, Declaration, Declarations, IntoPatch, PluginHeld, Sealed as PatchSealed};
use crate::{AnyAssign, AppKey, Clock, Graph, Session};

/// Why a [`PluginDecl`](crate::PluginDecl) refuses a patch. Nothing is changed,
/// and the plugin is not taken: it is declared again by a patch that is clean.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum PluginPatchError<K> {
    /// One loaded plugin is declared under two keys in one patch.
    #[error("one plugin is declared under both {first:?} and {second:?}")]
    DeclaredTwice {
        /// The key declared first.
        first: K,
        /// The key declared again with the same plugin.
        second: K,
    },
    /// A node took the plugin already, or its node was removed: a plugin is in
    /// one node for as long as it is open.
    #[error(
        "the plugin declared under {key:?} is spent, or its node is gone; load the plugin again"
    )]
    Spent {
        /// The key the plugin is declared under.
        key: K,
    },
    /// The plugin is lost: its server died before it had a node.
    #[error("the plugin declared under {key:?} is lost")]
    Lost {
        /// The key the plugin is declared under.
        key: K,
    },
    /// Another session loaded the plugin.
    #[error("the plugin declared under {key:?} was loaded by another session")]
    Stale {
        /// The key the plugin is declared under.
        key: K,
    },
    /// The plugin lists no parameter of this id.
    #[error("the plugin declared under {key:?} lists no parameter {id:?}")]
    NoSuchParam {
        /// The key the plugin is declared under.
        key: K,
        /// The declared parameter.
        id: ParamId,
    },
    /// The plugin lists the parameter read-only.
    #[error("the plugin declared under {key:?} lists parameter {id:?} read-only")]
    ParamReadOnly {
        /// The key the plugin is declared under.
        key: K,
        /// The declared parameter.
        id: ParamId,
    },
}

/// A declared plugin is part of a patch over an `f32` graph: a plugin format is
/// `f32` at its FFI.
impl<K> IntoPatch<K, Graph<f32>> for crate::PluginDecl<K> {
    fn add_to(self, patch: &mut Declarations<K, Graph<f32>>, _: PatchSealed) {
        patch.nodes.push(self.declaration());
    }
}

impl PluginHeld {
    /// The shape the node over this plugin has, from the ports the plugin
    /// reports: what a wire of the patch is checked against before the node
    /// exists.
    pub(super) fn shape(&self) -> Shape {
        self.loaded
            .shape()
            .expect("a plugin a patch adds is in its slot: the check refused a spent one")
    }

    /// Whether `other` declares this plugin: the same instance.
    pub(crate) fn is_plugin_of(&self, other: &Self) -> bool {
        self.loaded.is(&other.loaded)
    }

    /// Whether the plugin's record retired: no node holds it.
    pub(crate) fn is_closed(&self) -> bool {
        self.loaded.is_closed()
    }
}

/// Refuses a declared parameter the plugin does not list or lists read-only.
fn check_initials<K: Clone>(
    key: &K,
    held: &PluginHeld,
    listed: &[ParameterInfo],
) -> Result<(), PluginPatchError<K>> {
    for &(id, _) in &held.initials {
        match listed
            .iter()
            .find(|info| info.id() == id)
            .map(ParamAccess::of)
        {
            None => {
                return Err(PluginPatchError::NoSuchParam {
                    key: key.clone(),
                    id,
                });
            }
            Some(ParamAccess::ReadOnly) => {
                return Err(PluginPatchError::ParamReadOnly {
                    key: key.clone(),
                    id,
                });
            }
            Some(ParamAccess::Writable | ParamAccess::Unreported) => {}
        }
    }
    Ok(())
}

impl<K: AppKey, C: Clock> Session<K, C> {
    /// Refuses what the declared plugins cannot be made of, with nothing taken:
    /// one plugin under two keys, a plugin a node took or whose node is gone, a
    /// plugin that is lost or another session's, and a declared parameter the
    /// plugin does not list or lists read-only. A parameter list that has not
    /// been read is not checked here: the commit counts what it cannot write.
    pub(super) fn check_plugins(
        &self,
        nodes: &[(K, Declaration<Graph<C::Float>>, Kind)],
    ) -> Result<(), PatchError<K>> {
        let mut seen: Vec<(&PluginHeld, &K)> = Vec::new();
        for (key, declaration, kind) in nodes {
            let Body::Plugin(held) = &declaration.body else {
                continue;
            };
            if let Some((_, first)) = seen.iter().find(|(known, _)| known.is_plugin_of(held)) {
                return Err(PatchError::Plugin(PluginPatchError::DeclaredTwice {
                    first: (*first).clone(),
                    second: key.clone(),
                }));
            }
            seen.push((held, key));
            let listed = match kind {
                // The node stays: its record's list is the plugin's now.
                Kind::Values => self.declared.nodes.get(key).and_then(|stored| {
                    let hosted = self.graph.plugins.get(&stored.node)?;
                    hosted
                        .mirror
                        .is_ready()
                        .then(|| hosted.mirror.descriptors().to_vec())
                }),
                Kind::Add | Kind::Readd | Kind::Swap => {
                    let spent = || PatchError::Plugin(PluginPatchError::Spent { key: key.clone() });
                    if held.loaded.is_spent() {
                        return Err(spent());
                    }
                    if held.loaded.is_of_another(Cut::of(self)) {
                        return Err(PatchError::Plugin(PluginPatchError::Stale {
                            key: key.clone(),
                        }));
                    }
                    if held.loaded.is_lost() {
                        return Err(PatchError::Plugin(PluginPatchError::Lost {
                            key: key.clone(),
                        }));
                    }
                    held.loaded.listed()
                }
            };
            if let Some(listed) = listed {
                check_initials(key, held, &listed).map_err(PatchError::Plugin)?;
            }
        }
        Ok(())
    }

    /// Makes `key`'s node over a plugin match its declaration: installs the
    /// plugin through the session's one entry, swaps the node over to another
    /// plugin, or writes the initials that changed.
    pub(super) fn declare_plugin(
        &mut self,
        kept: &mut Kept<K, C::Float>,
        planned: PlannedNode<K, C::Float, Fate<Shape>>,
        patched: &mut Patched<K>,
        writes: &mut Vec<AnyAssign>,
    ) {
        let PlannedNode {
            key,
            declaration,
            plan: fate,
            ..
        } = planned;
        let held = declaration
            .body
            .plugin()
            .expect("a plugin is declared by a plugin declaration");
        match fate {
            Fate::Add(_) => {
                let node = <C::Float as Sealed>::install_declared(self, &held.loaded).erase();
                self.write_initials(node, &held.initials, writes);
                kept.nodes.insert(key, Stored { node, declaration });
            }
            Fate::Readd(_) => {
                // Taking out a node that is gone finds nothing.
                if let Some(old) = kept.nodes.get(&key) {
                    if let Some(parts) = kept.instruments.remove(&key) {
                        self.take_out_parts(&parts, old.declaration.fading);
                    }
                    self.take_out(old);
                }
                let node = <C::Float as Sealed>::install_declared(self, &held.loaded).erase();
                self.write_initials(node, &held.initials, writes);
                patched.readded.push(key.clone());
                kept.nodes.insert(key, Stored { node, declaration });
            }
            Fate::Swap(_) => {
                let old = kept.nodes.get_mut(&key).expect("a swap has its old node");
                match <C::Float as Sealed>::swap_declared(self, old.node, &held.loaded) {
                    Ok(()) => {
                        self.write_initials(old.node, &held.initials, writes);
                        patched.rebuilt.push(key);
                        old.declaration = declaration;
                    }
                    Err(refused) => patched.unconverged.push((key, refused)),
                }
            }
            Fate::Values => {
                let old = kept.nodes.get_mut(&key).expect("a kept key has its node");
                let before: BTreeMap<ParamId, Normalized> = old
                    .declaration
                    .body
                    .plugin()
                    .expect("a kept plugin node is declared by a plugin declaration")
                    .initials
                    .iter()
                    .copied()
                    .collect();
                let changed: Vec<_> = held
                    .initials
                    .iter()
                    .copied()
                    .filter(|(id, value)| before.get(id) != Some(value))
                    .collect();
                self.write_initials(old.node, &changed, writes);
                old.declaration = declaration;
            }
        }
    }

    /// Writes each of `initials` to the plugin at `node` through its record, which
    /// counts the ones the plugin no longer lists.
    fn write_initials(
        &self,
        node: NodeKey,
        initials: &[(ParamId, Normalized)],
        writes: &mut Vec<AnyAssign>,
    ) {
        if let Some(hosted) = self.graph.plugins.get(&node) {
            hosted.write_initials(node, initials, writes);
        }
    }
}
