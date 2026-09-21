//! What a media file actually is, according to ffprobe.
//!
//! Everything downstream depends on this being right, and two fields in particular are the
//! usual suspects behind "the colors are wrong" and "the cut is one frame off":
//!
//! - `color_range` / `color_matrix`: a limited-range BT.709 file decoded as full-range
//!   BT.601 looks washed out and slightly hue-shifted. We record what the file claims and
//!   pass it to ffmpeg explicitly on every decode instead of letting defaults decide.
//! - `vfr`: phone and screen recordings routinely have variable frame timing. Frame-exact
//!   editing against a VFR source is meaningless, so import detects it and builds a CFR
//!   proxy rather than pretending.

use crate::toolchain::Toolchain;
use dvs_core::error::{Error, Result};
use dvs_core::project::{
    AssetKind, AudioStream, ColorMatrix, ColorRange, Probe, VideoStream,
};
use dvs_core::time::{Fps, Rat, Time};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Probed {
    pub probe: Probe,
    pub kind: AssetKind,
}

/// Run ffprobe and translate its JSON into the document's typed form.
pub fn probe(tool: &Toolchain, path: &Path) -> Result<Probed> {
    let output = tool
        .ffprobe_command()
        .args([
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_entries",
            "stream_side_data=rotation",
        ])
        .arg(path)
        .output()
        .map_err(|e| Error::tool("ffprobe", e.to_string()))?;
    if !output.status.success() {
        return Err(Error::tool(
            "ffprobe",
            format!(
                "cannot probe '{}': {}",
                path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::tool("ffprobe", format!("unparseable output: {e}")))?;
    from_ffprobe_json(&json, path)
}

/// Split out from [`probe`] so the translation is testable without spawning anything: the
/// interesting failure modes here are all shapes of ffprobe JSON, not process handling.
pub fn from_ffprobe_json(json: &serde_json::Value, path: &Path) -> Result<Probed> {
    let streams = json
        .get("streams")
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    let format = json.get("format");

    let video = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("video"))
        .filter(|s| {
            // Cover art in an mp3 is a video stream with one frame; treating it as footage
            // would make an audio file look like a still.
            s.get("disposition")
                .and_then(|d| d.get("attached_pic"))
                .and_then(|v| v.as_i64())
                != Some(1)
        })
        .map(video_stream)
        .transpose()?;
    let audio = streams
        .iter()
        .find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("audio"))
        .map(audio_stream)
        .transpose()?;

    let duration = format
        .and_then(|f| f.get("duration"))
        .and_then(|d| d.as_str())
        .and_then(|d| Time::parse(d).ok())
        .or_else(|| {
            streams
                .iter()
                .filter_map(|s| s.get("duration").and_then(|d| d.as_str()))
                .filter_map(|d| Time::parse(d).ok())
                .max()
        })
        .unwrap_or(Time::ZERO);

    let container = format
        .and_then(|f| f.get("format_name"))
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string();

    let vfr = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("video"))
        .any(is_variable_rate);

    let kind = match (&video, &audio) {
        (Some(stream), _) if stream.frames == Some(1) => AssetKind::Image,
        (Some(_), _) => AssetKind::Video,
        (None, Some(_)) => AssetKind::Audio,
        (None, None) => AssetKind::Other,
    };
    let kind = match (kind, path.extension().and_then(|e| e.to_str())) {
        (AssetKind::Other, Some(ext)) => match ext.to_ascii_lowercase().as_str() {
            "png" | "jpg" | "jpeg" | "webp" | "tif" | "tiff" | "bmp" | "svg" => AssetKind::Image,
            "ttf" | "otf" | "woff" | "woff2" => AssetKind::Font,
            "cube" | "3dl" => AssetKind::Lut,
            "srt" | "vtt" | "ass" => AssetKind::Subtitle,
            _ => AssetKind::Other,
        },
        (kind, _) => kind,
    };

    Ok(Probed {
        probe: Probe {
            duration,
            video,
            audio,
            vfr,
            container,
        },
        kind,
    })
}

fn video_stream(stream: &serde_json::Value) -> Result<VideoStream> {
    let width = int(stream, "width").unwrap_or(0) as u32;
    let height = int(stream, "height").unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        return Err(Error::tool(
            "ffprobe",
            "video stream reports a zero dimension",
        ));
    }
    // `avg_frame_rate` is the honest average over the file; `r_frame_rate` is the base
    // timebase rate, which for VFR content is a lie (often 1000/1 or 30000/1).
    let fps = parse_rate(stream.get("avg_frame_rate").and_then(|v| v.as_str()))
        .or_else(|| parse_rate(stream.get("r_frame_rate").and_then(|v| v.as_str())))
        .unwrap_or_default();
    Ok(VideoStream {
        stream_index: int(stream, "index").unwrap_or(0) as u32,
        size: [width, height],
        fps,
        codec: text(stream, "codec_name"),
        pix_fmt: text(stream, "pix_fmt"),
        color_range: match stream.get("color_range").and_then(|v| v.as_str()) {
            Some("tv") | Some("limited") => ColorRange::Tv,
            Some("pc") | Some("full") => ColorRange::Pc,
            _ => ColorRange::Unknown,
        },
        color_matrix: match stream.get("color_space").and_then(|v| v.as_str()) {
            Some("bt709") => ColorMatrix::Bt709,
            Some("bt470bg") | Some("smpte170m") | Some("bt601") => ColorMatrix::Bt601,
            Some("bt2020nc") | Some("bt2020_ncl") => ColorMatrix::Bt2020Ncl,
            Some("smpte240m") => ColorMatrix::Smpte240m,
            _ => ColorMatrix::Unknown,
        },
        color_primaries: stream
            .get("color_primaries")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        transfer: stream
            .get("color_transfer")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        rotation: rotation(stream),
        sar: parse_aspect(stream.get("sample_aspect_ratio").and_then(|v| v.as_str())),
        frames: stream
            .get("nb_frames")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok())
            .or_else(|| int(stream, "nb_frames")),
        bit_rate: stream
            .get("bit_rate")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok()),
    })
}

fn audio_stream(stream: &serde_json::Value) -> Result<AudioStream> {
    Ok(AudioStream {
        stream_index: int(stream, "index").unwrap_or(0) as u32,
        rate: stream
            .get("sample_rate")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok())
            .unwrap_or(48_000),
        channels: int(stream, "channels").unwrap_or(2) as u16,
        codec: text(stream, "codec_name"),
        bit_rate: stream
            .get("bit_rate")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok()),
    })
}

/// A stream is treated as variable-rate when its nominal and average rates disagree by
/// more than a rounding artifact. `r_frame_rate` of 1000/1 against an average of 29.97 is
/// the signature of a screen recording.
fn is_variable_rate(stream: &serde_json::Value) -> bool {
    let nominal = parse_rate(stream.get("r_frame_rate").and_then(|v| v.as_str()));
    let average = parse_rate(stream.get("avg_frame_rate").and_then(|v| v.as_str()));
    match (nominal, average) {
        (Some(nominal), Some(average)) => {
            let (a, b) = (nominal.as_f64(), average.as_f64());
            if a <= 0.0 || b <= 0.0 {
                return false;
            }
            (a - b).abs() / b > 0.02
        }
        _ => false,
    }
}

fn rotation(stream: &serde_json::Value) -> i32 {
    if let Some(list) = stream.get("side_data_list").and_then(|v| v.as_array()) {
        for entry in list {
            if let Some(value) = entry.get("rotation").and_then(|v| v.as_f64()) {
                return value.round() as i32;
            }
        }
    }
    stream
        .get("tags")
        .and_then(|t| t.get("rotate"))
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<f64>().ok())
        .map(|v| v.round() as i32)
        .unwrap_or(0)
}

fn parse_rate(text: Option<&str>) -> Option<Fps> {
    let text = text?;
    let (num, den) = text.split_once('/')?;
    let num: i64 = num.parse().ok()?;
    let den: i64 = den.parse().ok()?;
    if num <= 0 || den <= 0 {
        return None;
    }
    Fps::new(num, den).ok()
}

/// ffprobe writes aspect ratios as `4:3`, and `0:1` when it has no idea.
fn parse_aspect(text: Option<&str>) -> Rat {
    let fallback = Rat::ONE;
    let Some(text) = text else { return fallback };
    let Some((num, den)) = text.split_once(':') else {
        return fallback;
    };
    match (num.parse::<i64>(), den.parse::<i64>()) {
        (Ok(num), Ok(den)) if num > 0 && den > 0 => Rat::new(num, den).unwrap_or(fallback),
        _ => fallback,
    }
}

fn int(value: &serde_json::Value, key: &str) -> Option<i64> {
    value.get(key).and_then(|v| v.as_i64())
}

fn text(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn json(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn ndf_video_is_read_as_an_exact_rational() {
        let probed = from_ffprobe_json(
            &json(
                r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,
                    "height":1080,"pix_fmt":"yuv420p","r_frame_rate":"30000/1001",
                    "avg_frame_rate":"30000/1001","color_range":"tv","color_space":"bt709",
                    "sample_aspect_ratio":"1:1","nb_frames":"1798"}],
                    "format":{"duration":"60.000000","format_name":"mov,mp4"}}"#,
            ),
            Path::new("talk.mp4"),
        )
        .unwrap();
        let video = probed.probe.video.unwrap();
        assert_eq!(video.fps, Fps::new(30000, 1001).unwrap());
        assert_eq!(video.color_range, ColorRange::Tv);
        assert_eq!(video.color_matrix, ColorMatrix::Bt709);
        assert_eq!(probed.probe.duration, Time::from_secs(60));
        assert!(!probed.probe.vfr);
        assert!(matches!(probed.kind, AssetKind::Video));
    }

    #[test]
    fn screen_recordings_are_flagged_variable_rate() {
        // 1000/1 nominal against a 29.97 average is exactly what a screen capture looks
        // like; treating it as CFR silently misplaces every cut.
        let probed = from_ffprobe_json(
            &json(
                r#"{"streams":[{"index":0,"codec_type":"video","width":1280,"height":720,
                    "r_frame_rate":"1000/1","avg_frame_rate":"30000/1001"}],
                    "format":{"duration":"12.5"}}"#,
            ),
            Path::new("screen.mkv"),
        )
        .unwrap();
        assert!(probed.probe.vfr);
        assert_eq!(
            probed.probe.video.unwrap().fps,
            Fps::new(30000, 1001).unwrap(),
            "the average rate is the honest one"
        );
    }

    #[test]
    fn unflagged_color_is_reported_as_unknown_not_guessed() {
        let probed = from_ffprobe_json(
            &json(
                r#"{"streams":[{"index":0,"codec_type":"video","width":640,"height":480,
                    "avg_frame_rate":"25/1"}],"format":{"duration":"1.0"}}"#,
            ),
            Path::new("x.avi"),
        )
        .unwrap();
        let video = probed.probe.video.unwrap();
        assert_eq!(video.color_range, ColorRange::Unknown);
        assert_eq!(video.color_matrix, ColorMatrix::Unknown);
    }

    #[test]
    fn cover_art_does_not_turn_an_mp3_into_footage() {
        let probed = from_ffprobe_json(
            &json(
                r#"{"streams":[
                    {"index":0,"codec_type":"audio","codec_name":"mp3","sample_rate":"44100","channels":2},
                    {"index":1,"codec_type":"video","codec_name":"mjpeg","width":600,"height":600,
                     "avg_frame_rate":"90000/3753","disposition":{"attached_pic":1}}],
                    "format":{"duration":"183.2","format_name":"mp3"}}"#,
            ),
            Path::new("music.mp3"),
        )
        .unwrap();
        assert!(matches!(probed.kind, AssetKind::Audio));
        assert!(probed.probe.video.is_none());
        assert_eq!(probed.probe.audio.unwrap().rate, 44_100);
    }

    #[test]
    fn rotation_and_anamorphic_aspect_reach_the_document() {
        let probed = from_ffprobe_json(
            &json(
                r#"{"streams":[{"index":0,"codec_type":"video","width":1920,"height":1080,
                    "avg_frame_rate":"30/1","sample_aspect_ratio":"4:3",
                    "side_data_list":[{"rotation":-90}]}],"format":{"duration":"2"}}"#,
            ),
            Path::new("phone.mov"),
        )
        .unwrap();
        let video = probed.probe.video.unwrap();
        assert_eq!(video.rotation, -90);
        assert_eq!(video.display_size(), [1080, 2560]);
    }

    #[test]
    fn extension_classifies_what_ffprobe_has_no_streams_for() {
        let probed = from_ffprobe_json(&json(r#"{"streams":[],"format":{}}"#), Path::new("a.srt"))
            .unwrap();
        assert!(matches!(probed.kind, AssetKind::Subtitle));
        let font = from_ffprobe_json(
            &json(r#"{"streams":[],"format":{}}"#),
            &PathBuf::from("Inter.ttf"),
        )
        .unwrap();
        assert!(matches!(font.kind, AssetKind::Font));
    }

    #[test]
    fn a_still_image_is_classified_by_its_single_frame() {
        let probed = from_ffprobe_json(
            &json(
                r#"{"streams":[{"index":0,"codec_type":"video","codec_name":"png","width":512,
                    "height":512,"avg_frame_rate":"25/1","nb_frames":"1"}],
                    "format":{"duration":"0.04"}}"#,
            ),
            Path::new("logo.png"),
        )
        .unwrap();
        assert!(matches!(probed.kind, AssetKind::Image));
    }
}
