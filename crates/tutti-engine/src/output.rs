//! The folds from the graph root to the device: built once on the control side,
//! lent to the audio thread.
//!
//! The root renders at the width of its plan (at most
//! [`MAX_CHANNELS`]) and the device
//! callback hands over a buffer of its own width. [`Fold`]
//! allocates when built, and a plan arrives on the audio thread (a commit
//! installed at a block's start), so the folds are built ahead of time for
//! every root the control side has published and every device width up to
//! [`DEFAULT_DEVICE_WIDTHS`]; a block picks the one for the root its plan
//! carries ([`Plan::root_id`](tutti_graph::Plan::root_id)) and the buffer it is
//! given. A plan and its folds therefore swap together, and no topology crosses
//! to the audio thread. A wider device is added on the control side
//! ([`Engine::support_output`](crate::Engine::support_output)).
//!
//! # Roots
//!
//! Each root generation ([`RootId`]) is published with the topology it was
//! declared with, before the commit that carries its id is sent
//! ([`OutputControl::publish_root`]). A root with no declaration is the
//! engine's order for the plan's width
//! ([`ChannelTopology::assumed`](tutti_types::ChannelTopology::assumed)), at
//! every width. A root's folds stay published until a plan of a later root has
//! rendered a block ([`OutputControl::reclaim`]): the root change is a cut at
//! the commit, so that block is where the older root's last plan ended.
//!
//! # What the device is
//!
//! A device states its channel count and nothing else (a backend has no channel
//! map), so it is [`Assumed`](Placement::Assumed) to be the engine's order for
//! its width where it has one (mono, stereo, quad, 5.1, 7.1, 7.1.4), and a 5.1
//! root plays folded on a stereo device and a 7.1 root plays on a 5.1 one by
//! position. A caller that knows the wiring states it
//! ([`OutputControl::state_device`]): the root folds by speaker into that
//! topology. Where either end has no named speaker (3, 5, 7, 9 to 11, 13 and
//! up, or a declaration of independent buses) channels go by index
//! ([`Fold::by_index`](tutti_buffers::Fold::by_index)): zero-filled when the
//! device is wider, and a pair that would drop a channel has no fold, so that
//! root renders a silent, counted block into that device. Such a pair is
//! refused where it is made (a declaration, a commit, a restart), and the
//! silent block is the last resort for a device swapped under the session.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tutti_buffers::{BufferError, Fold, FoldError};
use tutti_graph::{RootId, fold_to_topology};
use tutti_rt::ArcLend;
use tutti_types::{ChannelLayout, ChannelTopology, MAX_CHANNELS, Placement};

/// The widest device an engine can render into without being told
/// ([`Engine::support_output`](crate::Engine::support_output)).
pub(crate) const DEFAULT_DEVICE_WIDTHS: usize = 16;

/// What a device's channels are, and whether that was stated or guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceEntry {
    topology: ChannelTopology,
    placement: Placement,
}

/// One root generation's folds.
#[derive(Debug, Clone)]
struct RootFolds {
    id: RootId,
    /// What the root was declared with; `None` is the engine's order for the
    /// plan's width, at every width.
    topology: Option<ChannelTopology>,
    /// Index `d - 1` holds the folds into a device of `d` channels, `None` for
    /// a width not supported; inside, index `w - 1` folds a root of `w`.
    by_device: Vec<Option<Box<[Option<Fold>]>>>,
}

impl RootFolds {
    /// The fold of this root at `width` channels into `device`, or none: a
    /// declared root has the one width it was declared at, and a pair that
    /// cannot be placed has no fold.
    fn build(&self, width: usize, device: &DeviceEntry) -> Option<Fold> {
        let placed = match &self.topology {
            Some(root) if usize::from(root.width().count()) != width => return None,
            Some(root) => fold_to_topology(root, &device.topology),
            None => fold_to_topology(
                &ChannelTopology::assumed(
                    ChannelLayout::try_from(width).expect("a root width is within MAX_CHANNELS"),
                ),
                &device.topology,
            ),
        };
        placed.ok()
    }

    /// The folds of every root width this root has into `device`.
    fn column(&self, device: &DeviceEntry) -> Box<[Option<Fold>]> {
        (1..=MAX_CHANNELS).map(|w| self.build(w, device)).collect()
    }

    /// Whether a root of `width` channels is one this root can render.
    fn has_width(&self, width: usize) -> bool {
        self.topology
            .as_ref()
            .is_none_or(|root| usize::from(root.width().count()) == width)
    }
}

/// The folds from each published root, at each width it renders, to each
/// device width supported.
#[derive(Debug, Clone)]
pub(crate) struct OutputFolds {
    /// Index `d - 1` is the device of `d` channels, `None` for a width not
    /// supported.
    devices: Vec<Option<DeviceEntry>>,
    /// Every root still published, by ascending id.
    roots: Vec<RootFolds>,
}

impl OutputFolds {
    /// The folds into devices of `1..=widest` channels, for the undeclared
    /// first root. Control side: it allocates.
    pub(crate) fn up_to(widest: usize) -> Self {
        let mut folds = Self {
            devices: Vec::new(),
            roots: vec![RootFolds {
                id: RootId::GENESIS,
                topology: None,
                by_device: Vec::new(),
            }],
        };
        for device in 1..=widest {
            let device = ChannelLayout::try_from(device)
                .expect("a device width the engine holds ahead is small");
            let _ = folds.support(device);
        }
        folds
    }

    /// Whether the folds into a device of `device` channels are held.
    pub(crate) fn supports(&self, device: ChannelLayout) -> bool {
        self.has_device(usize::from(device.count()))
    }

    /// Adds a device of `device` channels, taken to be the engine's order for
    /// its width (a no-op if present).
    ///
    /// # Errors
    ///
    /// [`FoldError::Buffer`] (a zero width) for a device of no channels.
    pub(crate) fn support(&mut self, device: ChannelLayout) -> Result<(), FoldError> {
        let Some(slot) = usize::from(device.count()).checked_sub(1) else {
            return Err(BufferError::ZeroWidth.into());
        };
        if self.devices.get(slot).is_some_and(Option::is_some) {
            return Ok(());
        }
        self.set_device(
            DeviceEntry {
                topology: ChannelTopology::assumed(device),
                placement: Placement::Assumed,
            },
            slot,
        );
        Ok(())
    }

    /// Sets what the device of `slot + 1` channels is, and rebuilds its folds
    /// from every root published. A pair that cannot be placed has no fold:
    /// that root width renders a silent, counted block into this device.
    fn set_device(&mut self, device: DeviceEntry, slot: usize) {
        if self.devices.len() <= slot {
            self.devices.resize(slot + 1, None);
        }
        for root in &mut self.roots {
            if root.by_device.len() <= slot {
                root.by_device.resize(slot + 1, None);
            }
            root.by_device[slot] = Some(root.column(&device));
        }
        self.devices[slot] = Some(device);
    }

    /// The fold of a root of `width` channels, of the root generation `root`,
    /// into a device of `device` channels, or `None` for a root not published
    /// (or retired), a root of no channels (or more than the engine renders), a
    /// device not supported, or a pair that cannot be placed.
    pub(crate) fn get(&self, root: RootId, device: usize, width: usize) -> Option<&Fold> {
        let root = self.roots.iter().find(|r| r.id == root)?;
        root.by_device
            .get(device.checked_sub(1)?)?
            .as_ref()?
            .get(width.checked_sub(1)?)?
            .as_ref()
    }

    /// Whether a device of `device` channels is held (its pairs may still be
    /// unplaceable).
    pub(crate) fn has_device(&self, device: usize) -> bool {
        device
            .checked_sub(1)
            .and_then(|slot| self.devices.get(slot))
            .is_some_and(Option::is_some)
    }
}

/// What a device's channels are, as the engine folds into it
/// ([`OutputControl::device`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPlacement {
    /// The device's channels.
    pub topology: ChannelTopology,
    /// Whether the caller stated it, or the engine took the width's order.
    pub placement: Placement,
}

/// The control side of the engine's output folds: what the session keeps when
/// the [`Engine`](crate::Engine) goes to the audio thread, to say what each
/// graph root is, what each device is, and which devices it can take.
///
/// Cloneable; every clone publishes into the one cell the engine lends from.
/// A publish is a read-modify-write, so clones serialize on a lock (control
/// side only; the audio thread never touches it).
#[derive(Debug, Clone)]
pub struct OutputControl {
    cell: Arc<ArcLend<OutputFolds>>,
    writer: Arc<Mutex<()>>,
    /// The root of the plan the last block rendered, written by the audio
    /// thread: the older roots' folds are done with.
    installed: Arc<AtomicU64>,
}

impl OutputControl {
    pub(crate) fn new(folds: OutputFolds) -> Self {
        Self {
            cell: Arc::new(ArcLend::new(folds)),
            writer: Arc::default(),
            installed: Arc::default(),
        }
    }

    pub(crate) fn cell(&self) -> &Arc<ArcLend<OutputFolds>> {
        &self.cell
    }

    pub(crate) fn installed(&self) -> &Arc<AtomicU64> {
        &self.installed
    }

    /// Publishes the current set as `edit` leaves it, unless `edit` finds
    /// nothing to do (`Ok(false)`). Nothing is published on an error.
    fn update(
        &self,
        edit: impl FnOnce(&mut OutputFolds) -> Result<bool, FoldError>,
    ) -> Result<(), FoldError> {
        let _writer = self.writer.lock().unwrap_or_else(PoisonError::into_inner);
        let mut folds = (*self.cell.current()).clone();
        if edit(&mut folds)? {
            self.cell.publish(folds);
            self.cell.reclaim();
        }
        Ok(())
    }

    /// As [`Engine::support_output`](crate::Engine::support_output).
    ///
    /// # Errors
    ///
    /// [`FoldError::Buffer`] (a zero width) for a device of no channels.
    pub fn support(&self, device: ChannelLayout) -> Result<(), FoldError> {
        if self.cell.current().supports(device) {
            return Ok(());
        }
        self.update(|folds| folds.support(device).map(|()| true))
    }

    /// States what a device's channels are: the roots fold by speaker into
    /// `topology` instead of into the engine's order for its width, for the
    /// device of that many channels. `None` takes the statement back, and the
    /// device is the engine's order for its width again.
    ///
    /// A device of `topology`'s width must have been built for first
    /// ([`support`](Self::support)) or is added here.
    ///
    /// Control side: it allocates, and publishes the new set for the next
    /// block. A callback mid-block keeps the folds it lent. A root that
    /// cannot be placed on the statement has no fold into the device, and
    /// [`has_fold`](Self::has_fold) says so.
    ///
    /// # Errors
    ///
    /// [`FoldError::Buffer`] for a topology of no channels (nothing is
    /// published).
    pub fn state_device(
        &self,
        device: ChannelLayout,
        topology: Option<ChannelTopology>,
    ) -> Result<(), FoldError> {
        let Some(slot) = usize::from(device.count()).checked_sub(1) else {
            return Err(BufferError::ZeroWidth.into());
        };
        let entry = match topology {
            Some(topology) if topology.width() != device => {
                return Err(BufferError::WidthMismatch {
                    src: topology.width(),
                    dst: device,
                }
                .into());
            }
            Some(topology) => DeviceEntry {
                topology,
                placement: Placement::Stated,
            },
            None => DeviceEntry {
                topology: ChannelTopology::assumed(device),
                placement: Placement::Assumed,
            },
        };
        self.update(|folds| {
            if folds.devices.get(slot).and_then(Option::as_ref) == Some(&entry) {
                return Ok(false);
            }
            folds.set_device(entry, slot);
            Ok(true)
        })
    }

    /// What the device of `device` channels is as the engine folds into it:
    /// its topology and whether it was stated. `None` for a width not held.
    #[must_use]
    pub fn device(&self, device: ChannelLayout) -> Option<OutputPlacement> {
        let slot = usize::from(device.count()).checked_sub(1)?;
        let folds = self.cell.current();
        let entry = folds.devices.get(slot)?.as_ref()?;
        Some(OutputPlacement {
            topology: entry.topology.clone(),
            placement: entry.placement,
        })
    }

    /// Publishes the root generation `id`, declared as `root` (`None` is the
    /// engine's order for the plan's width), before the commit that carries
    /// `id` is sent: a plan of that root finds its folds on its first block.
    /// The older roots' folds stay until [`reclaim`](Self::reclaim) finds a
    /// plan of a later root has rendered.
    ///
    /// A declaration of independent buses
    /// ([`ChannelTopology::unassigned`]) stays by index: a 4-wide one into a
    /// 6-wide device keeps channels 2 and 3 at 2 and 3 (undeclared, a 4-wide
    /// root is quad, and they land on the side pair).
    ///
    /// Control side: it allocates, and publishes the new set for the next
    /// block. A callback mid-block keeps the folds it lent.
    ///
    /// # Errors
    ///
    /// The [`FoldError`] when the declaration cannot be placed on `device`
    /// (an unassigned root wider than it, or an unnamed channel); nothing is
    /// published. A device the engine holds that cannot take it has no fold
    /// ([`has_fold`](Self::has_fold)).
    ///
    /// # Panics
    ///
    /// When `id` is not past every root published: a root's id is minted in
    /// order by the one graph that declares it.
    pub fn publish_root(
        &self,
        id: RootId,
        root: Option<ChannelTopology>,
        device: ChannelLayout,
    ) -> Result<(), FoldError> {
        if let Some(root) = &root {
            let placed = self.device(device).map(|d| d.topology);
            fold_to_topology(
                root,
                &placed.unwrap_or_else(|| ChannelTopology::assumed(device)),
            )?;
        }
        self.update(|folds| {
            assert!(
                folds.roots.last().is_none_or(|last| last.id < id),
                "root {id} is not past the published roots"
            );
            let mut entry = RootFolds {
                id,
                topology: root,
                by_device: Vec::new(),
            };
            entry.by_device = folds
                .devices
                .iter()
                .map(|d| d.as_ref().map(|d| entry.column(d)))
                .collect();
            folds.roots.push(entry);
            Ok(true)
        })
    }

    /// Whether the root generation `root` has a fold at `width` channels into
    /// a device of `device` channels: the pair a commit of that width, or a
    /// restart onto that device, must be able to place.
    #[must_use]
    pub fn has_fold(&self, root: RootId, width: usize, device: ChannelLayout) -> bool {
        self.cell
            .current()
            .get(root, usize::from(device.count()), width)
            .is_some()
    }

    /// Whether a root of `width` channels is one the generation `root` can
    /// render at all, whichever device (a declared root has the one width).
    #[must_use]
    pub fn has_width(&self, root: RootId, width: usize) -> bool {
        self.cell
            .current()
            .roots
            .iter()
            .find(|r| r.id == root)
            .is_some_and(|r| r.has_width(width))
    }

    /// As [`Engine::reclaim`](crate::Engine::reclaim): first the roots a plan
    /// of a later root has rendered after are dropped from the published set,
    /// then what that and every earlier publish retired is freed once no
    /// block holds it.
    pub fn reclaim(&self) -> usize {
        let installed = self.installed.load(Ordering::Acquire);
        let _ = self.update(|folds| {
            let before = folds.roots.len();
            folds.roots.retain(|r| r.id.get() >= installed);
            Ok(folds.roots.len() != before)
        });
        self.cell.reclaim()
    }
}
