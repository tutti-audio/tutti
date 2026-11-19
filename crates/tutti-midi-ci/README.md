# tutti-midi-ci

This crate holds MIDI-CI, the MIDI 2.0 way for devices to discover each other, agree on profiles and exchange properties.

It is the message codec, the state machines for both ends of a conversation (Discovery, Profile Configuration, Process Inquiry and Property Exchange) and the tables of the standard Profiles. It does no input or output and reads no clock: you hand it the bytes you received and the time, and it says what to send. It needs only `tutti-ump` and `tutti-units` of tutti's crates, so you can use it on its own.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
