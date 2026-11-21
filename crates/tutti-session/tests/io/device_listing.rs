//! The output-device listing runs off the update: an enumeration that blocks
//! holds nothing, and its answer reaches the host as an event in a later
//! update.

use std::time::Duration;

use tutti_platform::Instant;

use tutti_buffers::ChannelLayout;
use tutti_cpal::{DeviceId, DeviceListing};
use tutti_engine::{ManualStreamDriver, OutputSpec};
use tutti_session::{DeviceEvent, Session, SessionEvent};
use tutti_units::SampleRate;

/// **A blocked enumeration does not hold the update, and lands when it
/// answers.** While the enumerator is blocked, updates go by and report
/// nothing; once it answers, a later update reports the list
/// (`SessionEvent::OutputsListed`).
#[test]
fn a_blocked_enumeration_does_not_hold_the_update_and_lands_when_it_answers() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(
            OutputSpec::new(
                SampleRate::SR_48K,
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device");
    let (release, held) = std::sync::mpsc::channel::<()>();
    s.list_outputs(DeviceListing::from_fn(move || {
        held.recv().ok();
        Ok(["Card", "USB"].into_iter().map(DeviceId::new).collect())
    }));
    for _ in 0..3 {
        let events = s.update();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::Device(DeviceEvent::OutputsListed(_)))),
            "not answered yet: {events:?}"
        );
    }

    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let listed = s.update().into_iter().find_map(|e| match e {
            SessionEvent::Device(DeviceEvent::OutputsListed(devices)) => Some(devices),
            _ => None,
        });
        if let Some(devices) = listed {
            let names: Vec<_> = devices.iter().map(|d| d.name().to_owned()).collect();
            assert_eq!(names, ["Card", "USB"]);
            break;
        }
        assert!(Instant::now() < deadline, "no list arrived");
        std::thread::yield_now();
    }
}

/// **A listing that fails reports the cpal error itself**, not its text: the
/// event carries the `tutti_cpal::DeviceError` the enumeration returned.
#[test]
fn a_failed_listing_reports_the_typed_error() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(
            OutputSpec::new(
                SampleRate::SR_48K,
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device");
    s.list_outputs(DeviceListing::from_fn(|| {
        Err(tutti_cpal::DeviceError::InvalidDevice("no card".into()))
    }));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let failed = s.update().into_iter().find_map(|e| match e {
            SessionEvent::Device(DeviceEvent::OutputsListFailed(why)) => Some(why),
            _ => None,
        });
        if let Some(why) = failed {
            assert!(
                matches!(&why, tutti_cpal::DeviceError::InvalidDevice(what) if what == "no card"),
                "{why:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "no failure arrived");
        std::thread::yield_now();
    }
}

/// **A listing whose enumeration panics is a failure, not a wait that never ends**:
/// the update reports `DeviceError::ListingEnded`.
#[test]
fn a_listing_that_panics_reports_that_it_ended() {
    let (driver, _stream) = ManualStreamDriver::new();
    let mut s = Session::builder()
        .device(tutti_cpal::DeviceSelector::Default)
        .build_device_on(
            OutputSpec::new(
                SampleRate::SR_48K,
                ChannelLayout::STEREO,
                tutti_buffers::SampleFormat::F32,
            ),
            driver,
        )
        .expect("builds with no device");
    s.list_outputs(DeviceListing::from_fn(|| panic!("the host exploded")));
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let failed = s.update().into_iter().find_map(|e| match e {
            SessionEvent::Device(DeviceEvent::OutputsListFailed(why)) => Some(why),
            _ => None,
        });
        if let Some(why) = failed {
            assert!(
                matches!(why, tutti_cpal::DeviceError::ListingEnded),
                "{why:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "no failure arrived");
        std::thread::yield_now();
    }
}
