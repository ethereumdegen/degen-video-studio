//! Turning a timeline into samples, exactly.
//!
//! The whole module exists to defend one property: **every clip lands on the sample the
//! document says it lands on, and the answer does not depend on which span you asked for.**
//! The naive mixer keeps a floating cursor and advances it by each clip's duration; after a
//! few hundred edits the cursor is a millisecond off, the a/v sync drifts, and nothing in
//! the document explains why. Here, an output frame index is always
//! `(time - span.start).sample_round(rate)` computed from exact rationals, so a clip at
//! `1/3` s on a 48 kHz timeline starts at sample 16000 whether the mix begins at 0 or at
//! 10 s, and two adjacent clips share a boundary sample instead of overlapping or gapping.
//!
//! Mixing is additive and has no limiter. Summing two loud clips can exceed full scale, and
//! that is deliberate: [`crate::analyze_loudness`] counts the samples that did, so an agent
//! reads a number instead of wondering whether a hidden limiter saved it. Silently
//! attenuating a mix would make the reported loudness a fiction.
//!
//! Signal order per clip is fixed and matches what an editor expects to see in a channel
//! strip: speed, reverse, clip fades, clip gain and pan, ducking, track gain and pan, and
//! finally the track's mute/solo state.

use crate::db_to_gain;
use dvs_core::asset::AssetStore;
use dvs_core::error::{Error, Result};
use dvs_core::ids::{AssetId, SequenceId, TrackId};
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Clip, Ducking, Generator, Project, Sequence, Source, Track, TrackKind};
use dvs_core::time::{Span, Time};
use dvs_media::toolchain::Toolchain;
use rayon::prelude::*;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::f64::consts::TAU;
use std::path::PathBuf;

/// Detection window for the ducking sidechain, in milliseconds. Short enough to follow a
/// syllable, long enough that the envelope does not collapse between the zero crossings of
/// a low-frequency voice.
const DETECT_WINDOW_MS: i64 = 10;

/// How deep a `Source::Sequence` chain may go before it is treated as a mistake. Real nests
/// are one or two deep; anything past this is a document that references itself through an
/// intermediate sequence, which would otherwise recurse until the stack runs out.
const MAX_NESTING: usize = 8;

/// The format a mix is rendered in: the sequence's sample rate and channel count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MixSpec {
    /// Samples per second per channel.
    pub rate: u32,
    /// Interleaved channel count. 1 and 2 are the meaningful values; panning applies to the
    /// first two channels and leaves any others alone.
    pub channels: u16,
}

impl MixSpec {
    /// The spec a sequence mixes in.
    pub fn of(sequence: &Sequence) -> MixSpec {
        MixSpec {
            rate: sequence.sample_rate,
            channels: sequence.channels,
        }
    }

    fn validate(self) -> Result<()> {
        if self.rate < 16 {
            return Err(Error::bad_args(format!(
                "sample rate {} is not usable; sequences run at 44100 or 48000",
                self.rate
            )));
        }
        if self.channels == 0 {
            return Err(Error::bad_args("a mix needs at least one channel"));
        }
        Ok(())
    }
}

/// Mix every audio-carrying clip of a sequence over `span`.
///
/// The result is interleaved f32, exactly `span.duration().sample_round(spec.rate) *
/// spec.channels` long — a caller can concatenate the mixes of adjacent spans and get the
/// same samples as one long mix.
#[allow(clippy::too_many_arguments)]
pub fn mix_span(
    project: &Project,
    sequence: &SequenceId,
    span: Span,
    spec: MixSpec,
    tool: &Toolchain,
    assets: &AssetStore,
    paths: &ProjectPaths,
) -> Result<Vec<f32>> {
    mix_tracks(project, sequence, None, span, spec, tool, assets, paths)
}

/// As [`mix_span`], restricted to a set of tracks.
///
/// `audio.normalize` needs the level of one track rather than of the finished mix, and
/// muting the rest of the sequence to find out would be a mutation with a measurement
/// hidden inside it.
#[allow(clippy::too_many_arguments)]
pub fn mix_tracks(
    project: &Project,
    sequence: &SequenceId,
    tracks: Option<&[TrackId]>,
    span: Span,
    spec: MixSpec,
    tool: &Toolchain,
    assets: &AssetStore,
    paths: &ProjectPaths,
) -> Result<Vec<f32>> {
    spec.validate()?;
    let mixer = Mixer {
        project,
        tool,
        assets,
        paths,
        spec,
    };
    mixer.sequence(sequence, tracks, span, true, &[])
}

/// Whether anything in `span` reaches the mix at all.
///
/// This is not "does the sequence have an audio track": a clip on a *video* track plays
/// its asset's audio, which is what "a video has sound" means. A renderer that asks the
/// narrower question ships a silent file for the commonest timeline there is — one talking
/// head on V1 — so the predicate lives next to the mixer and both use the same rule.
pub fn has_audio(project: &Project, sequence: &SequenceId, span: Span) -> Result<bool> {
    let seq = project.sequence(sequence)?;
    let soloed = seq
        .tracks
        .iter()
        .any(|track| track.solo && track.kind != TrackKind::Caption);
    for track in &seq.tracks {
        if track.kind == TrackKind::Caption || track.muted || (soloed && !track.solo) {
            continue;
        }
        for clip in &track.clips {
            if !clip.enabled || !clip.span().overlaps(&span) {
                continue;
            }
            if clip_carries_audio(project, seq, track, clip)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Whether one clip puts samples into the mix.
///
/// A clip on a video track plays the audio of its asset *unless* its audio was detached,
/// in which case an audio clip elsewhere is already playing those samples and counting
/// them twice would double the level of every detached shot.
pub fn clip_carries_audio(
    project: &Project,
    sequence: &Sequence,
    track: &Track,
    clip: &Clip,
) -> Result<bool> {
    match &clip.source {
        Source::Asset { asset, .. } => {
            if project.asset(asset)?.probe.audio.is_none() {
                return Ok(false);
            }
            Ok(!(track.kind == TrackKind::Video && detached(sequence, clip)))
        }
        Source::Sequence { .. } => Ok(true),
        Source::Generator { generator, .. } => Ok(*generator == Generator::Tone),
        Source::Title { .. } | Source::Color { .. } | Source::Image { .. } => Ok(false),
    }
}

/// One clip's contribution: which output frames it occupies and where it came from.
struct Job<'p> {
    track: &'p Track,
    clip: &'p Clip,
    /// Timeline span shared by the clip and the requested span.
    overlap: Span,
    /// First output frame this clip writes.
    at: usize,
    /// Output frames written.
    frames: usize,
}

struct Mixer<'a> {
    project: &'a Project,
    tool: &'a Toolchain,
    assets: &'a AssetStore,
    paths: &'a ProjectPaths,
    spec: MixSpec,
}

impl Mixer<'_> {
    fn channels(&self) -> usize {
        usize::from(self.spec.channels)
    }

    /// Output frame index of a timeline instant, relative to a span start. This one line is
    /// the sample-exactness guarantee: it is exact rational arithmetic rounded once, never
    /// an accumulated sum of per-clip lengths.
    fn frame_of(&self, at: Time, origin: Time) -> i64 {
        (at - origin).sample_round(self.spec.rate)
    }

    fn frames_in(&self, span: Span) -> usize {
        span.duration().sample_round(self.spec.rate).max(0) as usize
    }

    /// Mix one sequence by id. `stack` carries the sequences already being mixed further
    /// up, so a nested sequence that reaches itself is an error rather than a stack
    /// overflow.
    fn sequence(
        &self,
        seq_id: &SequenceId,
        only: Option<&[TrackId]>,
        span: Span,
        ducking: bool,
        stack: &[SequenceId],
    ) -> Result<Vec<f32>> {
        if stack.contains(seq_id) {
            return Err(Error::op(format!(
                "sequence '{seq_id}' contains itself; a nested sequence cannot reference an ancestor"
            )));
        }
        if stack.len() >= MAX_NESTING {
            return Err(Error::op(format!(
                "sequence nesting deeper than {MAX_NESTING} at '{seq_id}'"
            )));
        }
        let sequence = self.project.sequence(seq_id)?;
        let mut nested = Vec::with_capacity(stack.len() + 1);
        nested.extend_from_slice(stack);
        nested.push(seq_id.clone());
        self.tracks_of(sequence, only, span, ducking, &nested)
    }

    /// Mix the selected tracks of an already-resolved sequence.
    ///
    /// `stack` already names this sequence. Reading another track of the sequence being
    /// mixed — which is exactly what a ducking sidechain does — is not nesting, so it must
    /// not trip the cycle guard.
    fn tracks_of(
        &self,
        sequence: &Sequence,
        only: Option<&[TrackId]>,
        span: Span,
        ducking: bool,
        stack: &[SequenceId],
    ) -> Result<Vec<f32>> {
        let channels = self.channels();
        let frames = self.frames_in(span);
        let mut out = vec![0.0f32; frames * channels];
        if frames == 0 {
            return Ok(out);
        }
        // Solo is exclusive across the whole sequence: one soloed track silences every
        // other audio-carrying track, which is what the button means on a mixer.
        let soloed = sequence
            .tracks
            .iter()
            .any(|track| track.solo && track.kind != TrackKind::Caption);

        let mut jobs: Vec<Job<'_>> = Vec::new();
        for track in &sequence.tracks {
            if track.kind == TrackKind::Caption || track.muted || (soloed && !track.solo) {
                continue;
            }
            if only.is_some_and(|wanted| !wanted.contains(&track.id)) {
                continue;
            }
            for clip in &track.clips {
                if !clip.enabled || !self.carries_audio(sequence, track, clip)? {
                    continue;
                }
                let Some(overlap) = clip.span().intersect(&span) else {
                    continue;
                };
                let at = self.frame_of(overlap.start, span.start).max(0) as usize;
                let end = (self.frame_of(overlap.end, span.start).max(0) as usize).min(frames);
                if end <= at {
                    continue;
                }
                jobs.push(Job {
                    track,
                    clip,
                    overlap,
                    at,
                    frames: end - at,
                });
            }
        }
        if jobs.is_empty() {
            return Ok(out);
        }

        // Each job spawns its own ffmpeg decode, so the wall clock of a dense timeline is
        // process startup, not arithmetic. Rendering is pure, so it parallelizes; the sum
        // afterwards is sequential and therefore bit-identical run to run.
        let rendered = jobs
            .par_iter()
            .map(|job| self.render_clip(sequence, job, ducking, stack))
            .collect::<Result<Vec<Vec<f32>>>>()?;

        for (job, buffer) in jobs.iter().zip(rendered) {
            let base = job.at * channels;
            for (slot, value) in out[base..base + job.frames * channels]
                .iter_mut()
                .zip(buffer)
            {
                *slot += value;
            }
        }
        Ok(out)
    }

    /// Whether a clip puts samples into the mix.
    fn carries_audio(&self, sequence: &Sequence, track: &Track, clip: &Clip) -> Result<bool> {
        clip_carries_audio(self.project, sequence, track, clip)
    }

    /// Render one clip's contribution, with every gain stage applied.
    fn render_clip(
        &self,
        sequence: &Sequence,
        job: &Job<'_>,
        ducking: bool,
        stack: &[SequenceId],
    ) -> Result<Vec<f32>> {
        let channels = self.channels();
        let clip = job.clip;
        let mut buffer = self.source_samples(clip, job, stack)?;
        debug_assert_eq!(buffer.len(), job.frames * channels);

        self.apply_fades(&mut buffer, clip, job);
        apply_gain(&mut buffer, db_to_gain(clip.gain_db));
        apply_pan(&mut buffer, clip.pan, channels);
        if ducking {
            if let Some(duck) = &clip.ducking {
                let envelope = self.duck_envelope(sequence, job, duck, stack)?;
                for (frame, gain) in envelope.iter().enumerate() {
                    let base = frame * channels;
                    for slot in &mut buffer[base..base + channels] {
                        *slot *= gain;
                    }
                }
            }
        }
        apply_gain(&mut buffer, db_to_gain(job.track.gain_db));
        apply_pan(&mut buffer, job.track.pan, channels);
        Ok(buffer)
    }

    /// Decode the source material behind a clip's visible span, with speed and reverse
    /// applied, resized to exactly the output frames the job occupies.
    fn source_samples(&self, clip: &Clip, job: &Job<'_>, stack: &[SequenceId]) -> Result<Vec<f32>> {
        let channels = self.channels();
        let local_start = job.overlap.start - clip.start;
        let local_end = job.overlap.end - clip.start;
        // A reversed clip consumes the same source material, read back to front, so the
        // source instant at the *end* of the visible window is the earliest one needed.
        let (src_start, src_end) = if clip.reverse {
            let consumed = clip.duration * clip.speed;
            (
                clip.source_in + consumed - local_end * clip.speed,
                clip.source_in + consumed - local_start * clip.speed,
            )
        } else {
            (
                clip.source_in + local_start * clip.speed,
                clip.source_in + local_end * clip.speed,
            )
        };
        let source_span = Span::new(src_start.max(Time::ZERO), src_end.max(Time::ZERO));

        let mut buffer = match &clip.source {
            Source::Asset { asset, .. } => {
                let path = self.media_path(asset)?;
                dvs_media::decode_audio(
                    self.tool,
                    &path,
                    source_span,
                    self.spec.rate,
                    self.spec.channels,
                )?
            }
            Source::Sequence { sequence } => {
                self.sequence(sequence, None, source_span, true, stack)?
            }
            Source::Generator { generator, params } => {
                debug_assert_eq!(*generator, Generator::Tone);
                tone(params, source_span, self.spec)
            }
            other => {
                return Err(Error::op(format!(
                    "source {} carries no audio",
                    other.describe()
                )))
            }
        };
        if clip.reverse {
            reverse_frames(&mut buffer, channels);
        }
        if clip.speed.is_one() {
            // Only the rounding of two independent span endpoints can differ here, and by
            // at most one frame; padding beats resampling a whole clip for one sample.
            buffer.resize(job.frames * channels, 0.0);
            Ok(buffer)
        } else {
            resample(&buffer, channels, job.frames)
        }
    }

    /// Where the bytes of an asset live: the store copy, or the proxy when the original is
    /// gone. A project handed over without its `assets/` directory still mixes from its
    /// cache instead of failing outright, and the caller hears reduced quality rather than
    /// nothing.
    fn media_path(&self, asset: &AssetId) -> Result<PathBuf> {
        let asset = self.project.asset(asset)?;
        match self.assets.find(&asset.hash) {
            Ok(path) => Ok(path),
            Err(missing) => {
                let proxy = asset
                    .proxy
                    .as_deref()
                    .map(|stored| self.paths.resolve(stored))
                    .filter(|path| path.exists());
                proxy.ok_or(missing)
            }
        }
    }

    /// Clip fades, evaluated with the document's own [`Clip::fade_gain`] so an audio fade
    /// and the matching video fade cannot disagree.
    ///
    /// Only the frames a fade actually covers are visited. Walking a ten-minute clip to
    /// discover that 99.9% of it has unity gain would cost one exact-rational instant per
    /// sample for nothing.
    fn apply_fades(&self, buffer: &mut [f32], clip: &Clip, job: &Job<'_>) {
        let channels = self.channels();
        let mut regions = [
            Span::new(clip.start, clip.start + clip.fade_in),
            Span::new(clip.end() - clip.fade_out, clip.end()),
        ];
        // Fades longer than the clip overlap. `fade_gain` already multiplies both factors
        // at such an instant, so the overlapping frames are visited once as a single
        // region rather than twice with the gain squared.
        if !regions[0].is_empty() && !regions[1].is_empty() && regions[0].overlaps(&regions[1]) {
            regions = [
                Span::new(regions[0].start, regions[1].end),
                Span::new(Time::ZERO, Time::ZERO),
            ];
        }
        for region in regions {
            if region.is_empty() {
                continue;
            }
            let Some(hit) = region.intersect(&job.overlap) else {
                continue;
            };
            let first = self.frame_of(hit.start, job.overlap.start).max(0) as usize;
            let last = (self.frame_of(hit.end, job.overlap.start).max(0) as usize).min(job.frames);
            for frame in first..last {
                let at = job.overlap.start + Time::from_samples(frame as i64, self.spec.rate);
                let gain = clip.fade_gain(at);
                let base = frame * channels;
                for slot in &mut buffer[base..base + channels] {
                    *slot *= gain;
                }
            }
        }
    }

    /// Per-frame gain multipliers implementing this clip's ducking over its visible window.
    ///
    /// The sidechain is looked *ahead* by the attack time: the trigger track is examined up
    /// to `attack` beyond each frame, so the gain reduction begins one attack before speech
    /// and has reached full depth by the first syllable. Without the lookahead the first
    /// word is heard over an undipped music bed and the duck audibly chases it — the
    /// artefact every "my ducking sounds late" complaint describes.
    ///
    /// The window also starts `attack + release` early so the ramp state entering the span
    /// is the converged one. That is what makes a mix of `[5s, 10s)` identical to the same
    /// slice of a mix of `[0s, 10s)`.
    fn duck_envelope(
        &self,
        sequence: &Sequence,
        job: &Job<'_>,
        duck: &Ducking,
        stack: &[SequenceId],
    ) -> Result<Vec<f32>> {
        if duck.against == job.track.id {
            return Err(Error::op(format!(
                "track '{}' ducks against itself, which has no defined level",
                job.track.name
            )));
        }
        let trigger = sequence.track(&duck.against)?;
        let rate = self.spec.rate;
        let attack_frames = duck.attack.sample_round(rate).max(0) as usize;
        let release_frames = duck.release.sample_round(rate).max(0) as usize;
        let preroll = duck.attack + duck.release;
        let window = Span::new(
            (job.overlap.start - preroll).max(Time::ZERO),
            job.overlap.end + duck.attack,
        );
        let offset = self.frame_of(job.overlap.start, window.start).max(0) as usize;

        // Rendered with ducking switched off: a sidechain that listened to a ducked signal
        // would be a feedback loop with no fixed point.
        let signal = self.tracks_of(
            sequence,
            Some(std::slice::from_ref(&trigger.id)),
            window,
            false,
            stack,
        )?;
        let level = window_level_db(&signal, self.channels(), rate);

        let threshold = f64::from(duck.threshold);
        let depth = f64::from(duck.by);
        let attack_step = if attack_frames == 0 {
            f64::INFINITY
        } else {
            depth.abs() / attack_frames as f64
        };
        let release_step = if release_frames == 0 {
            f64::INFINITY
        } else {
            depth.abs() / release_frames as f64
        };

        let total = level.len();
        // `armed[i]` is "the trigger is above threshold somewhere in [i, i + attack]".
        // Computed with a sliding count rather than by rescanning the window at every
        // frame: at 48 kHz with a 200 ms attack that rescan would be ten thousand
        // comparisons per sample, billions for a few seconds of timeline.
        let mut armed = vec![false; total];
        let mut hot = 0usize;
        for frame in 0..total {
            if frame == 0 {
                for look in 0..=attack_frames.min(total - 1) {
                    if level[look] >= threshold {
                        hot += 1;
                    }
                }
            } else {
                if level[frame - 1] >= threshold {
                    hot -= 1;
                }
                let entering = frame + attack_frames;
                if entering < total && level[entering] >= threshold {
                    hot += 1;
                }
            }
            armed[frame] = hot > 0;
        }

        let mut envelope = Vec::with_capacity(job.frames);
        // Starting converged: the window begins a full attack-plus-release before the
        // clip, so whatever state the ramp is in by then is the steady one.
        let mut current = if armed.first().copied().unwrap_or(false) {
            depth
        } else {
            0.0
        };
        for frame in 0..total {
            let engaging = armed[frame];
            let target = if engaging { depth } else { 0.0 };
            let step = if engaging { attack_step } else { release_step };
            current = if current < target {
                (current + step).min(target)
            } else {
                (current - step).max(target)
            };
            if frame >= offset && envelope.len() < job.frames {
                envelope.push(db_to_gain(current as f32));
            }
        }
        // A window clipped at time zero, or a trigger track that ends early, leaves the
        // tail unspecified; unity is the only honest answer there.
        envelope.resize(job.frames, 1.0);
        Ok(envelope)
    }
}

/// Whether this clip's audio has been moved onto an audio track of its own.
fn detached(sequence: &Sequence, clip: &Clip) -> bool {
    clip.link.as_ref().is_some_and(|linked| {
        sequence
            .find_clip(linked)
            .is_some_and(|(track, _)| track.kind == TrackKind::Audio)
    })
}

fn apply_gain(buffer: &mut [f32], gain: f32) {
    if gain == 1.0 {
        return;
    }
    for sample in buffer {
        *sample *= gain;
    }
}

/// Constant-power pan across the first two channels.
///
/// The law is `cos`/`sin` over a quarter turn, normalized so that center is unity: a source
/// swept from hard left to hard right keeps constant total power, and a clip left at center
/// is untouched rather than quietly pushed 3 dB down. In the classic statement of the law
/// the center sits 3 dB below a hard-panned channel, which is exactly the `√2` factor
/// folded in here; the audible consequence is that pan changes position without changing
/// loudness. Mono output ignores pan, and channels beyond the first two are left alone.
fn apply_pan(buffer: &mut [f32], pan: f32, channels: usize) {
    if pan == 0.0 || channels < 2 {
        return;
    }
    let angle = (f64::from(pan.clamp(-1.0, 1.0)) + 1.0) * (std::f64::consts::FRAC_PI_4);
    let left = (std::f64::consts::SQRT_2 * angle.cos()) as f32;
    let right = (std::f64::consts::SQRT_2 * angle.sin()) as f32;
    for frame in buffer.chunks_exact_mut(channels) {
        frame[0] *= left;
        frame[1] *= right;
    }
}

fn reverse_frames(buffer: &mut [f32], channels: usize) {
    let frames = buffer.len() / channels;
    for frame in 0..frames / 2 {
        let (front, back) = (frame * channels, (frames - 1 - frame) * channels);
        for channel in 0..channels {
            buffer.swap(front + channel, back + channel);
        }
    }
}

/// Resample an interleaved buffer to exactly `want` frames.
///
/// Used for retimed clips, where the ratio is the reciprocal of the clip speed. A
/// windowed-sinc resampler costs more than dropping samples and is the difference between a
/// slowed-down voice and an aliased one.
fn resample(input: &[f32], channels: usize, want: usize) -> Result<Vec<f32>> {
    let have = input.len() / channels;
    if want == 0 {
        return Ok(Vec::new());
    }
    if have == 0 {
        return Ok(vec![0.0; want * channels]);
    }
    let ratio = want as f64 / have as f64;
    let parameters = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: Some(0.95),
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        1.0,
        &parameters,
        1024.min(have.max(1)),
        channels,
        FixedAsync::Input,
    )
    .map_err(|error| Error::op(format!("cannot build a resampler for speed change: {error}")))?;
    let adapter = InterleavedSlice::new(input, channels, have)
        .map_err(|error| Error::op(format!("bad resampler input buffer: {error}")))?;
    let output = resampler
        .process_all(&adapter, have, None)
        .map_err(|error| Error::op(format!("resampling failed: {error}")))?;
    let mut data = output.take_data();
    data.resize(want * channels, 0.0);
    Ok(data)
}

/// A 1 kHz (by default) sine for the `tone` generator, phase-locked to source time so that
/// the same instant of the same generator clip always produces the same sample.
fn tone(params: &serde_json::Map<String, serde_json::Value>, span: Span, spec: MixSpec) -> Vec<f32> {
    let number = |key: &str| params.get(key).and_then(serde_json::Value::as_f64);
    let frequency = number("freq").or_else(|| number("frequency")).unwrap_or(1000.0);
    let amplitude = number("amplitude").unwrap_or(0.5);
    let channels = usize::from(spec.channels);
    let frames = span.duration().sample_round(spec.rate).max(0) as usize;
    let origin = span.start.as_secs_f64();
    let step = 1.0 / f64::from(spec.rate);
    let mut out = vec![0.0f32; frames * channels];
    for (index, frame) in out.chunks_exact_mut(channels).enumerate() {
        let value = ((origin + index as f64 * step) * frequency * TAU).sin() * amplitude;
        frame.fill(value as f32);
    }
    out
}

/// Short-window RMS of an interleaved buffer, in dBFS, one value per frame.
///
/// The window is centered, so an onset is detected at the sample it happens on rather than
/// half a window later. Per-sample level would be useless here: a sine crosses zero 2000
/// times a second and would read as silence at every crossing.
fn window_level_db(buffer: &[f32], channels: usize, rate: u32) -> Vec<f64> {
    let frames = buffer.len() / channels.max(1);
    if frames == 0 {
        return Vec::new();
    }
    let mut energy = Vec::with_capacity(frames + 1);
    energy.push(0.0f64);
    let mut running = 0.0f64;
    for frame in buffer.chunks_exact(channels) {
        let sum: f64 = frame.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
        running += sum / channels as f64;
        energy.push(running);
    }
    let half = ((i64::from(rate) * DETECT_WINDOW_MS / 1000) / 2).max(1) as usize;
    (0..frames)
        .map(|frame| {
            let start = frame.saturating_sub(half);
            let end = (frame + half).min(frames);
            let mean = (energy[end] - energy[start]) / (end - start).max(1) as f64;
            if mean <= 0.0 {
                crate::SILENCE_FLOOR_DB
            } else {
                (10.0 * mean.log10()).max(crate::SILENCE_FLOOR_DB)
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod fixture {
    //! Shared test scaffolding: a real project directory with real media in it.
    //!
    //! The mixer's contract is about sample positions, so the tests synthesize actual WAV
    //! files with ffmpeg and read the samples back rather than mocking a decoder — a mocked
    //! decoder cannot be off by one the way a real `-ss` seek can.

    use super::*;
    use dvs_core::project::{
        Asset, AssetKind, AudioStream, Probe, Project, Sequence, Source, Track, TrackKind,
    };
    use dvs_core::time::Fps;
    use dvs_core::vfs::FsVfs;
    use std::path::Path;
    use std::process::Stdio;
    use std::sync::LazyLock;

    pub const RATE: u32 = 48_000;

    static TOOL: LazyLock<Toolchain> =
        LazyLock::new(|| Toolchain::discover().expect("ffmpeg is required for the audio tests"));

    pub fn tool() -> &'static Toolchain {
        &TOOL
    }

    /// Render an ffmpeg audio filter expression to a 32-bit float WAV. WAV and f32 because
    /// a compressed codec adds encoder priming delay, which would make a sample-position
    /// assertion a test of the codec instead of a test of the mixer.
    pub fn wav(path: &Path, lavfi: &str) {
        let output = tool()
            .ffmpeg_command()
            .args(["-f", "lavfi", "-i", lavfi])
            .args(["-c:a", "pcm_f32le"])
            .arg(path)
            .stderr(Stdio::piped())
            .output()
            .expect("ffmpeg runs");
        assert!(
            output.status.success(),
            "ffmpeg could not render '{lavfi}': {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A project directory with an asset store, plus the sequence under test.
    pub struct Fixture {
        pub dir: tempfile::TempDir,
        pub paths: ProjectPaths,
        pub assets: AssetStore,
        pub project: Project,
        pub sequence: SequenceId,
    }

    impl Fixture {
        pub fn new(channels: u16) -> Fixture {
            let dir = tempfile::tempdir().expect("tempdir");
            let paths = ProjectPaths::new(dir.path());
            let assets = AssetStore::new(paths.assets_dir(), FsVfs::shared());
            let mut project = Project::new(
                "test",
                Fps::new(30, 1).expect("30 fps"),
                [640, 360],
                RATE,
            );
            let sequence = project.active_sequence.clone();
            let seq = project.sequence_mut(&sequence).expect("active sequence");
            seq.channels = channels;
            seq.tracks.clear();
            Fixture {
                dir,
                paths,
                assets,
                project,
                sequence,
            }
        }

        pub fn spec(&self) -> MixSpec {
            MixSpec::of(self.project.sequence(&self.sequence).expect("sequence"))
        }

        pub fn seq(&mut self) -> &mut Sequence {
            let id = self.sequence.clone();
            self.project.sequence_mut(&id).expect("sequence")
        }

        /// Synthesize a WAV from a lavfi expression, import it, and register the asset.
        pub fn audio_asset(&mut self, name: &str, lavfi: &str, duration: Time, channels: u16) -> AssetId {
            let file = self.dir.path().join(format!("{name}.wav"));
            wav(&file, lavfi);
            let hash = self.assets.import_path(&file).expect("import");
            let id = AssetId::from_raw(format!("ast_{name}"));
            let asset = Asset {
                id: id.clone(),
                name: format!("{name}.wav"),
                hash,
                kind: AssetKind::Audio,
                probe: Probe {
                    duration,
                    video: None,
                    audio: Some(AudioStream {
                        stream_index: 0,
                        rate: RATE,
                        channels,
                        codec: "pcm_f32le".to_string(),
                        bit_rate: None,
                    }),
                    vfr: false,
                    container: "wav".to_string(),
                },
                proxy: None,
                source_path: None,
                imported: chrono_now(),
                provenance: None,
            };
            self.project.assets.insert(id.clone(), asset);
            id
        }

        /// Append an audio track carrying one clip of `asset` starting at `start`.
        pub fn audio_track(&mut self, name: &str, asset: &AssetId, start: Time, duration: Time) -> TrackId {
            let mut track = Track::new(name, TrackKind::Audio);
            track.id = TrackId::from_raw(format!("trk_{name}"));
            let mut clip = Clip::new(
                Source::Asset {
                    asset: asset.clone(),
                    stream: None,
                },
                start,
                duration,
            );
            clip.id = dvs_core::ids::ClipId::from_raw(format!("clp_{name}"));
            track.clips.push(clip);
            let id = track.id.clone();
            self.seq().tracks.push(track);
            id
        }

        pub fn clip_mut(&mut self, track: &TrackId) -> &mut Clip {
            self.seq()
                .track_mut(track)
                .expect("track")
                .clips
                .first_mut()
                .expect("clip")
        }

        pub fn mix(&self, span: Span) -> Vec<f32> {
            mix_span(
                &self.project,
                &self.sequence,
                span,
                self.spec(),
                tool(),
                &self.assets,
                &self.paths,
            )
            .expect("mix")
        }
    }

    /// `Asset::imported` needs a timestamp and the fixture does not care which.
    fn chrono_now() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now()
    }

    pub fn secs(num: i64, den: i64) -> Time {
        Time::new(num, den).expect("valid time")
    }

    /// Peak absolute sample of one channel.
    pub fn peak(buffer: &[f32], channel: usize, channels: usize) -> f32 {
        buffer
            .chunks_exact(channels)
            .map(|frame| frame[channel].abs())
            .fold(0.0f32, f32::max)
    }

    /// Mean square of an interleaved buffer over all channels.
    pub fn power(buffer: &[f32]) -> f64 {
        if buffer.is_empty() {
            return 0.0;
        }
        buffer.iter().map(|s| f64::from(*s) * f64::from(*s)).sum::<f64>() / buffer.len() as f64
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    #[test]
    fn output_length_is_exact_for_a_fractional_span() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=2:s=48000:c=stereo",
            Time::from_secs(2),
            2,
        );
        fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(2));
        // 1/7 s is not a whole number of samples at 48 kHz.
        let span = Span::new(Time::ZERO, secs(1, 7));
        let mixed = fixture.mix(span);
        let expected = span.duration().sample_round(RATE) as usize * 2;
        assert_eq!(mixed.len(), expected);
        assert_eq!(expected, 6857 * 2, "48000/7 rounds to 6857 samples");
    }

    #[test]
    fn a_clip_at_one_third_of_a_second_starts_at_sample_16000() {
        let mut fixture = Fixture::new(2);
        // A click track: one sample-wide bursts are hard to catch, so this is a constant
        // level whose first non-zero sample is unambiguously the clip's first sample.
        let asset = fixture.audio_asset(
            "click",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        fixture.audio_track("A1", &asset, secs(1, 3), Time::from_secs(1));
        let mixed = fixture.mix(Span::new(Time::ZERO, Time::from_secs(1)));
        let first = mixed
            .chunks_exact(2)
            .position(|frame| frame[0].abs() > 1e-6)
            .expect("the clip is audible");
        assert_eq!(first, 16_000, "1/3 s at 48 kHz is sample 16000 exactly");
    }

    #[test]
    fn overlapping_clips_sum_rather_than_replace() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "half",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        fixture.audio_track("A2", &asset, Time::ZERO, Time::from_secs(1));
        let mixed = fixture.mix(Span::new(Time::ZERO, secs(1, 2)));
        let middle = mixed[10_000];
        assert!(
            (middle - 1.0).abs() < 0.01,
            "two +0.5 clips must sum to +1.0, got {middle}"
        );
    }

    #[test]
    fn a_muted_track_contributes_nothing() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "half",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        fixture.audio_track("A2", &asset, Time::ZERO, Time::from_secs(1));
        fixture.seq().track_mut(&track).expect("track").muted = true;
        let mixed = fixture.mix(Span::new(Time::ZERO, secs(1, 2)));
        assert!(
            (mixed[10_000] - 0.5).abs() < 0.01,
            "only the unmuted track should be heard"
        );
    }

    #[test]
    fn a_soloed_track_silences_its_siblings() {
        let mut fixture = Fixture::new(2);
        let loud = fixture.audio_asset(
            "loud",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let quiet = fixture.audio_asset(
            "quiet",
            "aevalsrc=0.25:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        fixture.audio_track("A1", &loud, Time::ZERO, Time::from_secs(1));
        let solo = fixture.audio_track("A2", &quiet, Time::ZERO, Time::from_secs(1));
        fixture.seq().track_mut(&solo).expect("track").solo = true;
        let mixed = fixture.mix(Span::new(Time::ZERO, secs(1, 2)));
        assert!(
            (mixed[10_000] - 0.25).abs() < 0.01,
            "the soloed track alone should be heard, got {}",
            mixed[10_000]
        );
    }

    #[test]
    fn fade_in_ramps_from_zero_over_exactly_the_fade_length() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        fixture.clip_mut(&track).fade_in = secs(1, 4);
        let mixed = fixture.mix(Span::new(Time::ZERO, secs(1, 2)));
        let fade_frames = secs(1, 4).sample_round(RATE) as usize;

        assert!(mixed[0].abs() < 1e-6, "a fade in starts at silence");
        let mut previous = -1.0f32;
        for frame in (0..fade_frames).step_by(64) {
            let value = mixed[frame * 2];
            assert!(
                value >= previous - 1e-6,
                "fade envelope dipped at frame {frame}: {value} after {previous}"
            );
            previous = value;
        }
        let at_end = mixed[fade_frames * 2];
        assert!(
            (at_end - 0.5).abs() < 0.01,
            "the fade must reach unity at exactly its length, got {at_end}"
        );
        let midpoint = mixed[(fade_frames / 2) * 2];
        assert!(
            (midpoint - 0.25).abs() < 0.01,
            "halfway through a linear fade is half gain, got {midpoint}"
        );
    }

    #[test]
    fn fade_out_ramps_to_silence_at_the_end_of_the_clip() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        fixture.clip_mut(&track).fade_out = secs(1, 4);
        let mixed = fixture.mix(Span::new(Time::ZERO, Time::from_secs(1)));
        let frames = mixed.len() / 2;
        let fade_start = frames - secs(1, 4).sample_round(RATE) as usize;

        assert!(
            (mixed[(fade_start - 10) * 2] - 0.5).abs() < 0.01,
            "the clip is at unity right up to the fade"
        );
        assert!(
            (mixed[(fade_start + (frames - fade_start) / 2) * 2] - 0.25).abs() < 0.01,
            "halfway through the fade out is half gain"
        );
        assert!(
            mixed[(frames - 1) * 2].abs() < 0.01,
            "the last sample of a faded clip is silent, got {}",
            mixed[(frames - 1) * 2]
        );
    }

    #[test]
    fn fades_that_meet_in_the_middle_are_not_applied_twice() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        // Each fade covers the whole clip, so every frame is inside both of them.
        {
            let clip = fixture.clip_mut(&track);
            clip.fade_in = Time::from_secs(1);
            clip.fade_out = Time::from_secs(1);
        }
        let mixed = fixture.mix(Span::new(Time::ZERO, Time::from_secs(1)));
        let middle = mixed[(RATE as usize / 2) * 2];
        // fade_gain multiplies the two ramps once: 0.5 in x 0.5 out x 0.5 source.
        assert!(
            (middle - 0.125).abs() < 0.01,
            "overlapping fades must be applied once, expected 0.125, got {middle}"
        );
    }

    #[test]
    fn minus_six_db_clip_gain_halves_amplitude() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        fixture.clip_mut(&track).gain_db = -6.0;
        let mixed = fixture.mix(Span::new(Time::ZERO, secs(1, 4)));
        let value = mixed[1000];
        let expected = 0.5 * db_to_gain(-6.0);
        assert!(
            (value / expected - 1.0).abs() < 0.01,
            "−6 dB should give {expected}, got {value}"
        );
    }

    #[test]
    fn constant_power_pan_moves_energy_without_changing_it() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "dc",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        let span = Span::new(Time::ZERO, secs(1, 4));

        let center = fixture.mix(span);
        let reference = power(&center);

        fixture.clip_mut(&track).pan = -1.0;
        let left = fixture.mix(span);
        assert!(
            peak(&left, 1, 2) < 1e-6,
            "hard left must leave the right channel silent"
        );
        assert!(peak(&left, 0, 2) > 0.5, "hard left must keep the left channel");

        let ratio_db = 10.0 * (power(&left) / reference).log10();
        assert!(
            ratio_db.abs() < 0.5,
            "constant-power pan changed total power by {ratio_db:.2} dB"
        );

        fixture.clip_mut(&track).pan = 0.0;
        let recentered = fixture.mix(span);
        let center_db = 10.0 * (power(&recentered) / reference).log10();
        assert!(
            center_db.abs() < 0.5,
            "center pan changed total power by {center_db:.2} dB"
        );
    }

    #[test]
    fn ducking_dips_the_bed_before_speech_and_recovers_after_release() {
        let mut fixture = Fixture::new(2);
        let music = fixture.audio_asset(
            "music",
            "aevalsrc=0.5:d=6:s=48000:c=stereo",
            Time::from_secs(6),
            2,
        );
        // Speech: silence, then a loud second, then silence again.
        let speech = fixture.audio_asset(
            "speech",
            "aevalsrc='0.5*between(t,2,3)':d=6:s=48000:c=stereo",
            Time::from_secs(6),
            2,
        );
        let bed = fixture.audio_track("A1", &music, Time::ZERO, Time::from_secs(6));
        let voice = fixture.audio_track("A2", &speech, Time::ZERO, Time::from_secs(6));
        fixture.clip_mut(&bed).ducking = Some(Ducking {
            against: voice.clone(),
            by: -12.0,
            attack: secs(1, 5),
            release: secs(1, 2),
            threshold: -30.0,
        });
        let mixed = fixture.mix(Span::new(Time::ZERO, Time::from_secs(6)));

        let bed_at = |seconds: f64| -> f32 {
            let frame = (seconds * f64::from(RATE)) as usize;
            // The speech source is +0.5 while it plays, so subtract it to see the bed.
            mixed[frame * 2]
        };

        let before = bed_at(1.0);
        assert!((before - 0.5).abs() < 0.01, "unducked bed should be 0.5, got {before}");

        // At the first sample of speech the reduction is already fully applied: that is the
        // lookahead. Subtract the speech contribution, which is also 0.5 there.
        let at_onset = bed_at(2.0) - 0.5;
        let expected = 0.5 * db_to_gain(-12.0);
        assert!(
            (at_onset / expected - 1.0).abs() < 0.05,
            "duck should already be at −12 dB at speech onset: expected {expected}, got {at_onset}"
        );

        let ducked = bed_at(2.5) - 0.5;
        assert!(
            (ducked / expected - 1.0).abs() < 0.05,
            "duck should hold at −12 dB during speech, got {ducked}"
        );

        // Still dipping halfway through the release, back to unity after it.
        let mid_release = bed_at(3.25);
        assert!(
            mid_release < 0.49 && mid_release > expected,
            "release should be part way back at 3.25 s, got {mid_release}"
        );
        let recovered = bed_at(4.0);
        assert!(
            (recovered - 0.5).abs() < 0.01,
            "bed should be back to unity after the release, got {recovered}"
        );
    }

    #[test]
    fn a_detached_video_clip_does_not_play_its_audio_twice() {
        use dvs_core::ids::ClipId;

        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "av",
            "aevalsrc=0.5:d=1:s=48000:c=stereo",
            Time::from_secs(1),
            2,
        );
        let audio = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        let audio_clip = ClipId::from_raw("clp_A1");

        let mut video = Track::new("V1", TrackKind::Video);
        video.id = TrackId::from_raw("trk_V1");
        let mut clip = Clip::new(
            Source::Asset {
                asset: asset.clone(),
                stream: None,
            },
            Time::ZERO,
            Time::from_secs(1),
        );
        clip.id = ClipId::from_raw("clp_V1");
        clip.link = Some(audio_clip);
        video.clips.push(clip);
        fixture.seq().tracks.push(video);

        let mixed = fixture.mix(Span::new(Time::ZERO, secs(1, 4)));
        assert!(
            (mixed[1000] - 0.5).abs() < 0.01,
            "detached audio must be heard once, got {}",
            mixed[1000]
        );
        assert!(!audio.as_str().is_empty());
    }

    #[test]
    fn reversed_clips_play_the_source_backwards() {
        let mut fixture = Fixture::new(1);
        // A ramp from 0 to 1 over one second.
        let asset = fixture.audio_asset("ramp", "aevalsrc='t':d=1:s=48000:c=mono", Time::from_secs(1), 1);
        let track = fixture.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        fixture.clip_mut(&track).reverse = true;
        let mixed = fixture.mix(Span::new(Time::ZERO, Time::from_secs(1)));
        assert!(
            mixed[100] > 0.9,
            "a reversed ramp starts loud, got {}",
            mixed[100]
        );
        assert!(
            mixed[mixed.len() - 100] < 0.1,
            "a reversed ramp ends quiet, got {}",
            mixed[mixed.len() - 100]
        );
    }

    #[test]
    fn a_half_speed_clip_stretches_and_transposes_its_source() {
        // A 1 kHz burst that stops after half a second of source time. At speed 1 the
        // clip's second half is silent and the pitch is 1 kHz; at half speed the burst
        // fills the whole clip and the pitch is 500 Hz. Asserting both separates "the
        // resampler ran" from "the resampler ran at the right ratio".
        let lavfi = "aevalsrc='0.5*sin(2*PI*1000*t)*lt(t,0.5)':d=2:s=48000:c=mono";
        let window = |mixed: &[f32], seconds: f64| -> f64 {
            let start = (seconds * f64::from(RATE)) as usize;
            power(&mixed[start..start + 1000])
        };
        let zero_crossings = |mixed: &[f32], from: f64, to: f64| -> f64 {
            let start = (from * f64::from(RATE)) as usize;
            let end = (to * f64::from(RATE)) as usize;
            let crossings = mixed[start..end]
                .windows(2)
                .filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0))
                .count();
            crossings as f64 / (2.0 * (to - from))
        };

        let mut full = Fixture::new(1);
        let asset = full.audio_asset("burst", lavfi, Time::from_secs(2), 1);
        full.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        let at_speed_one = full.mix(Span::new(Time::ZERO, Time::from_secs(1)));
        assert!(
            window(&at_speed_one, 0.75) < 0.001,
            "at speed 1 the burst has already ended by 0.75 s"
        );
        let pitch_one = zero_crossings(&at_speed_one, 0.1, 0.4);
        assert!(
            (pitch_one - 1000.0).abs() < 20.0,
            "unretimed pitch should be 1 kHz, measured {pitch_one:.0} Hz"
        );

        let mut half = Fixture::new(1);
        let asset = half.audio_asset("burst", lavfi, Time::from_secs(2), 1);
        let track = half.audio_track("A1", &asset, Time::ZERO, Time::from_secs(1));
        half.clip_mut(&track).speed = dvs_core::time::Rat::new(1, 2).expect("half speed");
        let at_half = half.mix(Span::new(Time::ZERO, Time::from_secs(1)));
        assert!(
            window(&at_half, 0.75) > 0.05,
            "at half speed the 0.5 s burst fills the whole 1 s clip"
        );
        let pitch_half = zero_crossings(&at_half, 0.1, 0.8);
        assert!(
            (pitch_half - 500.0).abs() < 20.0,
            "half speed should transpose 1 kHz down to 500 Hz, measured {pitch_half:.0} Hz"
        );
    }

    #[test]
    fn adjacent_spans_concatenate_into_one_mix() {
        let mut fixture = Fixture::new(2);
        let asset = fixture.audio_asset(
            "tone",
            "aevalsrc='0.4*sin(2*PI*440*t)':d=2:s=48000:c=stereo",
            Time::from_secs(2),
            2,
        );
        fixture.audio_track("A1", &asset, secs(1, 3), Time::from_secs(1));
        let whole = fixture.mix(Span::new(Time::ZERO, Time::from_secs(1)));
        let head = fixture.mix(Span::new(Time::ZERO, secs(1, 3)));
        let tail = fixture.mix(Span::new(secs(1, 3), Time::from_secs(1)));
        assert_eq!(head.len() + tail.len(), whole.len());
        let joined: Vec<f32> = head.into_iter().chain(tail).collect();
        let worst = joined
            .iter()
            .zip(&whole)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(
            worst < 1e-4,
            "mixing in two spans must equal mixing in one, worst difference {worst}"
        );
    }

    #[test]
    fn pan_law_is_constant_power_at_every_position() {
        let mut buffer = vec![1.0f32; 2];
        apply_pan(&mut buffer, -1.0, 2);
        assert!((buffer[0] - std::f32::consts::SQRT_2).abs() < 1e-5);
        assert_eq!(buffer[1], 0.0);

        for pan in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
            let mut frame = vec![1.0f32, 1.0];
            apply_pan(&mut frame, pan, 2);
            let power = f64::from(frame[0]).powi(2) + f64::from(frame[1]).powi(2);
            assert!(
                (power - 2.0).abs() < 1e-6,
                "pan {pan} changed the power of a centered source to {power}"
            );
        }
    }
}
