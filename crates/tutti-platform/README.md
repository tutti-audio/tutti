# tutti-platform

The parts of a real-time audio program that depend on the machine it runs on: a clock, named threads, thread priority, the main thread and the control of denormal floats.

Each one has a different answer on Linux, macOS, Windows and the web, and tutti keeps those answers in this one crate. Everything above it builds on every target without a platform `cfg` of its own, and on the web the clock reads `performance.now()` and a priority request is refused with a reason.

It depends on `web-time`, the operating system's bindings and, on the web, the browser's queue and timers, and on no part of tutti, so any tutti crate can use it. It is a building block for the other tutti crates.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
