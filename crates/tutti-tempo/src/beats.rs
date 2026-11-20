use crate::error::{FromBeatsError, TempoError};
use crate::map::TempoMap;
use alloc::vec::Vec;
use tutti_units::{Beat, Bpm, BpmOutOfRange, SampleRate, Samples};

impl TempoMap {
    /// The map whose beat `n` falls on the `n`-th of `beats`, the positions in
    /// frames a beat tracker reports.
    ///
    /// Beat 0 is the first position, so the map's time zero is the first beat:
    /// a frame count from the start of the analysed audio is a frame count from
    /// the first beat minus `beats[0]`. Each interval between consecutive
    /// beats is one constant-tempo segment of `60 * rate / interval` BPM, a
    /// step at its first beat (no ramps), and the last interval's tempo holds
    /// from the last beat on.
    ///
    /// ```
    /// use tutti_tempo::TempoMap;
    /// use tutti_units::{Beat, Bpm, SampleRate, Samples};
    ///
    /// // A beat every 24 000 frames at 48 kHz is 120 BPM.
    /// let beats = [0, 24_000, 48_000].map(Samples::new);
    /// let map = TempoMap::from_beats(&beats, SampleRate::SR_48K)?;
    /// assert_eq!(map.tempo_at(Beat::new(1.0)), Bpm::of(120.0));
    /// # Ok::<(), tutti_tempo::FromBeatsError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// [`FromBeatsError::TooFewBeats`] for fewer than two beats,
    /// [`FromBeatsError::NotIncreasing`] for a beat that is not after the
    /// previous one, [`FromBeatsError::Tempo`] for an interval whose tempo is
    /// not a [`Bpm`], and [`FromBeatsError::Map`] for what
    /// [`TempoMap::new`] refuses.
    pub fn from_beats(beats: &[Samples], rate: SampleRate) -> Result<Self, FromBeatsError> {
        if beats.len() < 2 {
            return Err(FromBeatsError::TooFewBeats(beats.len()));
        }
        let tempi = beats
            .windows(2)
            .enumerate()
            .map(|(i, pair)| {
                let interval = pair[1]
                    .get()
                    .checked_sub(pair[0].get())
                    .filter(|&frames| frames > 0)
                    .ok_or(FromBeatsError::NotIncreasing(i + 1))?;
                let bpm = 60.0 * rate.get() / interval as f64;
                Bpm::new(bpm).ok_or(FromBeatsError::Tempo(BpmOutOfRange(bpm)))
            })
            .collect::<Result<Vec<Bpm>, _>>()?;
        let mut tempi = tempi.into_iter();
        let first = tempi.next().ok_or(TempoError::Empty)?;
        let map = tempi
            .enumerate()
            .fold(Self::builder(first), |builder, (i, tempo)| {
                builder.step_to(Beat::new((i + 1) as f64), tempo)
            })
            .build()?;
        Ok(map)
    }
}
