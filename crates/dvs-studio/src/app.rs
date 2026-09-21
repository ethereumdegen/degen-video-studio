//! The Tauri shell: commands, the frame protocol, and the bridge from the file watcher to
//! the window.
//!
//! Three boundaries live here and nowhere else.
//!
//! **Commands** are the only way the view mutates anything, and each one is a thin wrapper
//! over [`crate::engine::Handle`] so the webview can never touch a `Workspace` directly.
//! Errors cross as the same structured report the CLI prints — `{kind, code, message,
//! candidates}` — because "selector matched nothing; did you mean #talk" is as useful in a
//! status line as it is in a terminal.
//!
//! **Frames** are served over a custom URI scheme rather than returned from a command.
//! A command reply is JSON, so a 1280×720 frame would cross the boundary as ~3.5 MB of
//! base64 on every scrub tick; as a URL the webview fetches it as an ordinary image,
//! caches it, and decodes it off the main thread. The scheme carries the revision so a
//! document edit busts that cache instead of showing a stale picture.
//!
//! **The watcher** turns an agent's edit into a `document-changed` event. The window then
//! re-reads one snapshot; it never diffs the document itself.

use crate::engine::Handle;
use crate::monitor::Monitor;
use crate::state::{self, Applied, Finding, Snapshot, StudioOptions};
use dvs_core::error::{Error, Result};
use dvs_core::time::{Span, Time};
use serde::Serialize;
use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use tauri::{Emitter, Manager, State};
use tokio_stream::StreamExt;

/// What the frontend receives when a command fails.
///
/// Identical in shape to the CLI's `--json` error, deliberately: one error contract for
/// every surface means an agent, a script and this window all read failures the same way.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub kind: &'static str,
    pub code: i32,
    pub message: String,
    pub candidates: Vec<String>,
}

impl From<Error> for CommandError {
    fn from(error: Error) -> Self {
        let report = error.to_report();
        CommandError {
            kind: report.kind,
            code: report.code,
            message: report.message,
            candidates: report.candidates,
        }
    }
}

type CommandResult<T> = std::result::Result<T, CommandError>;

/// Shared application state: the worker handle, the audio device, and which playback
/// stream is current.
struct App {
    handle: Handle,
    monitor: Monitor,
    playing: Arc<Mutex<Playing>>,
}

#[tauri::command]
async fn snapshot(app: State<'_, App>) -> CommandResult<Snapshot> {
    Ok(app.handle.reload().await?)
}

#[tauri::command]
async fn run_command(line: String, app: State<'_, App>) -> CommandResult<Applied> {
    let (op, args) = app.handle.parse_command(&line)?;
    Ok(app.handle.apply(&op, args).await?)
}

#[tauri::command]
async fn apply_op(
    op: String,
    args: serde_json::Value,
    app: State<'_, App>,
) -> CommandResult<Applied> {
    Ok(app.handle.apply(&op, args).await?)
}

#[tauri::command]
async fn undo(app: State<'_, App>) -> CommandResult<Option<String>> {
    Ok(app.handle.undo().await?)
}

#[tauri::command]
async fn redo(app: State<'_, App>) -> CommandResult<Option<String>> {
    Ok(app.handle.redo().await?)
}

#[tauri::command]
async fn lint(app: State<'_, App>) -> CommandResult<Vec<Finding>> {
    Ok(app.handle.lint().await?)
}

#[tauri::command]
async fn describe_text(app: State<'_, App>) -> CommandResult<String> {
    let snapshot = app.handle.reload().await?;
    let findings = app.handle.lint().await.unwrap_or_default();
    Ok(state::describe(&snapshot, &findings))
}

/// How much audio is mixed per chunk. Long enough that a refill is rare, short enough that
/// pressing play does not wait on a minute of mixing before the first sample.
const CHUNK: i64 = 8;
/// Refill when the device has less than this much buffered.
const REFILL_AT: f64 = 3.0;
/// Frames rendered ahead of the playhead when playback starts.
const PREFETCH: i64 = 48;

/// What the current playback stream is playing, so a later `monitor_play` or a document
/// edit can retire it. Every stream gets a generation; a feeder whose generation is stale
/// exits instead of pushing samples into somebody else's playback.
#[derive(Debug, Default)]
struct Playing {
    generation: u64,
    from_frame: i64,
}

#[tauri::command]
async fn monitor_play(
    from_frame: i64,
    app: State<'_, App>,
    window: tauri::Window,
) -> CommandResult<serde_json::Value> {
    let fps = app.handle.fps();
    let duration = app.handle.duration();
    let start = Time::from_frames(from_frame.max(0), fps);
    if start >= duration {
        return Err(Error::op("the playhead is at the end of the sequence").into());
    }
    // Warm the picture before the sound starts: the whole point of prefetching is that the
    // first second of playback is not a slideshow.
    app.handle.prefetch(from_frame, PREFETCH);

    let end = (start + Time::from_secs(CHUNK)).min(duration);
    let (samples, rate, channels) = app.handle.mix(Span::new(start, end)).await?;
    let generation = {
        let mut playing = app.playing.lock().expect("playback state");
        playing.generation += 1;
        playing.from_frame = from_frame;
        playing.generation
    };
    app.monitor
        .play(Arc::new(samples), rate, channels, 0)
        .map_err(CommandError::from)?;

    let handle = app.handle.clone();
    let monitor = app.monitor.clone();
    let playing = app.playing.clone();
    let emitter = window.clone();
    tauri::async_runtime::spawn(async move {
        feed_and_report(handle, monitor, playing, emitter, generation, from_frame, end).await;
    });

    Ok(serde_json::json!({
        "playing": true,
        "rate": rate,
        "channels": channels,
        "device": app.monitor.device(),
        "fromFrame": from_frame,
    }))
}

/// Keep the device fed and tell the window where the audio clock is.
///
/// The window follows this, not `performance.now()`: a picture driven by wall time drifts
/// audibly from sound driven by a device clock within a few seconds.
async fn feed_and_report(
    handle: Handle,
    monitor: Monitor,
    playing: Arc<Mutex<Playing>>,
    window: tauri::Window,
    generation: u64,
    from_frame: i64,
    mut mixed_to: Time,
) {
    let fps = handle.fps();
    let duration = handle.duration();
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(50));
    loop {
        ticker.tick().await;
        let current = playing.lock().expect("playback state").generation;
        if current != generation || !monitor.is_playing() {
            break;
        }
        let (rate, channels) = monitor.format();
        let played = Time::from_samples(monitor.position() as i64, rate.max(1));
        let at = Time::from_frames(from_frame, fps) + played;
        let _ = window.emit(
            "monitor-position",
            serde_json::json!({
                "frame": at.frame_floor(fps),
                "underruns": monitor.underruns(),
            }),
        );
        if at >= duration {
            monitor.stop();
            let _ = window.emit("monitor-ended", ());
            break;
        }
        // Refill while the device still has a cushion, and keep the picture ahead too.
        let buffered = mixed_to - at;
        if buffered.as_secs_f64() < REFILL_AT && mixed_to < duration {
            let next_end = (mixed_to + Time::from_secs(CHUNK)).min(duration);
            match handle.mix(Span::new(mixed_to, next_end)).await {
                Ok((samples, _, _)) => {
                    if !monitor.push(&samples) {
                        break;
                    }
                    mixed_to = next_end;
                    handle.prefetch(at.frame_floor(fps), PREFETCH);
                }
                Err(error) => {
                    let _ = window.emit("monitor-failed", error.to_string());
                    monitor.stop();
                    break;
                }
            }
        }
        let _ = channels;
    }
}

#[tauri::command]
fn monitor_stop(app: State<'_, App>) -> CommandResult<()> {
    app.playing.lock().expect("playback state").generation += 1;
    app.monitor.stop();
    Ok(())
}

#[tauri::command]
fn monitor_state(app: State<'_, App>) -> CommandResult<serde_json::Value> {
    let fps = app.handle.fps();
    let (rate, _) = app.monitor.format();
    let from = app.playing.lock().expect("playback state").from_frame;
    let at = Time::from_frames(from, fps) + Time::from_samples(app.monitor.position() as i64, rate.max(1));
    Ok(serde_json::json!({
        "playing": app.monitor.is_playing(),
        "frame": at.frame_floor(fps),
        "device": app.monitor.device(),
        "underruns": app.monitor.underruns(),
    }))
}

#[tauri::command]
async fn peaks(buckets: u32, app: State<'_, App>) -> CommandResult<Vec<crate::engine::TrackPeaks>> {
    Ok(app.handle.peaks(buckets.clamp(16, 4096)).await?)
}

#[tauri::command]
fn prefetch(from_frame: i64, count: i64, app: State<'_, App>) -> CommandResult<()> {
    app.handle.prefetch(from_frame, count.clamp(1, 240));
    Ok(())
}

/// Open the window. Returns when it closes.
pub fn run(options: StudioOptions) -> Result<()> {
    let (handle, first) = Handle::open(&options)?;
    let title = format!(
        "dvs studio · {} · {}",
        first.project_name, first.sequence_name
    );
    let watcher = handle.clone();

    tauri::Builder::default()
        .manage(App {
            handle: handle.clone(),
            monitor: Monitor::new(),
            playing: Arc::new(Mutex::new(Playing::default())),
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            run_command,
            apply_op,
            undo,
            redo,
            lint,
            describe_text,
            monitor_play,
            monitor_stop,
            monitor_state,
            peaks,
            prefetch
        ])
        .register_asynchronous_uri_scheme_protocol("dvsframe", move |ctx, request, responder| {
            let handle = ctx.app_handle().state::<App>().handle.clone();
            let index = frame_index(request.uri().path());
            tauri::async_runtime::spawn(async move {
                responder.respond(match render_png(&handle, index).await {
                    Ok(bytes) => http_response(200, "image/png", bytes),
                    // A failed frame must not leave the viewport on a stale picture with no
                    // explanation, so the reason travels back as a plain-text body the
                    // window puts in its status line.
                    Err(error) => http_response(
                        500,
                        "text/plain; charset=utf-8",
                        error.to_string().into_bytes(),
                    ),
                });
            });
        })
        .setup(move |app| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_title(&title);
            }
            let emitter = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut changes = Box::pin(watcher.watch());
                while changes.next().await.is_some() {
                    // An edit retires the current playback: the mix it was streaming
                    // belongs to a document that no longer exists, and the frames queued
                    // behind it are wrong. The window restarts playback if it wants to.
                    let state = emitter.state::<App>();
                    if state.monitor.is_playing() {
                        state.playing.lock().expect("playback state").generation += 1;
                        state.monitor.stop();
                        let _ = emitter.emit("monitor-ended", ());
                    }
                    // The payload is deliberately empty of document data: the window
                    // re-reads one snapshot, so there is exactly one code path that turns
                    // a document into a view, whether the edit came from here or from an
                    // agent in a terminal.
                    let _ = emitter.emit("document-changed", ());
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .map_err(|error| Error::op(format!("the studio window failed to start: {error}")))
}

/// The `--describe` path: the same information as the window, as text, with no display.
pub fn describe(options: &StudioOptions) -> Result<String> {
    let (handle, _) = Handle::open(options)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| Error::op(format!("cannot start a runtime: {error}")))?;
    runtime.block_on(async {
        let snapshot = handle.reload().await?;
        let findings = handle.lint().await.unwrap_or_default();
        Ok(state::describe(&snapshot, &findings))
    })
}

/// `dvsframe://localhost/1274?rev=7&scale=0.5` → frame 1274. The query is cache-busting
/// only; the scale and revision the worker uses are its own.
fn frame_index(path: &str) -> i64 {
    path.trim_matches('/')
        .split('/')
        .next_back()
        .and_then(|segment| segment.parse::<i64>().ok())
        .unwrap_or(0)
}

async fn render_png(handle: &Handle, index: i64) -> Result<Vec<u8>> {
    let (width, height, rgba) = handle.frame(index).await?;
    encode_png(width, height, &rgba)
}

/// RGBA8 → PNG, in process.
///
/// Not through ffmpeg: the viewport re-encodes on every seek, and a process spawn per
/// frame turns a scrub into a fork bomb.
fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    // `image` panics on a short buffer rather than returning an error, and a panic inside
    // a protocol handler takes the window's IPC thread with it. Check first.
    let expected = (width as usize) * (height as usize) * 4;
    if rgba.len() != expected {
        return Err(Error::op(format!(
            "cannot encode a {width}×{height} frame: got {} bytes, expected {expected}",
            rgba.len()
        )));
    }
    let mut out = Vec::with_capacity(rgba.len() / 4);
    let encoder = image::codecs::png::PngEncoder::new_with_quality(
        &mut out,
        image::codecs::png::CompressionType::Fast,
        image::codecs::png::FilterType::NoFilter,
    );
    image::ImageEncoder::write_image(
        encoder,
        rgba,
        width,
        height,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|error| Error::op(format!("cannot encode a {width}×{height} frame: {error}")))?;
    Ok(out)
}

fn http_response(status: u16, mime: &str, body: Vec<u8>) -> http::Response<Cow<'static, [u8]>> {
    http::Response::builder()
        .status(status)
        .header(http::header::CONTENT_TYPE, mime)
        // The frame for a given (index, revision) never changes, and the revision is in
        // the query string, so the webview may keep it as long as it likes.
        .header(http::header::CACHE_CONTROL, "max-age=31536000, immutable")
        .body(Cow::Owned(body))
        .expect("a response with a valid status and headers")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_index_comes_from_the_last_path_segment() {
        assert_eq!(frame_index("/1274"), 1274);
        assert_eq!(frame_index("1274"), 1274);
        assert_eq!(frame_index("/frames/42"), 42);
        // A malformed request shows frame zero rather than failing the whole protocol:
        // the window has somewhere to recover from, and the status line carries the error
        // that actually matters.
        assert_eq!(frame_index("/"), 0);
        assert_eq!(frame_index("/nope"), 0);
    }

    #[test]
    fn a_frame_encodes_to_a_real_png() {
        let pixels: Vec<u8> = (0..16 * 9 * 4).map(|i| (i % 251) as u8).collect();
        let png = encode_png(16, 9, &pixels).expect("encode");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "not a PNG header");
        let decoded = image::load_from_memory(&png).expect("decode").to_rgba8();
        assert_eq!(decoded.dimensions(), (16, 9));
        assert_eq!(decoded.as_raw().as_slice(), pixels.as_slice(), "lossless round trip");
    }

    #[test]
    fn a_wrong_sized_buffer_is_an_error_not_a_panic() {
        let error = encode_png(64, 64, &[0u8; 16]).expect_err("too few pixels");
        assert!(error.to_string().contains("64×64"), "{error}");
    }

    #[test]
    fn errors_cross_the_boundary_with_their_candidates() {
        let error = Error::no_match("clip", "#tlk", vec!["#talk".into(), "#title".into()]);
        let command: CommandError = error.into();
        assert_eq!(command.kind, "no-match");
        assert_eq!(command.code, dvs_core::error::exit::NO_MATCH);
        assert_eq!(command.candidates, vec!["#talk".to_string(), "#title".to_string()]);
        let json = serde_json::to_value(&command).expect("serialize");
        assert!(json.get("candidates").is_some(), "the view needs the candidates");
    }

    #[test]
    fn a_frame_response_is_cacheable_and_typed() {
        let response = http_response(200, "image/png", vec![1, 2, 3]);
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()[http::header::CONTENT_TYPE], "image/png");
        assert!(response.headers()[http::header::CACHE_CONTROL]
            .to_str()
            .expect("ascii")
            .contains("immutable"));
    }
}
