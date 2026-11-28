//! A streamed config's one live reader, and the seats its nodes hold it from.
//!
//! A stream's ring has one reader, so one node at a time sounds a config's
//! voice. The config's [`Home`] keeps the reader while no node holds it. A node
//! built while it is home takes it ([`Seat::Owned`]). A node built while another
//! holds it waits for it ([`Seat::Waiting`]) over a [`Handoff`], renders silence,
//! and takes the reader at the first block after the holder gives it up
//! ([`Seat::receive`]). Every node returns the reader when it drops, on the
//! control side, so the reader is never lost: [`Seat::release`].

use std::sync::Arc;

use tutti_rt::{Handback, Handoff, Mutex, Rt, handoff};

use super::disk_voice::DiskVoice;
use super::memory_source::VoiceWindow;
use crate::SamplerFloat;

/// A node waiting for the reader: the control end that lends it.
struct Waiter<F: SamplerFloat> {
    id: u64,
    end: Handoff<DiskVoice<F>>,
    /// The reader was lent to this waiter: it is not asked again.
    served: bool,
}

/// The disk voice of a streamed config: its reader while no node holds it, a
/// template of it for a node that waits, and the nodes waiting, oldest first.
pub(crate) struct Home<F: SamplerFloat> {
    live: Option<DiskVoice<F>>,
    template: DiskVoice<F>,
    waiting: Vec<Waiter<F>>,
    next: u64,
    pub(crate) window: VoiceWindow,
}

/// How a streamed node holds its voice.
pub(crate) enum Seat<F: SamplerFloat> {
    /// A node with no stream: a memory voice, or a fork.
    Detached,
    /// The node's voice is the config's reader.
    Owned(Arc<Mutex<Home<F>>>),
    /// The node's voice is the template, until the holder gives the reader up.
    Waiting {
        rt: Rt<DiskVoice<F>>,
        id: u64,
        home: Arc<Mutex<Home<F>>>,
    },
    /// The node took the reader from its handoff: the node's voice is the
    /// reader, and the handoff holds the template.
    Received {
        rt: Rt<DiskVoice<F>>,
        id: u64,
        home: Arc<Mutex<Home<F>>>,
    },
}

impl<F: SamplerFloat> Home<F> {
    pub(crate) fn of(voice: DiskVoice<F>) -> Self {
        Self {
            window: voice.window(),
            template: voice.clone(),
            live: Some(voice),
            waiting: Vec::new(),
            next: 0,
        }
    }

    /// The voice a node is built over, and how the node holds it: the reader when
    /// it is home, else the template and a place in the queue for the reader.
    /// Control side.
    pub(crate) fn seat(home: &Arc<Mutex<Self>>) -> (DiskVoice<F>, Seat<F>) {
        let mut this = home.lock();
        if let Some(live) = this.live.take() {
            return (live, Seat::Owned(Arc::clone(home)));
        }
        let (end, rt) = handoff();
        let id = this.next;
        this.next += 1;
        this.waiting.push(Waiter {
            id,
            end,
            served: false,
        });
        let seat = Seat::Waiting {
            rt,
            id,
            home: Arc::clone(home),
        };
        (this.template.clone(), seat)
    }

    /// Gives the reader, flushed, to the oldest waiter that still has a node, or
    /// keeps it. A waiter whose node is gone is skipped: its drop withdraws it.
    fn give(&mut self, mut voice: DiskVoice<F>) {
        voice.flush();
        for waiter in self.waiting.iter_mut().filter(|w| !w.served) {
            if waiter.end.borrower_gone() {
                continue;
            }
            match waiter.end.lend(voice) {
                Ok(()) => {
                    waiter.served = true;
                    return;
                }
                Err(back) => voice = back,
            }
        }
        self.live = Some(voice);
    }

    /// Takes the waiter `id` out of the queue, with the reader it was lent and
    /// never took, if any.
    fn withdraw(&mut self, id: u64) -> Option<DiskVoice<F>> {
        let at = self.waiting.iter().position(|w| w.id == id)?;
        let mut waiter = self.waiting.remove(at);
        waiter
            .end
            .recall()
            .or_else(|| waiter.end.take().map(Handback::into_inner))
    }
}

impl<F: SamplerFloat> Seat<F> {
    /// Between blocks, on the audio thread: takes the reader if it was lent,
    /// swapping it into `disk` and keeping the template. Whether it did.
    /// Wait-free: a poll and a swap, nothing dropped.
    pub(crate) fn receive(&mut self, disk: &mut DiskVoice<F>) -> bool {
        let Self::Waiting { rt, .. } = self else {
            return false;
        };
        // A waiting seat's audio end holds no reader, so `poll` never runs `last`
        // on a held value and drops nothing on the audio thread.
        let Some(lent) = rt.poll(|_| {}) else {
            return false;
        };
        std::mem::swap(disk, lent);
        *self = match std::mem::replace(self, Self::Detached) {
            Self::Waiting { rt, id, home } => Self::Received { rt, id, home },
            other => other,
        };
        true
    }

    /// Returns the reader to the config's home: a holder gives it to the oldest
    /// waiter, a waiter leaves the queue (and passes on a reader it was lent and
    /// never took). `disk` is the node's voice. Control side, where a node drops.
    pub(crate) fn release(self, disk: &mut DiskVoice<F>) {
        match self {
            Self::Detached => {}
            Self::Owned(home) => {
                let template = disk.clone();
                let live = std::mem::replace(disk, template);
                home.lock().give(live);
            }
            Self::Waiting { rt, id, home } => {
                drop(rt);
                let mut home = home.lock();
                if let Some(lent) = home.withdraw(id) {
                    home.give(lent);
                }
            }
            Self::Received { mut rt, id, home } => {
                if let Some(template) = rt.held() {
                    std::mem::swap(disk, template);
                }
                let live = rt.take_held();
                drop(rt);
                let mut home = home.lock();
                let stray = home.withdraw(id);
                for voice in live.into_iter().chain(stray) {
                    home.give(voice);
                }
            }
        }
    }

    /// Whether the node holds the reader, for a stream's node; `None` without
    /// one.
    pub(crate) fn holds(&self) -> Option<bool> {
        match self {
            Self::Detached => None,
            Self::Owned(_) | Self::Received { .. } => Some(true),
            Self::Waiting { .. } => Some(false),
        }
    }
}
