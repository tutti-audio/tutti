//! The plugin's descriptors, waited on by the loop together with the socket
//! (unix: CLAP's `posix-fd-support`). One `cfg` on this module's declaration
//! holds the test and its helpers.

use super::*;

/// How long the loop idles between ticks here: the tick that serves the
/// plugin's callback request, which registers the descriptor.
const SLOW_TICK: Duration = Duration::from_millis(600);

/// **A descriptor the plugin registered is served when it is readable.** The
/// loop waits on the control socket and the plugin's descriptors in one
/// `poll`. The plugin registers a pipe, and the test makes it readable five
/// times, waiting for `on_fd` each time, and counts one call for each. How
/// soon it is served is not asserted: speed is not a test's.
#[test]
fn a_registered_descriptor_that_becomes_readable_is_served_once_each_time() {
    let _lock = crate::test_utils::plugin_load_lock();
    let probe = crate::test_utils::clap();
    probe.thread_reset();
    let _cmd =
        crate::test_utils::clap_probe_thread_command(tutti_clap_test_plugin::CMD_REGISTER_FD);
    let (mut fixture, _slab) = session("fds");
    let host = fixture.host();

    // A real socket pair: the loop polls the socket's descriptor together with
    // the plugin's.
    let (mut host_end, server_end) = UnixStream::pair().expect("socket pair");
    let mut transport = SocketTransport::new(Stream::from(OwnedFd::from(server_end)));

    let driver = std::thread::spawn(move || {
        write_frame(&mut host_end, &load_message()).expect("sends the load");
        let _loaded: tutti_plugin_ipc::ControlFrame =
            read_frame(&mut host_end).expect("the load is answered");
        assert!(
            host.block(1, 64, Duration::from_secs(5)).is_some(),
            "the render thread answers the block"
        );
        // The block asked for a callback; the tick serves it, and it registers
        // the descriptor.
        tutti_fixtures::wait_until("the descriptor to be registered", HANG_GUARD, || {
            crate::test_utils::clap_probe_thread_capture().fd_registered
        });
        for fire in 1..=5u32 {
            assert!(
                crate::test_utils::clap().fd_poke(),
                "the pipe is registered"
            );
            tutti_fixtures::wait_until("`on_fd` to be called", HANG_GUARD, || {
                crate::test_utils::clap_probe_thread_capture().fd_fires >= fire
            });
        }
        write_frame(&mut host_end, &ControlRequest::Shutdown).expect("asks it to stop");
    });

    let main = main_thread();
    let render = RenderThread::spawn(
        fixture.side.take().expect("run once"),
        Some(Box::new(fixture.audio.take().expect("run once"))),
    )
    .expect("the render thread");
    let result = serve_every(
        &mut fixture.session,
        &mut transport,
        &mut Native::default(),
        main,
        &render,
        SLOW_TICK,
    );
    fixture.session.drop_plugin(main);
    drop(render);
    assert!(result.is_ok(), "{result:?}");

    driver.join().expect("the driver ran");
    let cap = probe.thread_capture();
    assert_eq!(
        cap.fd_fires, 5,
        "one call of `on_fd` for each time it was readable"
    );
    assert_eq!(
        cap.fd_last_events, 1,
        "told it was readable (CLAP_POSIX_FD_READ)"
    );
}
