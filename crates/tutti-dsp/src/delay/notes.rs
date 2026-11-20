//! [`NoteTimes`]: the delay times of a tempo-synced delay, as note durations.

use tutti_music::NoteDuration;

use super::routing::MAX_WIDTH;

/// What a tempo-synced delay's channels are delayed by: a [`NoteDuration`] each, or one for
/// all of them.
///
/// The time of a channel is its note's beats at the session's tempo, so it follows the tempo
/// by the delay's own time ramp. A construction constant, like [`ChannelTimes`](crate::ChannelTimes):
/// a delay of the layout built with these ([`DelayLayoutBuilder::synced`](crate::DelayLayoutBuilder::synced))
/// has its times from the notes alone, and its time control is inert. That the notes fit the
/// delay's width is the layout's to check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteTimes {
    count: usize,
    notes: [NoteDuration; MAX_WIDTH],
    linked: bool,
}

/// A refused set of note times.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NoteTimesError {
    /// No notes.
    #[error("a synced delay has a note for each channel: none given")]
    Empty,
    /// More notes than a bus has channels.
    #[error("{count} note times: a bus has at most {max} channels")]
    TooMany {
        /// What was given.
        count: usize,
        /// The most a bus holds.
        max: usize,
    },
    /// The notes are not one for each channel.
    #[error("{got} note times on a {width}-channel delay")]
    Length {
        /// The delay's width.
        width: usize,
        /// What was given.
        got: usize,
    },
}

impl NoteTimes {
    /// Every channel is delayed by `note`.
    #[must_use]
    pub const fn all(note: NoteDuration) -> Self {
        Self {
            count: 1,
            notes: [note; MAX_WIDTH],
            linked: true,
        }
    }

    /// The notes: one when every channel shares it, else one for each channel.
    #[must_use]
    pub fn notes(&self) -> &[NoteDuration] {
        &self.notes[..self.count]
    }

    /// Whether every channel is delayed by the one note.
    #[must_use]
    pub(crate) const fn is_linked(&self) -> bool {
        self.linked
    }

    /// The note of channel `c`.
    #[must_use]
    pub(crate) fn of_channel(&self, c: usize) -> NoteDuration {
        if self.linked {
            self.notes[0]
        } else {
            self.notes[c]
        }
    }
}

/// The channels are delayed by `notes`, channel 0 first.
impl TryFrom<&[NoteDuration]> for NoteTimes {
    type Error = NoteTimesError;

    /// # Errors
    ///
    /// [`NoteTimesError::Empty`] or [`TooMany`](NoteTimesError::TooMany).
    fn try_from(notes: &[NoteDuration]) -> Result<Self, NoteTimesError> {
        let Some(&first) = notes.first() else {
            return Err(NoteTimesError::Empty);
        };
        if notes.len() > MAX_WIDTH {
            return Err(NoteTimesError::TooMany {
                count: notes.len(),
                max: MAX_WIDTH,
            });
        }
        let mut all = [first; MAX_WIDTH];
        all[..notes.len()].copy_from_slice(notes);
        Ok(Self {
            count: notes.len(),
            notes: all,
            linked: false,
        })
    }
}
