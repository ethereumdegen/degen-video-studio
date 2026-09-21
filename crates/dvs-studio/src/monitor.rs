//! Sound out of the studio window: one output device, one ring of samples, one clock.
//!
//! The window plays picture from an audio clock rather than the other way round. A
//! viewport advanced from `performance.now()` while a device plays from its own crystal
//! drifts audibly within seconds — the two clocks are not the same clock, and nothing in a
//! webview can make them one — so the samples this module hands to the device are the
//! authority, and [`Monitor::position`] is what the playhead follows.
//!
//! Three decisions here are load-bearing.
//!
//! **The callback never waits for anything.** The device calls back from a real-time
//! thread with a few milliseconds of deadline. It must not allocate, must not take a lock
//! the mixer can be holding, and must not block on the engine. So the mixer and the
//! callback share a fixed ring of samples with atomic cursors, the callback keeps its
//! resampler state inside its own closure, and [`Monitor::push`] — which does take a lock —
//! takes one the callback never touches. A callback that waited on the engine thread would
//! produce exactly the symptom this window exists to remove: sound that stutters while the
//! machine is busy, and no way to tell whether the edit or the monitor is at fault.
//!
//! **The device's format wins, and the samples are converted rather than refused.** A
//! sequence mixed at 48 kHz stereo playing on a 44.1 kHz mono device is an ordinary
//! Tuesday. Refusing it would make the studio silent on hardware that works fine for
//! everything else, so the callback resamples and remaps channels as it fills.
//!
//! **Every break in the sound is counted.** A dropout nobody can measure is how "playback
//! feels wrong" stays an unfalsifiable complaint. [`Monitor::underruns`] climbs when the
//! callback runs out of samples and when a push does not fit, because both sound the same
//! and both mean the same thing: the mixer did not keep up with the device.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, SupportedStreamConfig};
use dvs_core::error::{Error, Result};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// How much audio the ring holds, in seconds of the sequence's own samples.
///
/// The feeder mixes eight-second chunks and refills when the device has less than three
/// seconds left, so the worst case in flight is eleven seconds. Twelve leaves a second of
/// margin and costs 4.6 MB at 48 kHz stereo. Shorter would turn a slow mix into a dropout;
/// longer would mean a seek throws away more mixing than it saves.
const RING_SECONDS: usize = 12;

/// Source channels the callback can hold on its stack. Sequences mix at one or two
/// channels ([`dvs_audio::MixSpec`]); the ceiling is what lets the interpolator keep two
/// frames in fixed arrays instead of allocating on the audio thread.
const MAX_SOURCE_CHANNELS: usize = 8;

/// The studio's audio output. Cheap to clone; every clone drives the same device.
#[derive(Clone)]
pub struct Monitor {
    inner: Arc<Inner>,
}

struct Inner {
    /// Opened on first use, not in `new`: constructing the window must not depend on a
    /// sound server, and a machine with no audio still gets a studio.
    device: Mutex<Slot>,
    /// The one live stream. `None` means nothing is playing.
    current: Mutex<Option<Active>>,
}

enum Slot {
    Unopened,
    /// The device, or why there isn't one. The reason is kept so that every later `play`
    /// can name the same missing thing instead of retrying an absent sound server on
    /// every keypress.
    Opened(std::result::Result<Output, String>),
}

struct Output {
    device: cpal::Device,
    name: String,
    config: SupportedStreamConfig,
}

struct Active {
    stream: Arc<Stream>,
    /// Dropping this closes the device. Never read: the whole value of the field is its
    /// lifetime.
    _sink: cpal::Stream,
}

impl Monitor {
    pub fn new() -> Monitor {
        Monitor {
            inner: Arc::new(Inner {
                device: Mutex::new(Slot::Unopened),
                current: Mutex::new(None),
            }),
        }
    }

    /// The output device's name, or `None` when there is none. The window says which
    /// device it is playing through, because "no sound" and "sound on the wrong sink" look
    /// identical from the keyboard.
    pub fn device(&self) -> Option<String> {
        self.output(|output| output.name.clone()).ok()
    }

    /// The rate and channel count the *device* runs at — not the sequence's. Callers
    /// convert [`Monitor::position`] with this: a clock counted in device frames and
    /// divided by a sequence rate is a playhead that drifts by the ratio of the two.
    pub fn format(&self) -> (u32, u16) {
        self.output(|output| (output.config.sample_rate(), output.config.channels()))
            .unwrap_or((0, 0))
    }

    /// Start playing `samples` (interleaved f32 at `rate`/`channels`) from `from_sample`
    /// source frames in, replacing whatever was playing.
    ///
    /// The previous stream is dropped before the new one is built, so the device is never
    /// held by two callbacks at once — two streams on one sink do not take turns, they
    /// interleave, and the result is noise that sounds like a broken mixer.
    pub fn play(
        &self,
        samples: Arc<Vec<f32>>,
        rate: u32,
        channels: u16,
        from_sample: usize,
    ) -> Result<()> {
        if rate < 1_000 {
            return Err(Error::bad_args(format!(
                "{rate} Hz is not a playable sample rate; sequences run at 44100 or 48000"
            )));
        }
        if channels == 0 || usize::from(channels) > MAX_SOURCE_CHANNELS {
            return Err(Error::bad_args(format!(
                "the monitor plays 1 to {MAX_SOURCE_CHANNELS} channels, not {channels}"
            )));
        }

        // The device lock is held across the build. Resolving it happens exactly once,
        // and a `device()` in the status line waiting a few milliseconds behind a stream
        // being opened is invisible; an `Output` borrowed out from under the lock would
        // not be.
        let mut slot = lock(&self.inner.device);
        if matches!(*slot, Slot::Unopened) {
            *slot = Slot::Opened(open_output());
        }
        let output = match &*slot {
            Slot::Opened(Ok(output)) => output,
            Slot::Opened(Err(reason)) => return Err(Error::tool("audio", reason.clone())),
            Slot::Unopened => unreachable!("the slot was just opened"),
        };

        let stream = Arc::new(Stream::new(
            rate,
            channels,
            output.config.sample_rate(),
            output.config.channels(),
        ));
        stream.push(tail(&samples, from_sample, channels));

        {
            // Before, not after: building the next stream while the old one still owns the
            // device is how two callbacks end up fighting over one sink.
            let mut current = lock(&self.inner.current);
            *current = None;
        }
        let sink = build_sink(&output.device, &output.config, Arc::clone(&stream))?;
        sink.play().map_err(|error| {
            Error::tool("audio", format!("the output device would not start: {error}"))
        })?;
        *lock(&self.inner.current) = Some(Active {
            stream,
            _sink: sink,
        });
        Ok(())
    }

    /// Append to the stream already playing. `false` means there is none — the caller's
    /// playback was stopped or replaced, and it should stop mixing.
    ///
    /// The lock taken here is never taken by the audio callback, which holds its own
    /// reference to the stream: a mixer thread that could block the device is a mixer
    /// thread that will, on the first slow decode.
    pub fn push(&self, samples: &[f32]) -> bool {
        match lock(&self.inner.current).as_ref() {
            Some(active) => {
                active.stream.push(samples);
                true
            }
            None => false,
        }
    }

    /// Stop and release the device.
    pub fn stop(&self) {
        *lock(&self.inner.current) = None;
    }

    pub fn is_playing(&self) -> bool {
        lock(&self.inner.current).is_some()
    }

    /// Device frames of content handed to the device since [`Monitor::play`] — the audio
    /// clock, and the only honest one available here.
    ///
    /// Not elapsed wall time since playback started: that ignores the buffer the device
    /// has not played yet, ignores every underrun, and drifts against the device's own
    /// crystal for as long as the session lasts. Silence inserted during a dropout is not
    /// content and does not advance this, because the samples it stood in for are still
    /// coming: counting them would slide the picture permanently ahead of the sound by the
    /// length of every glitch.
    pub fn position(&self) -> usize {
        lock(&self.inner.current)
            .as_ref()
            .map_or(0, |active| active.stream.position.load(Ordering::Relaxed) as usize)
    }

    /// Breaks in the sound since [`Monitor::play`]: one per stretch the callback could not
    /// fill, one per push the ring could not hold, one per device error. A continuous
    /// starvation counts once, not once per callback — what matters is how often the sound
    /// broke, and a longer gap is still one audible click.
    pub fn underruns(&self) -> u64 {
        lock(&self.inner.current)
            .as_ref()
            .map_or(0, |active| active.stream.dropouts.load(Ordering::Relaxed))
    }

    /// Resolve the device once and read something out of it.
    fn output<T>(&self, map: impl FnOnce(&Output) -> T) -> Result<T> {
        let mut slot = lock(&self.inner.device);
        if matches!(*slot, Slot::Unopened) {
            *slot = Slot::Opened(open_output());
        }
        match &*slot {
            Slot::Opened(Ok(output)) => Ok(map(output)),
            Slot::Opened(Err(reason)) => Err(Error::tool("audio", reason.clone())),
            Slot::Unopened => unreachable!("the slot was just opened"),
        }
    }
}

impl Default for Monitor {
    fn default() -> Self {
        Monitor::new()
    }
}

/// A mutex guard that survives a panic elsewhere.
///
/// Every value behind these locks is plain data with no invariant a panic could break, and
/// a monitor that answered "poisoned" for the rest of the session would turn one unrelated
/// panic into a window with no sound and no explanation.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Find the default output device and the format it wants.
fn open_output() -> std::result::Result<Output, String> {
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else {
        return Err(format!(
            "no default output device on the {} host; a sound server (PipeWire, PulseAudio or ALSA) must be running for the studio to play sound",
            host.id()
        ));
    };
    let config = device.default_output_config().map_err(|error| {
        format!("the default output device ({device}) reports no usable configuration: {error}")
    })?;
    Ok(Output {
        name: device.to_string(),
        device,
        config,
    })
}

/// The part of a mix `play` starts from.
///
/// `from_sample` counts source *frames* — samples per channel — so a caller that mixed a
/// span and wants to start in the middle of it does not have to multiply by the channel
/// count, which is the multiplication everybody gets wrong exactly once.
fn tail(samples: &[f32], from_sample: usize, channels: u16) -> &[f32] {
    let start = from_sample
        .saturating_mul(usize::from(channels))
        .min(samples.len());
    &samples[start..]
}

fn build_sink(
    device: &cpal::Device,
    config: &SupportedStreamConfig,
    stream: Arc<Stream>,
) -> Result<cpal::Stream> {
    let format = config.sample_format();
    let config = config.config();
    let built = match format {
        SampleFormat::F32 => sink::<f32>(device, config, stream),
        SampleFormat::F64 => sink::<f64>(device, config, stream),
        SampleFormat::I16 => sink::<i16>(device, config, stream),
        SampleFormat::I32 => sink::<i32>(device, config, stream),
        SampleFormat::U8 => sink::<u8>(device, config, stream),
        SampleFormat::U16 => sink::<u16>(device, config, stream),
        other => {
            return Err(Error::tool(
                "audio",
                format!(
                    "the device wants {other} samples; the monitor writes f32, f64, i32, i16, u16 or u8"
                ),
            ))
        }
    };
    built.map_err(|error| {
        Error::tool(
            "audio",
            format!("the output stream could not be opened: {error}"),
        )
    })
}

/// One output stream, converting to whatever the device wants as it writes.
///
/// The conversion happens at the point of the store rather than through a staging buffer,
/// so the common case — an f32 device and an f32 mix — is a direct write and the callback
/// owns no heap at all.
fn sink<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    stream: Arc<Stream>,
) -> std::result::Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let on_error = Arc::clone(&stream);
    let mut pull = Pull::default();
    device.build_output_stream::<T, _, _>(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| pull.fill(out, &stream),
        move |_error| {
            // A device error — a reroute, an xrun, a disconnect — is a break in the sound
            // like any other, and the window's dropout counter is where a break is
            // visible. Silently swallowing it is how "it sounded odd for a second"
            // survives as a rumour.
            on_error.dropouts.fetch_add(1, Ordering::Relaxed);
        },
        None,
    )
}

// ------------------------------------------------------------------- the shared stream

/// Everything the mixer and the audio callback share. One per [`Monitor::play`], so a
/// replaced stream takes its buffer, its clock and its counters with it and cannot report
/// a position belonging to playback nobody asked for any more.
struct Stream {
    ring: Ring,
    source_rate: u32,
    source_channels: u16,
    device_rate: u32,
    device_channels: u16,
    position: AtomicU64,
    dropouts: AtomicU64,
}

impl Stream {
    fn new(source_rate: u32, source_channels: u16, device_rate: u32, device_channels: u16) -> Stream {
        let capacity = RING_SECONDS
            .saturating_mul(source_rate as usize)
            .saturating_mul(usize::from(source_channels.max(1)));
        Stream {
            ring: Ring::new(capacity),
            source_rate,
            source_channels,
            device_rate,
            device_channels,
            position: AtomicU64::new(0),
            dropouts: AtomicU64::new(0),
        }
    }

    fn push(&self, samples: &[f32]) {
        if self.ring.push(samples) > 0 {
            self.dropouts.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A single-producer, single-consumer ring of f32 samples.
///
/// The slots are atomics rather than plain floats because the overflow policy deliberately
/// lets the producer reclaim space the consumer may be reading at that instant. With plain
/// memory that is a data race and therefore undefined behaviour; with relaxed atomics it is
/// a load whose result the consumer's compare-exchange then throws away. Relaxed loads and
/// stores of `u32` are ordinary machine loads and stores on every target this runs on, so
/// the soundness costs nothing.
struct Ring {
    slots: Box<[AtomicU32]>,
    /// Samples ever written and ever read. Both monotone, so occupancy is a subtraction
    /// and wrapping is a modulo — no empty-or-full ambiguity to get wrong.
    write: AtomicU64,
    read: AtomicU64,
}

impl Ring {
    fn new(capacity: usize) -> Ring {
        let capacity = capacity.max(1);
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || AtomicU32::new(0));
        Ring {
            slots: slots.into_boxed_slice(),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
        }
    }

    fn capacity(&self) -> u64 {
        self.slots.len() as u64
    }

    /// Samples waiting to be played. Occupancy is the tests' business rather than the
    /// feeder's: `app.rs` refills from the mix cursor it already keeps, which stays right
    /// even when the device has not started.
    #[cfg(test)]
    fn available(&self) -> u64 {
        self.write
            .load(Ordering::Acquire)
            .saturating_sub(self.read.load(Ordering::Acquire))
    }

    /// Append, dropping the oldest samples when they do not fit. Returns how many were
    /// dropped.
    ///
    /// Dropping rather than blocking is the point: the producer is the engine thread, and
    /// a push that waited for the device would stall every frame, op and lint behind a
    /// buffer that is already seconds ahead of the playhead. Dropping the *oldest* keeps
    /// the newest mix — the part nearest to what the user is about to hear.
    fn push(&self, samples: &[f32]) -> u64 {
        let capacity = self.capacity();
        let mut dropped = 0;
        let samples = if samples.len() as u64 > capacity {
            let keep = capacity as usize;
            dropped += (samples.len() - keep) as u64;
            &samples[samples.len() - keep..]
        } else {
            samples
        };

        let write = self.write.load(Ordering::Relaxed);
        let read = self.read.load(Ordering::Acquire);
        let need = samples.len() as u64;
        let free = capacity - (write - read);
        if need > free {
            let ahead = need - free;
            self.read.fetch_max(read + ahead, Ordering::AcqRel);
            dropped += ahead;
        }

        for (offset, sample) in samples.iter().enumerate() {
            let at = ((write + offset as u64) % capacity) as usize;
            self.slots[at].store(sample.to_bits(), Ordering::Relaxed);
        }
        // Publishing the cursor last is what makes the stores above visible to the
        // callback before it can reach them.
        self.write.store(write + need, Ordering::Release);
        dropped
    }

    /// Take one frame of `out.len()` samples. `false` means the ring ran dry and nothing
    /// was consumed.
    fn pop_frame(&self, out: &mut [f32]) -> bool {
        let capacity = self.capacity();
        let want = out.len() as u64;
        loop {
            let read = self.read.load(Ordering::Relaxed);
            let write = self.write.load(Ordering::Acquire);
            if write - read < want {
                return false;
            }
            for (offset, slot) in out.iter_mut().enumerate() {
                let at = ((read + offset as u64) % capacity) as usize;
                *slot = f32::from_bits(self.slots[at].load(Ordering::Relaxed));
            }
            // A failed exchange means the producer reclaimed this space while the copy was
            // running, so what was just read is no longer in the buffer. Retrying from the
            // cursor it left behind is the difference between one skipped chunk and a
            // frame of noise.
            if self
                .read
                .compare_exchange(read, read + want, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return true;
            }
        }
    }
}

// -------------------------------------------------------------------- the audio callback

/// The callback's own state. Nothing here is shared, so nothing here is atomic: the device
/// calls one callback at a time on one thread, and the whole point of the ring is that this
/// state never needs a lock.
#[derive(Default)]
struct Pull {
    /// The source frames the playback point sits between.
    current: [f32; MAX_SOURCE_CHANNELS],
    next: [f32; MAX_SOURCE_CHANNELS],
    /// How many of the two hold real samples yet.
    ready: u8,
    /// Where between them, in source frames.
    phase: f64,
    /// Whether the previous fill ran dry, so that one long starvation counts as one break
    /// rather than one per callback.
    starved: bool,
}

impl Pull {
    /// Fill one device buffer, resampling and remapping channels on the way.
    ///
    /// Linear interpolation, deliberately: this is a monitor, the exported mix is
    /// resampled by ffmpeg in the render path, and a windowed-sinc kernel in a callback
    /// that may not allocate is CPU spent where nobody is listening for it. When the rates
    /// match — the ordinary case, both 48 kHz — the phase is exactly zero at every frame
    /// and the interpolation returns the source sample unchanged.
    fn fill<T>(&mut self, out: &mut [T], stream: &Stream)
    where
        T: SizedSample + FromSample<f32>,
    {
        let source = usize::from(stream.source_channels).min(MAX_SOURCE_CHANNELS);
        let device = usize::from(stream.device_channels).max(1);
        let step = f64::from(stream.source_rate) / f64::from(stream.device_rate.max(1));
        let usable = out.len() / device * device;

        let mut frame = [0.0f32; MAX_SOURCE_CHANNELS];
        let mut produced = 0usize;
        for slot in out[..usable].chunks_mut(device) {
            if !self.bracket(stream, source) {
                break;
            }
            let between = self.phase as f32;
            for channel in 0..source {
                let (a, b) = (self.current[channel], self.next[channel]);
                frame[channel] = a + (b - a) * between;
            }
            place(&frame[..source], slot);
            self.phase += step;
            produced += 1;
        }

        let filled = produced * device;
        if filled < out.len() {
            // The device plays whatever is in the buffer, so the tail has to be silence
            // rather than the previous callback's samples repeated.
            out[filled..].fill(T::from_sample(0.0f32));
        }
        if filled < usable {
            if !self.starved {
                stream.dropouts.fetch_add(1, Ordering::Relaxed);
                self.starved = true;
            }
        } else {
            self.starved = false;
        }
        stream.position.fetch_add(produced as u64, Ordering::Relaxed);
    }

    /// Pull source frames until `current` and `next` bracket the playback point. `false`
    /// when the ring ran dry, in which case nothing moved: the next callback resumes on
    /// the sample this one stopped at instead of skipping past it.
    fn bracket(&mut self, stream: &Stream, source: usize) -> bool {
        while self.ready < 2 {
            if !self.shift(stream, source) {
                return false;
            }
            self.ready += 1;
        }
        while self.phase >= 1.0 {
            if !self.shift(stream, source) {
                return false;
            }
            self.phase -= 1.0;
        }
        true
    }

    fn shift(&mut self, stream: &Stream, source: usize) -> bool {
        let mut frame = [0.0f32; MAX_SOURCE_CHANNELS];
        if !stream.ring.pop_frame(&mut frame[..source]) {
            return false;
        }
        self.current = self.next;
        self.next = frame;
        true
    }
}

/// Put one source frame on the device's channels.
///
/// Mono into anything plays out of every speaker; anything into mono is summed and divided
/// by the channel count. A straight sum would clip a mix that already peaks near full
/// scale and invent distortion the document does not contain, and taking the left channel
/// alone would lose whatever is panned right — which is worse than either.
fn place<T>(source: &[f32], out: &mut [T])
where
    T: SizedSample + FromSample<f32>,
{
    match (source.len(), out.len()) {
        (0, _) => out.fill(T::from_sample(0.0f32)),
        (_, 1) => {
            let sum: f32 = source.iter().sum();
            out[0] = T::from_sample(sum / source.len() as f32);
        }
        (1, _) => out.fill(T::from_sample(source[0])),
        (channels, wanted) => {
            for (slot, value) in out.iter_mut().zip(source.iter()) {
                *slot = T::from_sample(*value);
            }
            // A device with more channels than the mix leaves the rest silent: a stereo
            // mix belongs on the front pair, not smeared around the room.
            if wanted > channels {
                out[channels..].fill(T::from_sample(0.0f32));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stream with no device behind it, for the paths that must work on a machine with
    /// no sound server — and for proving the no-device path on one that has.
    fn headless() -> Monitor {
        Monitor {
            inner: Arc::new(Inner {
                device: Mutex::new(Slot::Opened(Err("no output device in this test".into()))),
                current: Mutex::new(None),
            }),
        }
    }

    /// cpal finds a real device on a workstation with PipeWire; a CI container has none.
    /// A test that needs hardware says so and passes rather than failing a build over an
    /// absent speaker. Run with `--nocapture` to see the reason.
    fn hardware(monitor: &Monitor, test: &str) -> bool {
        match monitor.device() {
            Some(name) => {
                eprintln!("{test}: playing through {name}");
                true
            }
            None => {
                eprintln!("{test}: SKIPPED, no audio output device on this machine");
                false
            }
        }
    }

    fn ramp(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels)
            .map(|index| (index / channels) as f32)
            .collect()
    }

    #[test]
    fn a_full_ring_drops_the_oldest_samples_instead_of_blocking() {
        let ring = Ring::new(8);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 0);
        assert_eq!(ring.available(), 6);

        let mut frame = [0.0f32; 1];
        assert!(ring.pop_frame(&mut frame) && frame[0] == 1.0);
        assert!(ring.pop_frame(&mut frame) && frame[0] == 2.0);

        // Four slots free, eight offered: the four oldest go rather than the writer.
        assert_eq!(ring.push(&[7.0, 8.0, 9.0, 10.0, 11.0, 12.0, 13.0, 14.0]), 4);
        assert_eq!(ring.available(), 8, "the ring never holds more than it has");
        assert!(ring.pop_frame(&mut frame));
        assert_eq!(frame[0], 7.0, "3..6 were dropped, not 7..14");

        // A push longer than the whole ring keeps its tail, which is the part that is
        // about to be heard.
        let ring = Ring::new(4);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]), 2);
        assert!(ring.pop_frame(&mut frame));
        assert_eq!(frame[0], 3.0);
    }

    #[test]
    fn the_clock_counts_frames_consumed_and_a_dry_ring_counts_one_break() {
        let stream = Stream::new(48_000, 2, 48_000, 2);
        stream.push(&ramp(100, 2));

        let mut pull = Pull::default();
        let mut out = vec![0.0f32; 128 * 2];
        pull.fill(&mut out, &stream);

        // 100 frames in, 99 out: linear interpolation needs the frame after the one it is
        // emitting, so the last one stays in hand until more arrives.
        assert_eq!(stream.position.load(Ordering::Relaxed), 99);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[2], 1.0, "at matching rates the samples pass through exactly");
        assert_eq!(out[98 * 2], 98.0);
        assert_eq!(out[99 * 2], 0.0, "the tail of a short fill is silence");
        assert_eq!(stream.dropouts.load(Ordering::Relaxed), 1);

        // Still starved: one break, not one per callback.
        pull.fill(&mut out, &stream);
        assert_eq!(stream.dropouts.load(Ordering::Relaxed), 1);
        assert_eq!(stream.position.load(Ordering::Relaxed), 99);

        // Fed again and drained again: a second break. 100 more frames cover the 64-frame
        // callback exactly and leave the 128-frame one short.
        stream.push(&ramp(100, 2));
        let mut small = vec![0.0f32; 64 * 2];
        pull.fill(&mut small, &stream);
        assert_eq!(stream.position.load(Ordering::Relaxed), 99 + 64);
        assert_eq!(
            stream.dropouts.load(Ordering::Relaxed),
            1,
            "a full fill is not a break"
        );
        assert_eq!(
            small[0], 99.0,
            "the frame held back for interpolation is played, not skipped"
        );

        pull.fill(&mut out, &stream);
        assert_eq!(stream.position.load(Ordering::Relaxed), 99 + 64 + 36);
        assert_eq!(stream.dropouts.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_mono_48k_mix_plays_on_a_stereo_44k1_device_for_the_same_length_of_time() {
        let stream = Stream::new(48_000, 1, 44_100, 2);
        // One second of a 1 kHz tone, so a resampler that dropped or repeated blocks would
        // show up as a level change rather than as nothing.
        let source: Vec<f32> = (0..48_000)
            .map(|index| (index as f32 * 1_000.0 * std::f32::consts::TAU / 48_000.0).sin())
            .collect();
        stream.push(&source);

        let mut pull = Pull::default();
        let mut out = vec![0.0f32; 441 * 2];
        let mut blocks = 0;
        while stream.ring.available() > 0 && blocks < 200 {
            pull.fill(&mut out, &stream);
            blocks += 1;
        }
        let played = stream.position.load(Ordering::Relaxed) as i64;
        assert!(
            (played - 44_100).abs() <= 44,
            "a second of 48 kHz must take a second on a 44.1 kHz device, got {played} frames"
        );
        assert!(
            (0..441).all(|frame| out[frame * 2] == out[frame * 2 + 1]),
            "a mono mix reaches both speakers"
        );

        // Stereo into a mono device keeps both channels rather than half the mix.
        let stream = Stream::new(48_000, 2, 48_000, 1);
        stream.push(&[1.0, 0.0, 0.0, 1.0, 0.5, 0.5, 0.0, 0.0]);
        let mut mono = vec![0.0f32; 3];
        Pull::default().fill(&mut mono, &stream);
        assert_eq!(mono[0], 0.5, "left-only is heard at half the mix's level");
        assert_eq!(mono[1], 0.5, "right-only is heard too, not dropped");
        assert_eq!(mono[2], 0.5, "a centred signal keeps its level");
    }

    #[test]
    fn without_a_device_play_names_audio_and_nothing_is_playing() {
        let monitor = headless();
        assert_eq!(monitor.device(), None);
        assert_eq!(monitor.format(), (0, 0));

        let error = monitor
            .play(Arc::new(vec![0.0; 480]), 48_000, 2, 0)
            .expect_err("there is no device in this test");
        assert_eq!(error.kind(), "tool-missing");
        assert!(
            error.to_string().starts_with("audio unavailable"),
            "the failure has to name the missing tool: {error}"
        );

        assert!(!monitor.is_playing());
        assert_eq!(monitor.position(), 0);
        assert_eq!(monitor.underruns(), 0);
        assert!(!monitor.push(&[0.0; 64]), "nothing to push into");
        monitor.stop();
    }

    #[test]
    fn playing_twice_leaves_one_stream_and_restarts_the_clock() {
        let monitor = Monitor::new();
        if !hardware(&monitor, "playing_twice_leaves_one_stream") {
            return;
        }
        let (rate, _) = monitor.format();

        monitor
            .play(Arc::new(vec![0.25; 48_000 * 2]), 48_000, 2, 0)
            .expect("the first stream");
        let first = lock(&monitor.inner.current)
            .as_ref()
            .map(|active| Arc::clone(&active.stream))
            .expect("something is playing");
        assert!(monitor.push(&[0.25; 4_800]), "the first stream takes more");

        // Wait for the device to actually take samples. How long a sound server needs to
        // start a stream is its business, so this polls the clock rather than assuming a
        // latency; a clock that never moves fails here instead of hanging.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while first.position.load(Ordering::Relaxed) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the first stream never played a sample"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        monitor
            .play(Arc::new(vec![-0.5; 48_000 * 2]), 48_000, 2, 0)
            .expect("the second stream");
        let second = lock(&monitor.inner.current)
            .as_ref()
            .map(|active| Arc::clone(&active.stream))
            .expect("something is playing");

        assert!(
            !Arc::ptr_eq(&first, &second),
            "a second play must not reuse the retired stream"
        );
        assert_eq!(
            Arc::strong_count(&first),
            1,
            "the first stream's callback is gone: two streams never share the device"
        );
        assert!(
            monitor.position() < rate as usize / 4,
            "the clock restarts with the stream"
        );

        // Whatever is left in the new stream is the new stream's samples, not the old
        // buffer's: 0.25 was never mixed into it.
        let mut frame = [0.0f32; 2];
        assert!(second.ring.pop_frame(&mut frame));
        assert_eq!(frame, [-0.5, -0.5]);

        monitor.stop();
        assert!(!monitor.is_playing());
    }

    #[test]
    fn a_real_second_of_tone_advances_the_clock_by_a_second() {
        let monitor = Monitor::new();
        if !hardware(&monitor, "a_real_second_of_tone") {
            return;
        }
        let (device_rate, _) = monitor.format();
        let rate = 48_000u32;
        // Quiet: this runs in `cargo test`, and a full-scale tone out of a laptop is a
        // hostile way to learn that the monitor works.
        let tone: Vec<f32> = (0..rate as usize)
            .flat_map(|index| {
                let value =
                    (index as f32 * 440.0 * std::f32::consts::TAU / rate as f32).sin() * 0.05;
                [value, value]
            })
            .collect();

        monitor
            .play(Arc::new(tone), rate, 2, 0)
            .expect("play a second of 440 Hz");
        assert!(monitor.is_playing());
        std::thread::sleep(std::time::Duration::from_millis(1_300));

        let played = monitor.position() as f64 / f64::from(device_rate.max(1));
        assert!(
            (played - 1.0).abs() < 0.1,
            "a second of samples must take about a second of device frames, got {played:.3} s"
        );
        monitor.stop();
        assert_eq!(monitor.position(), 0, "a stopped monitor has no clock");
    }

    #[test]
    fn from_sample_counts_frames_not_samples() {
        let samples = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        assert_eq!(tail(&samples, 2, 2), &[5.0, 6.0]);
        assert_eq!(tail(&samples, 1, 1), &[2.0, 3.0, 4.0, 5.0, 6.0]);
        // Past the end is silence, not a panic: a caller seeking to the end of a chunk is
        // asking for nothing, not for a crash.
        assert!(tail(&samples, 99, 2).is_empty());
    }

    #[test]
    fn the_monitor_can_live_in_tauri_state() {
        // Tauri hands `&State<Monitor>` to command handlers on several threads and the
        // feeder task clones one into a future. A field that is not `Sync` would fail
        // here rather than at the far end of the wiring.
        fn managed<T: Send + Sync + Clone + 'static>() {}
        managed::<Monitor>();
    }
}
