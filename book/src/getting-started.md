# Getting started

Let's make your first sound! In this chapter we write the smallest program
that makes one: a pure tone, the A that orchestras tune to, held for two
seconds and saved to a file. It needs no sound card, so it runs anywhere Rust
does, and at the end of the page you can listen to exactly what it produced.

## Setting up a project

If you have not installed Rust yet, [rustup](https://rustup.rs) is the
easiest way, and it only takes a minute. Then make a new program with Cargo
and add tutti to it:

```sh
cargo new hello-tutti
cd hello-tutti
cargo add tutti --git https://github.com/tutti-audio/tutti --features export,encode-flac
```

tutti keeps most of its pieces behind features, so a program only builds
what it uses. We ask for two: `export`, to render sound into files, and
`encode-flac`, to write those files as FLAC.

That's it, you're ready! We will build the program up a few lines at a
time, and put it all together at the end.

## A place for sound to happen

Before we can make any sound, we need somewhere for it to happen. In tutti
that place is a [`Session`](../api/tutti/struct.Session.html). Think of it as
a little studio: it holds the instruments and effects you add, the wiring
between them, and the clock that keeps them all in time.

A session usually plays through your sound card, but for our first sound we
would rather write a file, so we build one that is not attached to any
device:

```rust,ignore
// No sound card: the session renders when we ask it to.
let mut session = Session::builder().sample_rate(SampleRate::SR_48K).build()?;
```

The sample rate (also known as *sampling frequency*) tells
the session how finely to slice time. 48 000 slices a
second is the usual choice for music, and it is plenty for our ears.

<details><summary>🎧 What is a sample rate?</summary>

Digital audio is a long list of numbers, one for every tiny slice of time.
The sample rate is how many slices make up one second. At 48 000 slices a
second, everything the human ear can hear fits comfortably.

</details>

## Something to listen to

An empty studio is a quiet one, so let's add something that makes sound. The
simplest sound there is, the pure tone you hear in a hearing test, comes from
an oscillator: a source that repeats one smooth wave forever.

```rust,ignore
// An A at 440 Hz, a quarter of full scale.
let sine = session.graph().add(OscillatorNode::from(
    OscillatorConfig::new(OscillatorWave::Sine)
        .frequency(Hz::new(440.0))
        .level(Amplitude::new(0.25)),
));
```

Here we add an [`OscillatorNode`](../api/tutti/nodes/struct.OscillatorNode.html)
that plays a sine wave 440 times a second, the A above middle C. In return
the session hands us `sine`, a key we will use to talk about this oscillator
from now on.

You may have noticed that we did not write a plain `440.0`. The pitch is a
[`Hz`](../api/tutti/struct.Hz.html) and the loudness an
[`Amplitude`](../api/tutti/struct.Amplitude.html). In tutti every quantity
carries its unit, so mixing up a pitch with a volume is a compile error
rather than a nasty surprise in your headphones.

<details><summary>🎧 Why a quarter, and not full volume?</summary>

An amplitude of 1.0 is full scale (also known as *0 dBFS*), the loudest value a
file can hold. A sine
that loud is unpleasant and leaves no room for anything you add later, so we
start at a quarter of it, about 12 dB quieter. Your ears will thank you.

</details>

## Letting it reach our ears

Our oscillator is playing now, but nobody can hear it yet: a sound only
reaches the outside world when we wire it to the session's output. The
oscillator has a single output, and we want to hear it in both ears, so we
connect it to the left channel and to the right one:

```rust,ignore
// One mono output, heard in the left and the right channel.
session
    .graph()
    .connect(sine.output(0), MainOutput::channel(0))?;
session
    .graph()
    .connect(sine.output(0), MainOutput::channel(1))?;
```

[`MainOutput`](../api/tutti/struct.MainOutput.html) is the session's main output,
the end of every signal path (the *master bus* of other tools). Wiring is how everything in
tutti fits together, and later chapters build much richer paths than this
one.

## Putting it on disk

All that is left is to capture the sound. Recording the output of a session
into a file is called a *bounce* (also known as *export*, or *mixdown*).
First we describe what we want to keep: the first two seconds.

```rust,ignore
Bounce::new()
    .range(Seconds::new(0.0)..Seconds::new(2.0)) // from 0 s to 2 s
```

Then we tell the [`Bounce`](../api/tutti/struct.Bounce.html) where the file
goes: the *main output*, the file of the whole mix (also known as the *master*,
*main mix*, or *stereo mix*). It is a `ToFile`: how the sound is written (here
in stereo, left and right) and where it lands. A bounce never overwrites a file
by accident, so we also say that a second run of the program may replace the
file the first one wrote. Then we hand it to the session:

```rust,ignore
// The file's name picks its format: `.flac` is FLAC.
let mut session = tone()?;
let rendering = session.bounce(two_seconds().main_output(ToFile::new(
    Encoding::new(Format::from_path(&path)?).width(ChannelLayout::STEREO),
    tutti::codec::ToFile::new(&path).replace(),
)))?;
// The session renders on a thread of its own; we wait for it, then hand
// what it made back to the session.
let rendered = block_on(rendering.into_future());
session.apply(rendered)?;
```

The session renders the two seconds as fast as your computer can manage,
which is far quicker than listening to them, and writes the file.

## Hearing it

Put together, with a few lines to choose where the file goes, our program
fits in one `src/main.rs`:

```rust,ignore
{{#include ../../testing/tutti-book/examples/getting_started.rs:imports}}

{{#include ../../testing/tutti-book/examples/getting_started.rs:session}}

{{#include ../../testing/tutti-book/examples/getting_started.rs:bounce}}

{{#include ../../testing/tutti-book/examples/getting_started.rs:main}}
```

Now run it with `cargo run -- .`, and a few moments later
`getting_started.flac` appears next to your project. Here is ours, so you can
compare:

<audio controls src="sounds/getting_started.flac"></audio>

A steady, gentle A, the same in both ears, for two seconds. If yours sounds
like that, congratulations: you just made your first sound with tutti! 🎉

## Try this

The best way to get a feel for it is to change one thing, run it again, and
listen:

- Change `Hz::new(440.0)` to `Hz::new(220.0)`, and the tone drops an octave.
- Change `OscillatorWave::Sine` to `OscillatorWave::Saw`. The pitch stays the
  same, but the sound turns bright and buzzy, because a saw wave carries
  every harmonic of the note.
- Remove the line that wires `MainOutput::channel(1)`, and the tone moves to your
  left ear.

A single tone is not music yet. In the guide, we start shaping it into some.
