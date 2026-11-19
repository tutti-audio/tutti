# Welcome to the tutti Book 👋

tutti is a real-time audio engine for building DAWs in Rust. It is the part
of an audio program that lives underneath the interface: it keeps the sound
flowing and keeps everything in time, while your program draws the knobs and
the timeline on top.

This book will help you make sound with tutti, whether you have written audio
software before or this is your very first time.

## Why tutti?

Audio software has always felt a little chaotic to us: formats with their own
rules, specs with their own corners, vendor quirks no document mentions, and
systems code and DSP math living in the same program. tutti is our attempt to
tame that chaos with Rust's expressive type system. A frequency is a `Hz`, a
gain is a `Db`, a length is a count of frames, and a value that would be wrong
simply does not compile. We hope it makes audio programming feel calmer for
you too.

## What you can make with it

- 🎹 A music tool: a sequencer, a sampler, a groove box, a DAW of your own.
- 🎮 The audio of a game or an installation, where the sound follows what
  happens on screen.
- 💿 An offline renderer: a program that writes a mix, a set of stems or a
  finished master to disk, with no sound card at all.

## What tutti is not

tutti has no user interface of its own: it draws no windows, knobs or
timelines. Your program draws those and asks tutti for the sound. It is not a
finished DAW either; it is what you build one on.

## How this book works

We build sound step by step. Every program in the book is short, every one
compiles, and every one that makes a sound lets you hear it right on the page,
so you can check your ears against ours as you go.

1. **Getting started** sets up a project and makes your first sound.
2. **The guide** builds one piece of music from nothing, a chapter at a time:
   a sound, then shaping it, movement, time, notes, recordings, mixing, and
   finally bouncing it to files.
3. **Building blocks** goes back over each idea the guide used, one chapter
   each, from the smallest piece up.
4. **Going further** covers live audio and MIDI devices, hosting plugins,
   writing your own node, and analysis without a graph.

We assume you know some Rust. You do not need to be an audio engineer: when
the book meets an audio idea, a short 🎧 note explains it, and a 🦀 note does
the same for a Rust detail. Open them when they help, skip them when you
already know.

The book teaches; the API reference lists everything. You can build it with
`cargo doc -p tutti --open`.

Ready? Let's make some noise. 🎶
