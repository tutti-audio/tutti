//! Tests of the beat walk a node takes over its block's
//! [`Env`](tutti_graph::Env), `Env::for_each_beat`.

#[cfg(test)]
mod tests {
    use tutti_graph::{Env, TransportChanges, TransportPosition};
    use tutti_music::FrameClock;
    use tutti_units::Beat;
    use tutti_units::Frame;
    use tutti_units::{Bpm, SampleRate, Samples};

    const SR: SampleRate = SampleRate::SR_48K;

    /// The beats a block of `len` frames starting on `host`'s frame gets, in
    /// `f64`.
    fn walked(host: &FrameClock, len: usize, looping: Option<tutti_music::LoopRange>) -> Vec<Beat> {
        let env = Env {
            frame: Frame::new(0),
            sample_rate: SR,
            block_len: Samples::new(len),
            transport: TransportPosition::counted(true, host.tempo(), host.origin(), looping),
            changes: TransportChanges::NONE,
            play: tutti_graph::PlayClock::ZERO,
        };
        let mut out = vec![Beat::new(f64::NAN); len];
        env.for_each_beat(|i, b| out[i] = b);
        out
    }

    /// The walk continues the host's clock in `f64`, bit for bit, far into
    /// a segment at a tempo whose frame step is not representable, through
    /// loop wraps shorter than the block.
    #[test]
    fn the_env_walk_is_the_host_clock_in_f64() {
        for (tempo, looping) in [
            (97.0, None),
            (133.3, None),
            (
                97.0,
                tutti_music::LoopRange::new(Beat::new(1_000.0), Beat::new(1_000.004)),
            ),
        ] {
            let mut host = FrameClock::new(Beat::new(0.25), Bpm::of(tempo), SR);
            host.advance(Samples::new(987_654_321), None);
            if let Some(l) = looping {
                host.seat(Beat::new(1_000.0));
                host.advance(Samples::new(3), Some(l));
            }
            let got = walked(&host, 512, looping);
            for (i, b) in got.iter().enumerate() {
                assert_eq!(
                    b.get().to_bits(),
                    host.beat().get().to_bits(),
                    "{tempo} BPM, loop {looping:?}: frame {i}"
                );
                host.advance(Samples::new(1), looping);
            }
        }
    }
}
