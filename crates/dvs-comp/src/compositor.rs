//! The compositor: a sequence and a frame index in, one frame out.
//!
//! This is the single renderer. The pixels the Tauri viewport shows, the pixels `dvs render`
//! encodes, and the pixels the digest measures all come from here — there is no second
//! preview path that can disagree with the export.
//!
//! Order of operations per frame, bottom track first:
//!
//! 1. resolve the clip covering the instant on each visible track;
//! 2. decode/rasterize/generate its source **at the size it will occupy**, so scaling
//!    happens once, in ffmpeg's SIMD scaler, rather than twice in ours;
//! 3. run its effect chain;
//! 4. place it (fit, transform, crop, rotation) into a sequence-sized layer;
//! 5. if the clip is inside a `transitionIn`, render the previous clip *past its out-point*
//!    — the handles a transition needs — and mix the two layers;
//! 6. composite the layer with the clip's blend mode, keyframed opacity and fades.
//!
//! Every per-frame value (opacity, transform, effect parameters) goes through
//! `Clip::param_at`, so animation is resolved here and nothing downstream knows about time.

use crate::effects::{self, EffectCx};
use crate::generator;
use crate::geometry::{self, Placement};
use crate::title::Rasterizer;
use crate::transition;
use dvs_core::asset::AssetStore;
use dvs_core::color::Rgba;
use dvs_core::error::{Error, Result};
use dvs_core::ids::{AssetId, ClipId, SequenceId, TrackId};
use dvs_core::paths::ProjectPaths;
use dvs_core::project::{Clip, Project, Sequence, Source, Track, TrackKind, Transform};
use dvs_core::time::{Fps, Time};
use dvs_media::decode::{DecodeSpec, VideoDecoder};
use dvs_media::{Frame, Toolchain};
use serde::Serialize;
use std::collections::HashMap;

/// Render-time knobs that are not part of the document.
#[derive(Debug, Clone)]
pub struct CompOptions {
    /// Output scale. `0.5` renders a half-size preview from the same document.
    pub scale: f64,
    /// Decode from proxies when an asset has one. Fast and lower quality: correct for
    /// scrubbing, contact sheets and lint passes, wrong for delivery.
    pub use_proxy: bool,
    /// ffmpeg scaler for decode-time resizing.
    pub scaler: &'static str,
}

impl Default for CompOptions {
    fn default() -> Self {
        CompOptions {
            scale: 1.0,
            use_proxy: false,
            scaler: "bicubic",
        }
    }
}

impl CompOptions {
    /// Preview settings: half size, proxies, cheap scaler.
    pub fn preview() -> CompOptions {
        CompOptions {
            scale: 0.5,
            use_proxy: true,
            scaler: "bilinear",
        }
    }
}

/// What a frame turned out to contain. This is the per-frame half of the digest: an agent
/// cannot see the frame, so the compositor reports what it put in it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameReport {
    pub frame: i64,
    pub at: Time,
    pub layers: Vec<LayerReport>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LayerReport {
    pub track: TrackId,
    pub clip: ClipId,
    pub name: String,
    pub source: String,
    /// `[x, y, width, height]` in output pixels.
    pub bbox: [f32; 4],
    pub opacity: f32,
    /// Ratio of destination pixels to source pixels. Above 1.0 the clip is being upscaled,
    /// which is the `upscaled` lint.
    pub scale: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transition: Option<String>,
    /// The source ran out of frames before the clip ended.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub past_source_end: bool,
}

/// Cache key for a decoder: one process per (asset, decode size, proxy choice).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DecoderKey {
    asset: AssetId,
    size: [u32; 2],
    proxy: bool,
}

pub struct Compositor<'a> {
    tool: &'a Toolchain,
    project: &'a Project,
    paths: &'a ProjectPaths,
    assets: &'a AssetStore,
    sequence: SequenceId,
    size: [u32; 2],
    fps: Fps,
    options: CompOptions,
    rasterizer: Rasterizer,
    decoders: HashMap<DecoderKey, VideoDecoder<'a>>,
    stills: HashMap<(AssetId, [u32; 2]), Frame>,
    nested: HashMap<SequenceId, Box<Compositor<'a>>>,
    /// Sequences currently being rendered, so a nesting cycle is an error rather than a
    /// stack overflow.
    stack: Vec<SequenceId>,
}

impl<'a> Compositor<'a> {
    pub fn new(
        tool: &'a Toolchain,
        project: &'a Project,
        paths: &'a ProjectPaths,
        assets: &'a AssetStore,
        sequence: &SequenceId,
        options: CompOptions,
    ) -> Result<Compositor<'a>> {
        let seq = project.sequence(sequence)?;
        let scale = options.scale.max(0.01);
        let size = [
            ((seq.size[0] as f64 * scale).round() as u32).max(2),
            ((seq.size[1] as f64 * scale).round() as u32).max(2),
        ];
        Ok(Compositor {
            tool,
            project,
            paths,
            assets,
            sequence: sequence.clone(),
            size,
            fps: seq.fps,
            options,
            rasterizer: Rasterizer::new(),
            decoders: HashMap::new(),
            stills: HashMap::new(),
            nested: HashMap::new(),
            stack: vec![sequence.clone()],
        })
    }

    pub fn size(&self) -> [u32; 2] {
        self.size
    }

    pub fn fps(&self) -> Fps {
        self.fps
    }

    pub fn sequence(&self) -> &SequenceId {
        &self.sequence
    }

    fn seq(&self) -> Result<&'a Sequence> {
        self.project.sequence(&self.sequence)
    }

    /// Total frames in the sequence, at the render frame rate.
    pub fn frame_count(&self) -> Result<i64> {
        Ok(self.seq()?.frame_count())
    }

    pub fn frame(&mut self, index: i64) -> Result<Frame> {
        Ok(self.frame_with_report(index)?.0)
    }

    pub fn frame_at(&mut self, at: Time) -> Result<Frame> {
        self.frame(at.frame_floor(self.fps))
    }

    /// Composite one frame and report what went into it.
    pub fn frame_with_report(&mut self, index: i64) -> Result<(Frame, FrameReport)> {
        let at = Time::from_frames(index, self.fps);
        let seq = self.seq()?;
        let mut out = Frame::transparent(self.size[0], self.size[1]);
        let mut report = FrameReport {
            frame: index,
            at,
            layers: Vec::new(),
        };

        for track in &seq.tracks {
            if track.hidden || !matches!(track.kind, TrackKind::Video | TrackKind::Caption) {
                continue;
            }
            if matches!(track.kind, TrackKind::Caption) {
                self.composite_captions(track, at, &mut out)?;
                continue;
            }
            let Some(index_in_track) = track.clips.iter().position(|clip| clip.span().contains(at))
            else {
                continue;
            };
            let clip = &track.clips[index_in_track];
            if !clip.enabled {
                continue;
            }
            let (layer, mut layer_report) = self.render_layer(track, clip, at)?;

            // A transition reads the outgoing clip past its out-point, which is exactly what
            // handles are for. The clips themselves never overlap on the timeline.
            let mut layer = layer;
            if let Some(trans) = &clip.transition_in {
                let elapsed = at - clip.start;
                if trans.duration.is_positive() && elapsed < trans.duration {
                    if let Some(previous) = index_in_track
                        .checked_sub(1)
                        .map(|i| &track.clips[i])
                        .filter(|previous| previous.end() == clip.start && previous.enabled)
                    {
                        let raw = elapsed.as_secs_f64() / trans.duration.as_secs_f64();
                        let progress = trans.easing.apply(raw) as f32;
                        let (outgoing, _) = self.render_layer(track, previous, at)?;
                        layer = transition::mix(&outgoing, &layer, trans, progress);
                        layer_report.transition =
                            Some(format!("{:?}", trans.kind).to_lowercase());
                    }
                }
            }

            let opacity = clip.param_at("opacity", clip.opacity as f64, at) as f32;
            let alpha = (opacity * clip.fade_gain(at)).clamp(0.0, 1.0);
            layer_report.opacity = alpha;
            composite(&mut out, &layer, clip, alpha);
            report.layers.push(layer_report);
        }
        Ok((out, report))
    }

    /// Render one clip into a sequence-sized layer.
    fn render_layer(
        &mut self,
        track: &Track,
        clip: &Clip,
        at: Time,
    ) -> Result<(Frame, LayerReport)> {
        let transform = self.transform_at(clip, at);
        let scale = [
            transform.scale[0].max(0.0001),
            transform.scale[1].max(0.0001),
        ];
        let (source_size, native_size) = self.source_size(clip)?;
        let (crop_size, crop_offset) = geometry::cropped_size(source_size, clip.crop.as_ref());
        let placement = geometry::place(crop_size, self.size, clip.fit, &transform, scale);

        // Decode/rasterize at the destination size when nothing needs the full source: one
        // resample instead of two, done by the fastest scaler in the stack.
        let target = if clip.crop.is_some() {
            crop_size
        } else {
            [
                (placement.dest.w.round().max(2.0)) as u32,
                (placement.dest.h.round().max(2.0)) as u32,
            ]
        };
        let (mut source, past_end) = self.source_frame(clip, at, target)?;

        let cx = EffectCx {
            project: self.project,
            tool: self.tool,
            paths: self.paths,
            assets: self.assets,
            clip,
            at,
            sequence_size: self.size,
        };
        effects::apply_all(&mut source, &cx)?;

        // When the source came back at the destination size, the crop rectangle is already
        // applied; otherwise blit maps the crop window.
        let (blit_offset, blit_size) = if clip.crop.is_some() {
            (crop_offset, crop_size)
        } else {
            ([0.0, 0.0], source.size())
        };
        let mut layer = Frame::transparent(self.size[0], self.size[1]);
        geometry::blit(
            &mut layer,
            &source,
            &placement,
            blit_offset,
            blit_size,
            1.0,
            dvs_core::project::Blend::Normal,
        );

        let upscale = if native_size[0] > 0 {
            placement.dest.w / native_size[0] as f32
        } else {
            1.0
        };
        let report = LayerReport {
            track: track.id.clone(),
            clip: clip.id.clone(),
            name: clip.label().to_string(),
            source: clip.source.describe(),
            bbox: [
                placement.dest.x,
                placement.dest.y,
                placement.dest.w,
                placement.dest.h,
            ],
            opacity: 1.0,
            scale: upscale,
            transition: None,
            past_source_end: past_end,
        };
        Ok((layer, report))
    }

    /// Keyframe-resolved transform at an instant. Paths are `transform.pos.x`,
    /// `transform.pos.y`, `transform.scale` (uniform), `transform.scale.x/.y` and
    /// `transform.rotation`.
    fn transform_at(&self, clip: &Clip, at: Time) -> Transform {
        let base = clip.transform;
        let uniform = clip.keyframes.contains_key("transform.scale");
        let scale_x = if uniform {
            clip.param_at("transform.scale", base.scale[0] as f64, at) as f32
        } else {
            clip.param_at("transform.scale.x", base.scale[0] as f64, at) as f32
        };
        let scale_y = if uniform {
            clip.param_at("transform.scale", base.scale[1] as f64, at) as f32
        } else {
            clip.param_at("transform.scale.y", base.scale[1] as f64, at) as f32
        };
        Transform {
            pos: [
                clip.param_at("transform.pos.x", base.pos[0] as f64, at) as f32,
                clip.param_at("transform.pos.y", base.pos[1] as f64, at) as f32,
            ],
            scale: [scale_x, scale_y],
            rotation: clip.param_at("transform.rotation", base.rotation as f64, at) as f32,
            anchor: base.anchor,
        }
    }

    /// Source size in its own pixels, and its native size for the upscale metric.
    fn source_size(&self, clip: &Clip) -> Result<([u32; 2], [u32; 2])> {
        match &clip.source {
            Source::Asset { asset, .. } | Source::Image { asset } => {
                let asset = self.project.asset(asset)?;
                let stream = asset.probe.video.as_ref().ok_or_else(|| {
                    Error::op(format!(
                        "clip '{}' uses '{}', which has no video stream",
                        clip.label(),
                        asset.name
                    ))
                })?;
                let native = stream.display_size();
                Ok((native, native))
            }
            Source::Title { title } => {
                let title = self
                    .project
                    .titles
                    .get(title)
                    .ok_or_else(|| Error::no_match("title", title.as_str(), Vec::new()))?;
                Ok((title.size, title.size))
            }
            Source::Sequence { sequence } => {
                let nested = self.project.sequence(sequence)?;
                Ok((nested.size, nested.size))
            }
            // A color, generator or tone has no intrinsic size: it fills the frame.
            Source::Color { .. } | Source::Generator { .. } => Ok((self.size, self.size)),
        }
    }

    /// The source pixels for a clip at an instant, rendered at `target` size.
    fn source_frame(
        &mut self,
        clip: &Clip,
        at: Time,
        target: [u32; 2],
    ) -> Result<(Frame, bool)> {
        let target = [target[0].max(2), target[1].max(2)];
        match &clip.source {
            Source::Color { color } => Ok((Frame::filled(target[0], target[1], *color), false)),
            Source::Generator { generator, params } => {
                let frame = generator::render(
                    *generator,
                    params,
                    target,
                    at - clip.start,
                    clip.duration,
                    self.fps,
                    &self.rasterizer,
                )?;
                Ok((frame, false))
            }
            Source::Title { title } => {
                let title = self
                    .project
                    .titles
                    .get(title)
                    .ok_or_else(|| Error::no_match("title", title.as_str(), Vec::new()))?;
                Ok((self.rasterizer.rasterize(&title.resolved_svg(), target)?, false))
            }
            Source::Image { asset } => {
                let id = asset.clone();
                let frame = self.still(&id, target)?;
                Ok((frame, false))
            }
            Source::Sequence { sequence } => {
                let frame = self.nested_frame(sequence, at - clip.start, target)?;
                Ok((frame, false))
            }
            Source::Asset { asset, .. } => {
                let asset_ref = self.project.asset(asset)?;
                // A still imported as a video source (single-frame file) decodes once.
                if asset_ref.probe.video.as_ref().and_then(|v| v.frames) == Some(1) {
                    let id = asset.clone();
                    return Ok((self.still(&id, target)?, false));
                }
                let key = DecoderKey {
                    asset: asset.clone(),
                    size: target,
                    proxy: self.options.use_proxy && asset_ref.proxy.is_some(),
                };
                let source_time = clip.source_time(at);
                if !self.decoders.contains_key(&key) {
                    let path = self.media_path(asset, key.proxy)?;
                    let stream = asset_ref.probe.video.as_ref().ok_or_else(|| {
                        Error::op(format!(
                            "clip '{}' uses '{}', which has no video stream",
                            clip.label(),
                            asset_ref.name
                        ))
                    })?;
                    let spec = DecodeSpec {
                        size: target,
                        fps: self.fps,
                        range: stream.color_range,
                        matrix: stream.color_matrix,
                        scaler: self.options.scaler,
                    };
                    self.decoders
                        .insert(key.clone(), VideoDecoder::open(self.tool, path, spec));
                }
                let decoder = self
                    .decoders
                    .get_mut(&key)
                    .expect("decoder inserted above");
                let frame = decoder.frame_at(source_time)?;
                Ok((frame, decoder.is_exhausted()))
            }
        }
    }

    fn still(&mut self, asset: &AssetId, target: [u32; 2]) -> Result<Frame> {
        if let Some(frame) = self.stills.get(&(asset.clone(), target)) {
            return Ok(frame.clone());
        }
        let path = self.media_path(asset, false)?;
        let native = dvs_media::decode::decode_image(self.tool, &path)?;
        let frame = geometry::resample(&native, target);
        self.stills.insert((asset.clone(), target), frame.clone());
        Ok(frame)
    }

    fn nested_frame(
        &mut self,
        sequence: &SequenceId,
        local: Time,
        target: [u32; 2],
    ) -> Result<Frame> {
        if self.stack.contains(sequence) {
            return Err(Error::op(format!(
                "sequence '{}' nests itself ({})",
                self.project.sequence(sequence)?.name,
                self.stack
                    .iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(" -> ")
            )));
        }
        if !self.nested.contains_key(sequence) {
            let mut child = Compositor::new(
                self.tool,
                self.project,
                self.paths,
                self.assets,
                sequence,
                self.options.clone(),
            )?;
            child.stack = {
                let mut stack = self.stack.clone();
                stack.push(sequence.clone());
                stack
            };
            self.nested.insert(sequence.clone(), Box::new(child));
        }
        let child = self
            .nested
            .get_mut(sequence)
            .expect("nested compositor inserted above");
        let frame = child.frame_at(local)?;
        Ok(geometry::resample(&frame, target))
    }

    /// Path of the media to decode: the proxy when asked for and present, else the blob.
    fn media_path(&self, asset: &AssetId, proxy: bool) -> Result<std::path::PathBuf> {
        let record = self.project.asset(asset)?;
        if proxy {
            if let Some(stored) = &record.proxy {
                let path = self.paths.resolve(stored);
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
        self.assets.find(&record.hash).map_err(|_| {
            Error::op(format!(
                "media for '{}' is missing from the asset store ({}); run `dvs asset relink`",
                record.name, record.hash
            ))
        })
    }

    /// Burn caption cues for this instant. Captions render through the same SVG path as
    /// titles so there is exactly one text renderer in the engine.
    fn composite_captions(&mut self, track: &Track, at: Time, out: &mut Frame) -> Result<()> {
        let active: Vec<&dvs_core::project::CaptionCue> = track
            .cues
            .iter()
            .filter(|cue| cue.span.contains(at))
            .collect();
        if active.is_empty() {
            return Ok(());
        }
        let default_style = dvs_core::project::CaptionStyle::named("default");
        for cue in active {
            let style = cue
                .style
                .as_ref()
                .or(track.style.as_ref())
                .and_then(|id| self.project.styles.get(id))
                .unwrap_or(&default_style);
            let svg = dvs_text::captions::cue_svg(cue, style, self.size);
            let frame = self.rasterizer.rasterize(&svg, self.size)?;
            let placement = Placement {
                dest: crate::geometry::Rect {
                    x: 0.0,
                    y: 0.0,
                    w: self.size[0] as f32,
                    h: self.size[1] as f32,
                },
                rotation: 0.0,
            };
            geometry::blit(
                out,
                &frame,
                &placement,
                [0.0, 0.0],
                frame.size(),
                1.0,
                dvs_core::project::Blend::Normal,
            );
        }
        Ok(())
    }

    /// Decoder respawn count, summed across sources. The render loop logs it because a
    /// number that grows with frame count means an access pattern is thrashing.
    pub fn seek_count(&self) -> u32 {
        self.decoders.values().map(|d| d.seek_count()).sum::<u32>()
            + self
                .nested
                .values()
                .map(|nested| nested.seek_count())
                .sum::<u32>()
    }

    /// Flatten onto the sequence background, which is what an encoder without alpha gets.
    pub fn background(&self) -> Result<Rgba> {
        Ok(self.seq()?.background)
    }
}

fn composite(out: &mut Frame, layer: &Frame, clip: &Clip, alpha: f32) {
    if alpha <= 0.0 {
        return;
    }
    let size = out.size();
    for y in 0..size[1] {
        for x in 0..size[0] {
            let src = layer.pixel(x, y);
            if src[3] <= 0.0 {
                continue;
            }
            let dst = out.pixel(x, y);
            out.set_pixel(x, y, crate::blend::blend_pixel(dst, src, clip.blend, alpha));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dvs_core::project::{
        Clip, Easing, Keyframe, Project, Source, Track, TrackKind, Transition, TransitionKind,
    };
    use dvs_core::vfs::FsVfs;
    use std::sync::Arc;

    struct Harness {
        _dir: tempfile::TempDir,
        paths: ProjectPaths,
        assets: AssetStore,
        project: Project,
    }

    fn fps() -> Fps {
        Fps::new(30, 1).unwrap()
    }

    fn harness(size: [u32; 2]) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let paths = ProjectPaths::new(dir.path());
        let assets = AssetStore::new(paths.assets_dir(), Arc::new(FsVfs));
        let project = Project::new("t", fps(), size, 48_000);
        Harness {
            _dir: dir,
            paths,
            assets,
            project,
        }
    }

    fn color_clip(color: Rgba, start: i64, duration: i64) -> Clip {
        Clip::new(
            Source::Color { color },
            Time::from_secs(start),
            Time::from_secs(duration),
        )
    }

    fn push_track(project: &mut Project, clips: Vec<Clip>) -> TrackId {
        let seq_id = project.active_sequence.clone();
        let sequence = project.sequence_mut(&seq_id).unwrap();
        let name = sequence.next_track_name(TrackKind::Video);
        let mut track = Track::new(name, TrackKind::Video);
        for clip in clips {
            track.place(clip);
        }
        let id = track.id.clone();
        sequence.tracks.push(track);
        id
    }

    #[test]
    fn upper_tracks_composite_over_lower_ones() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([16, 16]);
        push_track(&mut h.project, vec![color_clip(Rgba::opaque(255, 0, 0), 0, 2)]);
        let mut top = color_clip(Rgba::opaque(0, 0, 255), 0, 2);
        top.transform.scale = [0.5, 0.5];
        push_track(&mut h.project, vec![top]);

        let seq = h.project.active_sequence.clone();
        let mut comp = Compositor::new(
            &tool,
            &h.project,
            &h.paths,
            &h.assets,
            &seq,
            CompOptions::default(),
        )
        .unwrap();
        let (frame, report) = comp.frame_with_report(0).unwrap();
        // Center is the half-size blue clip, edges are the red one underneath.
        assert!(frame.pixel(8, 8)[2] > 0.5, "center should be blue: {:?}", frame.pixel(8, 8));
        assert!(frame.pixel(0, 0)[0] > 0.5, "edge should be red: {:?}", frame.pixel(0, 0));
        assert_eq!(report.layers.len(), 2);
        assert_eq!(report.layers[1].bbox[2], 8.0, "half scale of a 16px frame");
    }

    #[test]
    fn a_disabled_clip_and_a_hidden_track_contribute_nothing() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([8, 8]);
        let mut clip = color_clip(Rgba::WHITE, 0, 2);
        clip.enabled = false;
        push_track(&mut h.project, vec![clip]);
        let seq = h.project.active_sequence.clone();
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        assert_eq!(comp.frame(0).unwrap().alpha_coverage(), 0.0);

        let seq_id = h.project.active_sequence.clone();
        h.project.sequence_mut(&seq_id).unwrap().tracks[0].clips[0].enabled = true;
        h.project.sequence_mut(&seq_id).unwrap().tracks[0].hidden = true;
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        assert_eq!(comp.frame(0).unwrap().alpha_coverage(), 0.0);
    }

    #[test]
    fn a_gap_renders_empty_rather_than_holding_the_previous_clip() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([8, 8]);
        push_track(
            &mut h.project,
            vec![color_clip(Rgba::WHITE, 0, 1), color_clip(Rgba::WHITE, 2, 1)],
        );
        let seq = h.project.active_sequence.clone();
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        assert_eq!(comp.frame(0).unwrap().alpha_coverage(), 1.0);
        assert_eq!(comp.frame(45).unwrap().alpha_coverage(), 0.0, "1.5s is the gap");
        assert_eq!(comp.frame(60).unwrap().alpha_coverage(), 1.0);
    }

    #[test]
    fn a_dissolve_reads_the_outgoing_clip_past_its_out_point() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([8, 8]);
        let first = color_clip(Rgba::opaque(255, 0, 0), 0, 2);
        let mut second = color_clip(Rgba::opaque(0, 0, 255), 2, 2);
        second.transition_in = Some(Transition {
            kind: TransitionKind::Dissolve,
            duration: Time::from_secs(1),
            easing: Easing::Linear,
            direction: Default::default(),
            color: None,
        });
        push_track(&mut h.project, vec![first, second]);
        let seq = h.project.active_sequence.clone();
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        // Halfway through the transition (2.5s = frame 75) both clips contribute.
        let (frame, report) = comp.frame_with_report(75).unwrap();
        let pixel = frame.pixel(4, 4);
        assert!(pixel[0] > 0.1 && pixel[2] > 0.1, "both clips should show: {pixel:?}");
        assert_eq!(report.layers[0].transition.as_deref(), Some("dissolve"));
        // After the transition only the incoming clip remains.
        let after = comp.frame(105).unwrap().pixel(4, 4);
        assert!(after[0] < 0.01 && after[2] > 0.5, "{after:?}");
    }

    #[test]
    fn keyframed_opacity_is_resolved_per_frame() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([4, 4]);
        let mut clip = color_clip(Rgba::WHITE, 0, 2);
        clip.keyframes.insert(
            "opacity".into(),
            vec![
                Keyframe { at: Time::ZERO, value: 0.0, easing: Easing::Linear },
                Keyframe { at: Time::from_secs(2), value: 1.0, easing: Easing::Linear },
            ],
        );
        push_track(&mut h.project, vec![clip]);
        let seq = h.project.active_sequence.clone();
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        let start = comp.frame(0).unwrap().pixel(2, 2)[3];
        let middle = comp.frame(30).unwrap().pixel(2, 2)[3];
        let end = comp.frame(59).unwrap().pixel(2, 2)[3];
        assert!(start < 0.02, "{start}");
        assert!((middle - 0.5).abs() < 0.05, "{middle}");
        assert!(end > 0.95, "{end}");
    }

    #[test]
    fn preview_scale_renders_the_same_composition_smaller() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([32, 16]);
        push_track(&mut h.project, vec![color_clip(Rgba::WHITE, 0, 1)]);
        let seq = h.project.active_sequence.clone();
        let mut comp = Compositor::new(
            &tool,
            &h.project,
            &h.paths,
            &h.assets,
            &seq,
            CompOptions { scale: 0.5, ..CompOptions::default() },
        )
        .unwrap();
        assert_eq!(comp.size(), [16, 8]);
        assert_eq!(comp.frame(0).unwrap().size(), [16, 8]);
    }

    #[test]
    fn a_self_nesting_sequence_is_an_error_not_a_stack_overflow() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([8, 8]);
        let seq = h.project.active_sequence.clone();
        let clip = Clip::new(
            Source::Sequence { sequence: seq.clone() },
            Time::ZERO,
            Time::from_secs(1),
        );
        push_track(&mut h.project, vec![clip]);
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        let err = comp.frame(0).unwrap_err();
        assert!(err.to_string().contains("nests itself"), "{err}");
    }

    #[test]
    fn a_nested_sequence_composites_its_own_tracks() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([16, 16]);
        // Inner sequence: a white clip.
        let mut inner = dvs_core::project::Sequence::new("inner", fps(), [16, 16], 48_000);
        let mut inner_track = Track::new("V1", TrackKind::Video);
        inner_track.place(color_clip(Rgba::WHITE, 0, 2));
        inner.tracks.push(inner_track);
        let inner_id = inner.id.clone();
        h.project.sequences.insert(inner_id.clone(), inner);

        push_track(
            &mut h.project,
            vec![Clip::new(
                Source::Sequence { sequence: inner_id },
                Time::ZERO,
                Time::from_secs(1),
            )],
        );
        let seq = h.project.active_sequence.clone();
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        let frame = comp.frame(0).unwrap();
        assert!(frame.pixel(8, 8)[3] > 0.9, "nested content should be visible");
        assert!(frame.pixel(8, 8)[0] > 0.9);
    }

    #[test]
    fn real_media_decodes_at_the_frame_the_timeline_asks_for() {
        let tool = Toolchain::discover().unwrap();
        let mut h = harness([160, 120]);
        let media = h.paths.root().join("src.mp4");
        dvs_media::decode::synthesize(&tool, &media, "testsrc2", Time::from_secs(3), fps(), [160, 120])
            .unwrap();
        let hash = h.assets.import_path(&media).unwrap();
        let probed = dvs_media::probe(&tool, &media).unwrap();
        let id = AssetId::new();
        h.project.assets.insert(
            id.clone(),
            dvs_core::project::Asset {
                id: id.clone(),
                name: "src.mp4".into(),
                hash,
                kind: probed.kind,
                probe: probed.probe,
                proxy: None,
                source_path: None,
                imported: chrono::Utc::now(),
                provenance: None,
            },
        );
        let mut clip = Clip::new(
            Source::Asset { asset: id, stream: None },
            Time::ZERO,
            Time::from_secs(2),
        );
        clip.source_in = Time::from_secs(1);
        clip.fit = dvs_core::project::Fit::Stretch;
        push_track(&mut h.project, vec![clip]);

        let seq = h.project.active_sequence.clone();
        let mut comp =
            Compositor::new(&tool, &h.project, &h.paths, &h.assets, &seq, CompOptions::default())
                .unwrap();
        let first = comp.frame(0).unwrap();
        assert_eq!(first.size(), [160, 120]);
        assert!(first.alpha_coverage() > 0.99, "decoded media should be opaque");
        // testsrc2 animates: timeline frame 0 (source 1s) and frame 30 (source 2s) differ.
        let later = comp.frame(30).unwrap();
        assert_ne!(first.pixels(), later.pixels());
        // And the source offset is honoured: timeline 0 is not source 0.
        let mut direct = VideoDecoder::open(
            &tool,
            &media,
            DecodeSpec {
                size: [160, 120],
                fps: fps(),
                range: Default::default(),
                matrix: Default::default(),
                scaler: "bicubic",
            },
        );
        let source_zero = direct.frame_at(Time::ZERO).unwrap();
        assert_ne!(
            first.pixels(),
            source_zero.pixels(),
            "source_in was ignored: timeline 0 decoded source 0"
        );
    }
}
