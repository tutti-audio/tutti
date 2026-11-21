use super::*;
use crate::{Job, Resample};
use tutti_codec::{Format, Pcm};

/// What these tests vary: a 1 kHz, eight-frame render into a WAV of `pcm`.
struct Case {
    rate: SampleRate,
    channels: ChannelLayout,
    pcm: Pcm,
    topology: Option<tutti_buffers::ChannelTopology>,
    resample: Option<Resample>,
}

impl Case {
    fn plan(&self) -> RenderPlan {
        RenderPlan::of(
            &Job::new(self.rate, Samples::new(8), Samples::new(0), Samples::new(0)).unwrap(),
        )
    }

    fn target(&self, path: &Path) -> FileTarget {
        let target = FileTarget::new(path, Format::Wav(self.pcm), self.channels);
        let target = match &self.topology {
            Some(topology) => target.topology(topology.clone()),
            None => target,
        };
        match self.resample {
            Some(resample) => target.resample(resample),
            None => target,
        }
    }
}

fn config(channels: u16, pcm: Pcm) -> Case {
    Case {
        rate: SampleRate::of(1_000.0),
        channels: ChannelLayout::from(channels),
        pcm,
        topology: None,
        resample: None,
    }
}

/// A plan that cannot fit a WAV file is refused before any of it is rendered
/// and before the path is touched: bytes already at the path survive.
///
/// The plan is real: 2^32 mono frames at 16 bits is 8 GiB, past the format's
/// 32-bit sizes, and no frame is written because the refusal is made before
/// the file is opened.
#[test]
fn a_plan_that_cannot_fit_is_refused_before_the_file_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(1, Pcm::Int16);
    let frames = Samples::new(1 << 32);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: Samples::new(0),
    };
    let rate = config.rate;
    let path = dir.path().join("keep.wav");
    std::fs::write(&path, b"sentinel").unwrap();
    let err = open_file::<f32>(&config.target(&path), &plan, rate)
        .map(drop)
        .expect_err("2^32 mono 16-bit frames do not fit a WAV file");
    assert!(
        matches!(
            err,
            RenderError::Encode(tutti_codec::EncodeError::WavTooLarge { limit_frames }) if limit_frames < 1 << 32
        ),
        "{err}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"sentinel",
        "a refused export must not touch the file at the path"
    );
}

/// The limit is judged on the frames the file will hold, which a resample to a
/// higher rate raises: 2^31 - 1000 frames fit a mono 16-bit WAV at the render
/// rate, and do not once they are written at twice the rate.
#[test]
fn the_limit_is_judged_at_the_output_rate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.wav");
    let mut config = config(1, Pcm::Int16);
    let frames = Samples::new((1 << 31) - 1_000);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: Samples::new(0),
    };
    let rate = config.rate;
    open_file::<f32>(&config.target(&path), &plan, rate)
        .map(drop)
        .expect("2^31 - 1000 mono 16-bit frames fit");
    config.resample = Some(Resample::to(SampleRate::of(2_000.0)));
    let err = open_file::<f32>(&config.target(&path), &plan, rate)
        .map(drop)
        .expect_err("twice the frames do not");
    assert!(
        matches!(
            err,
            RenderError::Encode(tutti_codec::EncodeError::WavTooLarge { .. })
        ),
        "{err}"
    );
}

/// A topology reaches the file: four channels declare the quad mask `0x33`,
/// told they are quad or default to it by their width, six declare 5.1 by their
/// width, and four unassigned channels declare none.
///
/// This restores the export-level half of the old
/// `an_export_at_the_limit_succeeds_and_declares_its_mask`; the limit half
/// lives in `tutti-codec`, where the limit can be lowered.
#[test]
fn the_topology_reaches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    for (channels, topology, mask) in [
        (4u16, Some(tutti_buffers::Bed::Quad.topology()), 0x33u32),
        (4, None, 0x33),
        (
            4,
            Some(tutti_buffers::ChannelTopology::unassigned(
                ChannelLayout::QUAD,
            )),
            0,
        ),
        (6, None, 0x60F),
    ] {
        let mut cfg = config(channels, Pcm::Int24);
        cfg.topology = topology;
        let path = dir.path().join(format!("m{channels}_{mask}.wav"));
        let layout = ChannelLayout::from(channels);
        let mut chain = open_file::<f32>(&cfg.target(&path), &cfg.plan(), cfg.rate).unwrap();
        chain
            .block(Interleaved::new(&vec![0.5f32; 8 * usize::from(channels)], layout).unwrap())
            .unwrap();
        chain.finalize().unwrap().commit().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            mask,
            "{channels} channels"
        );
        assert_eq!(
            hound::WavReader::open(&path).unwrap().len(),
            8 * u32::from(channels)
        );
    }
}
