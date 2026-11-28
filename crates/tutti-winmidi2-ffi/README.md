# tutti-winmidi2-ffi

This crate is a small wrapper that lets tutti call Windows MIDI Services (the `Windows.Devices.Midi2` API) from safe Rust, to list MIDI endpoints with their properties and Function Blocks, to publish virtual devices, and to send and receive MIDI 2.0 Universal MIDI Packets through them.

It is an internal building block of `tutti-midi-hardware` and is empty off Windows. Most people will use it through the `tutti` crate rather than directly.

Until Windows ships the API in the box, an app that uses it puts `Windows.Devices.Midi2.dll` (from Microsoft's MIDI repository) beside its executable; tutti never ships it.

This crate is part of [tutti](https://github.com/tutti-audio/tutti), a real-time audio engine for Rust.

## License

Licensed under either of [Apache License, Version 2.0](https://github.com/tutti-audio/tutti/blob/main/LICENSE-APACHE) or [MIT license](https://github.com/tutti-audio/tutti/blob/main/LICENSE-MIT) at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in tutti by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
