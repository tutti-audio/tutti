//! The folds from the graph's main output to the device: built once on the control side,
//! lent to the audio thread.
//!
//! The main output renders at the width of its plan (at most
//! [`MAX_CHANNELS`]) and the device
//! callback hands over a buffer of its own width. [`Fold`]
//! allocates when built, and a plan arrives on the audio thread (a commit
//! installed at a block's start), so the folds are built ahead of time for
//! every main output the control side has published and every device width up to
//! [`DEFAULT_DEVICE_WIDTHS`]; a block picks the one for the main output its plan
//! carries ([`Plan::main_output_id`](tutti_graph::Plan::main_output_id)) and the buffer it is
//! given. A plan and its folds therefore swap together, and no topology crosses
//! to the audio thread. A wider device is added on the control side
//! ([`Engine::support_output`](crate::Engine::support_output)).
//!
//! # Main outputs
//!
//! Each main output generation ([`MainOutputId`]) is published with the topology it was
//! declared with, before the commit that carries its id is sent
//! ([`OutputControl::publish_main_output`]). A main output with no declaration is the
//! engine's order for the plan's width
//! ([`ChannelTopology::default_for`](tutti_buffers::ChannelTopology::default_for)), at
//! every width. A main output's folds stay published until a plan of a later main output has
//! rendered a block ([`OutputControl::reclaim`]). A main output change that cuts ends
//! the older main output at the commit's block. One that crossfades
//! ([`Editor::set_main_output_fade`](tutti_graph::Editor::set_main_output_fade)) keeps the
//! older main output's folds until the block that ends the fade, since that block
//! still folds the old main output's outputs. At most
//! [`MAX_PENDING_MAIN_OUTPUTS`] are published ahead of the plan in force, and the
//! editor refuses a commit whose main output was never published
//! ([`CommitError::MainOutputNotPublished`](tutti_graph::CommitError::MainOutputNotPublished)).
//!
//! # A routing change
//!
//! A change of the routing ([`OutputControl::set_routing`]) or of what a
//! device is ([`OutputControl::state_device`]) replaces folds in place, and the
//! table it replaced stays published beside the new one while the audio thread
//! crossfades from it: the same rendered channels folded the old way and the
//! new way, mixed by [`FadeLaw::EqualAmplitude`](tutti_graph::FadeLaw::EqualAmplitude)
//! over the declick length. The audio thread counts the frames since it first
//! lent a table of the new generation, reports the fade's end in an atomic, and
//! [`OutputControl::reclaim`] drops the replaced table through the cell; the
//! audio thread never holds or frees it. A change that lands while a main output
//! crossfade runs, or while the last routing fade has not ended, cuts: it holds
//! no third table.
//!
//! # What the device is
//!
//! A device states its channel count and nothing else (a backend has no channel
//! map), so it takes the default layout for
//! its width where it has one (mono, stereo, quad, 5.1, 7.1, 7.1.4), and a 5.1
//! main output plays folded on a stereo device and a 7.1 main output plays on a 5.1 one by
//! position. A caller that knows the wiring states it
//! ([`OutputControl::state_device`]): the main output folds by speaker into that
//! topology. Where either end has no named speaker (3, 5, 7, 9 to 11, 13 and
//! up, or a declaration of independent buses) channels go by index
//! ([`Fold::by_index`](tutti_buffers::Fold::by_index)): zero-filled when the
//! device is wider, and a pair that would drop a channel has no fold, so that
//! main output renders a silent, counted block into that device. Such a pair is
//! refused where it is made (a declaration, a commit, a restart), and the
//! silent block is the last resort for a device swapped under the session.

use crate::routing::{OutputLevels, Routing, RoutingError, StageTargets};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tutti_buffers::{BufferError, Float, Fold, FoldError};
use tutti_buffers::{ChannelLayout, ChannelTopology, MAX_CHANNELS, Placement};
use tutti_graph::{MainOutputError, MainOutputId};
use tutti_rt::{LockExt, Publisher};

/// The widest device an engine can render into without being told
/// ([`Engine::support_output`](crate::Engine::support_output)).
pub(crate) const DEFAULT_DEVICE_WIDTHS: usize = 16;

/// The most main output generations that may be published and not yet rendered: a
/// stream that is not running never advances the rendered main output, so without a
/// bound each publish would clone a longer table. Past it
/// [`OutputControl::publish_main_output`] refuses with [`MainOutputError::TooManyPending`].
pub const MAX_PENDING_MAIN_OUTPUTS: usize = 8;

/// What a device's channels are, and whether that was stated or guessed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeviceEntry {
    topology: ChannelTopology,
    placement: Placement,
}

/// One main output generation's folds.
#[derive(Debug, Clone)]
struct MainOutputFolds<F: Float> {
    id: MainOutputId,
    /// What the main output was declared with; `None` is the engine's order for the
    /// plan's width, at every width.
    topology: Option<ChannelTopology>,
    /// Index `d - 1` holds the folds into a device of `d` channels, `None` for
    /// a width not supported; inside, index `w - 1` folds a main output of `w`.
    by_device: Vec<Option<Box<[Option<Fold<F>>]>>>,
}

impl<F: Float> MainOutputFolds<F> {
    /// The fold of this main output at `width` channels into `device`: `None` for a
    /// width a declared main output does not have (it renders at the one it was
    /// declared at), and the routing's refusal for a pair that cannot be placed.
    fn build(
        &self,
        width: usize,
        device: &DeviceEntry,
        routing: &Routing,
    ) -> Result<Option<Fold<F>>, RoutingError> {
        let default;
        let main_output = match &self.topology {
            Some(main_output) if usize::from(main_output.width().count()) != width => {
                return Ok(None);
            }
            Some(main_output) => main_output,
            None => {
                default = ChannelTopology::default_for(
                    ChannelLayout::try_from(width)
                        .expect("a main output width is within MAX_CHANNELS"),
                );
                &default
            }
        };
        routing.fold(main_output, &device.topology).map(Some)
    }

    /// The folds of every main output width this main output has into `device`. A width the
    /// routing cannot place has no fold, and renders a silent, counted block
    /// into the device; [`OutputControl::set_routing`] refuses before that for
    /// the pair in use.
    fn column(&self, device: &DeviceEntry, routing: &Routing) -> Box<[Option<Fold<F>>]> {
        (1..=MAX_CHANNELS)
            .map(|w| self.build(w, device, routing).unwrap_or_default())
            .collect()
    }

    /// Whether a main output of `width` channels is one this main output can render.
    fn has_width(&self, width: usize) -> bool {
        self.topology
            .as_ref()
            .is_none_or(|main_output| usize::from(main_output.width().count()) == width)
            && (1..=MAX_CHANNELS).contains(&width)
    }
}

/// The folds from each published main output, at each width it renders, to each
/// device width supported.
#[derive(Debug, Clone)]
pub(crate) struct OutputFolds<F: Float> {
    /// Index `d - 1` is the device of `d` channels, `None` for a width not
    /// supported.
    devices: Vec<Option<DeviceEntry>>,
    /// Every main output still published, by ascending id.
    main_outputs: Vec<MainOutputFolds<F>>,
    /// Where the mix lands: composed into every main output's folds.
    routing: Routing,
    /// Counts the changes that replace folds in place (a routing, a device's
    /// statement); a change of it starts a crossfade on the audio thread.
    generation: u64,
    /// The table the last such change replaced, while the audio thread
    /// crossfades from it; `None` is a cut. It holds no `previous` of its own.
    previous: Option<Arc<OutputFolds<F>>>,
}

impl<F: Float> OutputFolds<F> {
    /// The folds into devices of `1..=widest` channels, for the undeclared
    /// first main output. Control side: it allocates.
    pub(crate) fn up_to(widest: usize) -> Self {
        let mut folds = Self {
            devices: Vec::new(),
            main_outputs: vec![MainOutputFolds {
                id: MainOutputId::GENESIS,
                topology: None,
                by_device: Vec::new(),
            }],
            routing: Routing::default(),
            generation: 0,
            previous: None,
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
                topology: ChannelTopology::default_for(device),
                placement: Placement::Default,
            },
            slot,
        );
        Ok(())
    }

    /// Sets what the device of `slot + 1` channels is, and rebuilds its folds
    /// from every main output published. A pair that cannot be placed has no fold:
    /// that main output width renders a silent, counted block into this device.
    fn set_device(&mut self, device: DeviceEntry, slot: usize) {
        if self.devices.len() <= slot {
            self.devices.resize(slot + 1, None);
        }
        for main_output in &mut self.main_outputs {
            if main_output.by_device.len() <= slot {
                main_output.by_device.resize(slot + 1, None);
            }
            main_output.by_device[slot] = Some(main_output.column(&device, &self.routing));
        }
        self.devices[slot] = Some(device);
    }

    /// Starts a routing generation: the table as it stands becomes
    /// [`previous`](Self::previous) for the audio thread to fade from, unless
    /// `in_flight` (the fade of the last generation has not ended), which cuts.
    /// Call before the edit.
    fn begin_fade(&mut self, in_flight: bool) {
        self.previous = None;
        let replaced = (!in_flight).then(|| Arc::new(self.clone()));
        self.generation += 1;
        self.previous = replaced;
    }

    /// The count of in-place fold changes: the audio thread fades when it
    /// differs from the one it last rendered.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The table the last in-place change replaced, while it is crossfaded from.
    pub(crate) fn previous(&self) -> Option<&Arc<OutputFolds<F>>> {
        self.previous.as_ref()
    }

    /// Whether every main output can be placed on the device of `device` channels
    /// under `routing`, a main output of no declared width at `width` channels.
    ///
    /// # Errors
    ///
    /// The first [`RoutingError`] a main output's fold into the device meets.
    fn place(&self, routing: &Routing, width: usize, device: usize) -> Result<(), RoutingError> {
        let Some(entry) = device
            .checked_sub(1)
            .and_then(|slot| self.devices.get(slot))
            .and_then(Option::as_ref)
        else {
            return Ok(());
        };
        for main_output in &self.main_outputs {
            let width = main_output
                .topology
                .as_ref()
                .map_or(width, |t| usize::from(t.width().count()));
            main_output.build(width, entry, routing)?;
        }
        Ok(())
    }

    /// Rebuilds every main output's folds into every device held, from `routing`.
    fn set_routing(&mut self, routing: Routing) {
        self.routing = routing;
        for main_output in &mut self.main_outputs {
            main_output.by_device = self
                .devices
                .iter()
                .map(|d| d.as_ref().map(|d| main_output.column(d, &self.routing)))
                .collect();
        }
    }

    /// The fold of a main output of `width` channels, of the main output generation `main_output`,
    /// into a device of `device` channels, or `None` for a main output not published
    /// (or retired), a main output of no channels (or more than the engine renders), a
    /// device not supported, or a pair that cannot be placed.
    pub(crate) fn get(
        &self,
        main_output: MainOutputId,
        device: usize,
        width: usize,
    ) -> Option<&Fold<F>> {
        let main_output = self.main_outputs.iter().find(|r| r.id == main_output)?;
        main_output
            .by_device
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
/// graph's main output is, what each device is, and which devices it can take.
///
/// Cloneable; every clone publishes into the one cell the engine lends from.
/// A publish is a read-modify-write, so clones serialize on a lock (control
/// side only; the audio thread never touches it).
#[derive(Debug, Clone)]
pub struct OutputControl<F: Float = f32> {
    cell: Publisher<OutputFolds<F>>,
    /// The gain each device output is ramped to, lent beside the folds.
    stage: Publisher<StageTargets>,
    writer: Arc<Mutex<()>>,
    /// The main output of the plan the last block rendered, written by the audio
    /// thread: the older main outputs' folds are done with.
    installed: Arc<AtomicU64>,
    /// The newest routing generation whose crossfade the audio thread has
    /// finished or cut: its `previous` table is no longer read.
    routing_ended: Arc<AtomicU64>,
    /// The main output width and the device width of the last block rendered, packed
    /// by [`pack_in_use`]; `0` before one has. Written by the audio thread and
    /// cleared on the control side wherever the device in use changes
    /// ([`state_device`](Self::state_device) onto another width, a new
    /// [`OutputBlock`](crate::OutputBlock)). A late write from the last
    /// callback of a stream being replaced can put the old pair back, until
    /// that stream stops.
    in_use: Arc<AtomicU64>,
}

/// The widths of the pair a block renders, in one word: the main output's width above
/// the device's. Both are at least one in a block that renders a main output.
pub(crate) fn pack_in_use(width: usize, device: usize) -> u64 {
    ((width as u64) << 32) | device as u64
}

impl<F: Float> OutputControl<F> {
    pub(crate) fn new(folds: OutputFolds<F>) -> Self {
        Self {
            cell: Publisher::new(folds),
            stage: Publisher::new(StageTargets::UNITY),
            writer: Arc::default(),
            installed: Arc::default(),
            routing_ended: Arc::default(),
            in_use: Arc::default(),
        }
    }

    pub(crate) fn in_use_cell(&self) -> &Arc<AtomicU64> {
        &self.in_use
    }

    /// Forgets the pair in use: the device is about to change, and the next
    /// block renders the pair that counts.
    pub(crate) fn forget_in_use(&self) {
        self.in_use.store(0, Ordering::Relaxed);
    }

    /// The widths of the (main output, device) pair the last block rendered, once one
    /// has: the main output's, then the device's.
    fn in_use(&self) -> Option<(usize, usize)> {
        let packed = self.in_use.load(Ordering::Relaxed);
        (packed != 0).then(|| {
            (
                (packed >> 32) as usize,
                (packed & u64::from(u32::MAX)) as usize,
            )
        })
    }

    pub(crate) fn routing_ended(&self) -> &Arc<AtomicU64> {
        &self.routing_ended
    }

    /// Whether the audio thread may still be fading from `folds.previous`.
    fn fade_in_flight(&self, folds: &OutputFolds<F>) -> bool {
        folds.previous.is_some() && self.routing_ended.load(Ordering::Acquire) < folds.generation
    }

    pub(crate) fn cell(&self) -> &Publisher<OutputFolds<F>> {
        &self.cell
    }

    pub(crate) fn stage(&self) -> &Publisher<StageTargets> {
        &self.stage
    }

    pub(crate) fn installed(&self) -> &Arc<AtomicU64> {
        &self.installed
    }

    /// Publishes the current set as `edit` leaves it, unless `edit` finds
    /// nothing to do (`Ok(false)`). Nothing is published on an error.
    fn update<E>(
        &self,
        edit: impl FnOnce(&mut OutputFolds<F>) -> Result<bool, E>,
    ) -> Result<(), E> {
        let _writer = self.writer.lock_recover();
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

    /// States what a device's channels are: the main outputs fold by speaker into
    /// `topology` instead of into the engine's order for its width, for the
    /// device of that many channels. `None` takes the statement back, and the
    /// device is the engine's order for its width again.
    ///
    /// Replacing a device already held crossfades like
    /// [`set_routing`](Self::set_routing): the audio thread mixes from the old
    /// folds to the new over the declick length, and the same cuts apply (a
    /// main output fade, a fade still running, no old fold; see the module's "A
    /// routing change").
    ///
    /// A device of `topology`'s width must have been built for first
    /// ([`support`](Self::support)) or is added here.
    ///
    /// Control side: it allocates, and publishes the new set for the next
    /// block. A callback mid-block keeps the folds it lent. A main output that
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
                topology: ChannelTopology::default_for(device),
                placement: Placement::Default,
            },
        };
        if self.in_use().is_some_and(|(_, d)| d != slot + 1) {
            self.forget_in_use();
        }
        self.update(|folds| {
            if folds.devices.get(slot).and_then(Option::as_ref) == Some(&entry) {
                return Ok(false);
            }
            if folds.has_device(slot + 1) {
                let in_flight = self.fade_in_flight(folds);
                folds.begin_fade(in_flight);
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

    /// Publishes the main output generation `id`, declared as `main_output` (`None` is the
    /// engine's order for the plan's width), before the commit that carries
    /// `id` is sent: a plan of that main output finds its folds on its first block.
    /// The older main outputs' folds stay until [`reclaim`](Self::reclaim) finds a
    /// plan of a later main output has rendered.
    ///
    /// A declaration of independent buses
    /// ([`ChannelTopology::unassigned`]) stays by index: a 4-wide one into a
    /// 6-wide device keeps channels 2 and 3 at 2 and 3 (undeclared, a 4-wide
    /// main output is quad, and they land on the side pair).
    ///
    /// Control side: it allocates, and publishes the new set for the next
    /// block. A callback mid-block keeps the folds it lent.
    ///
    /// # Errors
    ///
    /// Nothing is published on an error.
    ///
    /// - [`MainOutputError::Fold`] when the declaration cannot be placed on `device`
    ///   (an unassigned main output wider than it, or an unnamed channel). A device
    ///   the engine holds that cannot take it has no fold
    ///   ([`has_fold`](Self::has_fold)).
    /// - [`MainOutputError::NotPast`] when `id` is not past every main output published.
    /// - [`MainOutputError::TooManyPending`] when [`MAX_PENDING_MAIN_OUTPUTS`] main outputs are
    ///   published and none a plan has rendered: a stream that is not running
    ///   advances no main output, so the table is bounded rather than grown.
    pub fn publish_main_output(
        &self,
        id: MainOutputId,
        main_output: Option<ChannelTopology>,
        device: ChannelLayout,
    ) -> Result<(), MainOutputError> {
        if let Some(main_output) = &main_output {
            let placed = self
                .device(device)
                .map_or_else(|| ChannelTopology::default_for(device), |d| d.topology);
            // A refusal of the fold itself is the call's; a map that does not
            // fit leaves the pair with no fold, which a commit names.
            if let Err(RoutingError::Fold(refused)) =
                self.cell.current().routing.fold::<F>(main_output, &placed)
            {
                return Err(refused.into());
            }
        }
        let installed = self.installed.load(Ordering::Acquire);
        self.update(|folds| {
            if let Some(last) = folds
                .main_outputs
                .last()
                .map(|r| r.id)
                .filter(|last| *last >= id)
            {
                return Err(MainOutputError::NotPast { id, last });
            }
            let pending = folds
                .main_outputs
                .iter()
                .filter(|r| r.id.get() > installed)
                .count();
            if pending >= MAX_PENDING_MAIN_OUTPUTS {
                return Err(MainOutputError::TooManyPending {
                    limit: MAX_PENDING_MAIN_OUTPUTS,
                });
            }
            let mut entry = MainOutputFolds {
                id,
                topology: main_output,
                by_device: Vec::new(),
            };
            entry.by_device = folds
                .devices
                .iter()
                .map(|d| d.as_ref().map(|d| entry.column(d, &folds.routing)))
                .collect();
            folds.main_outputs.push(entry);
            Ok(true)
        })
    }

    /// Whether the main output generation `main_output` has a fold at `width` channels into
    /// a device of `device` channels: the pair a commit of that width, or a
    /// restart onto that device, must be able to place.
    #[must_use]
    pub fn has_fold(&self, main_output: MainOutputId, width: usize, device: ChannelLayout) -> bool {
        self.cell
            .current()
            .get(main_output, usize::from(device.count()), width)
            .is_some()
    }

    /// Whether a main output of `width` channels is one the generation `main_output` can
    /// render at all, whichever device (a declared main output has the one width).
    #[must_use]
    pub fn has_width(&self, main_output: MainOutputId, width: usize) -> bool {
        self.cell
            .current()
            .main_outputs
            .iter()
            .find(|r| r.id == main_output)
            .is_some_and(|r| r.has_width(width))
    }

    /// Sets where the mix lands: the format the main output is heard in and the map
    /// to the device, composed into every main output's folds into every device held
    /// (one fold per pair, so the callback still applies one). Live: from the
    /// next segment the callback crossfades the same rendered channels from the
    /// old routing's folds to the new ones over the declick length
    /// ([`declick_frames`](tutti_graph::declick_frames)), with no restart.
    ///
    /// The change cuts instead, folding by the new routing at once, when a
    /// main output change is crossfading, when the last routing change (or device
    /// statement) is still fading, when the old routing has no fold for the
    /// main output, or when no block has rendered yet. A main output a routing cannot place on
    /// a device the stream does not render into has no fold into it
    /// ([`has_fold`](Self::has_fold)).
    ///
    /// Control side: it allocates and publishes. A callback mid-block keeps the
    /// folds it lent.
    ///
    /// # Errors
    ///
    /// The [`RoutingError`] of the first main output the routing cannot place on the
    /// device the last block rendered into (a main output of no declared width at the
    /// width that block rendered); nothing is published. Before a block has
    /// rendered no device is in use and every routing is taken.
    pub fn set_routing(&self, routing: Routing) -> Result<(), RoutingError> {
        let in_use = self.in_use();
        self.update(|folds| {
            if folds.routing == routing {
                return Ok(false);
            }
            if let Some((width, device)) = in_use {
                folds.place(&routing, width, device)?;
            }
            let in_flight = self.fade_in_flight(folds);
            folds.begin_fade(in_flight);
            folds.set_routing(routing);
            Ok(true)
        })
    }

    /// The routing in force.
    #[must_use]
    pub fn routing(&self) -> Routing {
        self.cell.current().routing.clone()
    }

    /// Sets the gain of each device output from `levels`: the callback ramps
    /// each output to its new gain over the declick length ([`declick_frames`](tutti_graph::declick_frames)), so a
    /// change never clicks and no fold is swapped. Control side.
    pub fn set_levels(&self, levels: &OutputLevels) {
        let targets = levels.targets();
        let _writer = self.writer.lock_recover();
        if *self.stage.current() != targets {
            self.stage.publish(targets);
            self.stage.reclaim();
        }
    }

    /// As [`Engine::reclaim`](crate::Engine::reclaim): first the main outputs a plan
    /// of a later main output has rendered after are dropped from the published set,
    /// then what that and every earlier publish retired is freed once no
    /// block holds it.
    pub fn reclaim(&self) -> usize {
        let installed = self.installed.load(Ordering::Acquire);
        // The table is cloned only when a main output drops; an idle tick reads.
        let retired = self
            .cell
            .current()
            .main_outputs
            .iter()
            .any(|r| r.id.get() < installed);
        if retired {
            let _ = self.update(|folds| {
                let before = folds.main_outputs.len();
                folds.main_outputs.retain(|r| r.id.get() >= installed);
                Ok::<_, std::convert::Infallible>(folds.main_outputs.len() != before)
            });
        }
        // A crossfade the audio thread has finished: its `previous` goes
        // with the table that held it, freed here once no block lends it.
        let ended = self.routing_ended.load(Ordering::Acquire);
        let current = self.cell.current();
        let finished = current.previous.is_some() && ended >= current.generation;
        drop(current);
        if finished {
            let _ = self.update(|folds| {
                let finished = folds.previous.is_some() && ended >= folds.generation;
                folds.previous = None;
                Ok::<_, std::convert::Infallible>(finished)
            });
        }
        self.cell.reclaim() + self.stage.reclaim()
    }
}

impl<F: Float> tutti_graph::MainOutputGate for OutputControl<F> {
    fn published(&self, main_output: MainOutputId, outputs: usize) -> bool {
        self.has_width(main_output, outputs)
    }
}
