//! `dvs render`, `dvs frame`, `dvs sheet`: the three ways to turn a document into pixels.
//!
//! All three pair the artifact with a description of it, because the operator cannot look
//! at the result. `render` reports the segments it reused (the incremental cache is only
//! trustworthy if it is observable), `frame` reports the clip boxes and their ids, and
//! `sheet` reports the instants it sampled. A command here that printed only "ok" would
//! be a dead end for the caller.

use crate::cli::{FrameArgs, RenderArgs, SheetArgs};
use crate::commands::Ctx;
use dvs_core::error::{Error, Result};
use dvs_core::time::Time;
use dvs_mcp::tools;
use dvs_render::RenderSpec;
use serde_json::json;

pub fn render(ctx: &Ctx, args: RenderArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let tool = ctx.toolchain()?;
    let sequence = ctx.sequence(&workspace)?;
    let fps = ctx.fps(&workspace, &sequence)?;

    let mut spec = RenderSpec::new(&args.out, sequence.clone());
    spec.scale = args.scale;
    spec.encoder = dvs_media::Encoder::parse(&args.encoder);
    spec.quality = args.quality;
    spec.preset = args.preset.clone();
    spec.use_proxy = args.proxy;
    spec.no_cache = args.no_cache;
    spec.with_digest = args.digest.is_some();
    if let Some(range) = &args.range {
        spec.range = Some(tools::parse_span(range, fps)?);
    }
    if spec.scale <= 0.0 {
        return Err(Error::bad_args("--scale must be greater than zero"));
    }

    if ctx.dry_run {
        let value = json!({
            "dryRun": true,
            "output": args.out.display().to_string(),
            "sequence": sequence,
            "range": spec.range,
            "scale": spec.scale,
            "segments": dvs_render::plan_keys(&workspace, tool, &spec)?,
        });
        ctx.out.report(&value, || {
            format!(
                "would render {} segment(s) to {}",
                value["segments"].as_array().map_or(0, Vec::len),
                args.out.display()
            )
        });
        return Ok(());
    }

    let report = dvs_render::render(&workspace, tool, &spec)?;
    if let (Some(path), Some(digest)) = (&args.digest, &report.digest) {
        let mut bytes = serde_json::to_vec_pretty(digest).map_err(|e| Error::json(path, e))?;
        bytes.push(b'\n');
        std::fs::write(path, bytes).map_err(|e| Error::io(path, e))?;
    }
    // The report carries `tools`, but rendering does not write it into the document:
    // `doctor` is the one writer of that provenance. Two writers would fight — doctor
    // records what this ffmpeg *can* do, a render records what it *used* — and a project
    // that changed on disk every time someone rendered would be a surprise.
    let value = serde_json::to_value(&report).map_err(|e| Error::json(&report.output, e))?;
    ctx.out.report(&value, || {
        format!(
            "{} · {} frames · {} · {}/{} segments reused · {} ms",
            report.output.display(),
            report.frames,
            report.duration,
            report.segments_reused,
            report.segments_total,
            report.render_ms
        )
    });
    Ok(())
}

pub fn frame(ctx: &Ctx, args: FrameArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let tool = ctx.toolchain()?;
    let sequence = ctx.sequence(&workspace)?;
    let fps = ctx.fps(&workspace, &sequence)?;
    let at = Time::parse_with_fps(&args.at, fps)?;

    if ctx.dry_run {
        let value = json!({
            "dryRun": true,
            "at": at,
            "frame": at.frame_floor(fps),
            "output": args.out.display().to_string(),
        });
        ctx.out.report(&value, || {
            format!("would render frame {} to {}", at.frame_floor(fps), args.out.display())
        });
        return Ok(());
    }

    let shot = tools::frame(
        &workspace,
        tool,
        &sequence,
        at,
        args.scale,
        args.annotate,
        &args.out,
    )?;
    let value = json!({
        "output": shot.output.display().to_string(),
        "frame": shot.frame,
        "at": shot.at,
        "timecode": shot.at.timecode(fps),
        "annotated": shot.annotated,
        "annotations": shot.annotations,
    });
    ctx.out.report(&value, || {
        format!(
            "{} · frame {} · {} · {} layer(s)",
            shot.output.display(),
            shot.frame,
            shot.at.timecode(fps),
            shot.annotations.as_array().map_or(0, Vec::len)
        )
    });
    Ok(())
}

pub fn sheet(ctx: &Ctx, args: SheetArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let tool = ctx.toolchain()?;
    let sequence = ctx.sequence(&workspace)?;
    let fps = ctx.fps(&workspace, &sequence)?;
    let every = Time::parse_with_fps(&args.every, fps)?;
    if !every.is_positive() {
        return Err(Error::bad_args("--every must be greater than zero"));
    }
    if args.cols == 0 || args.width == 0 {
        return Err(Error::bad_args("--cols and --width must be greater than zero"));
    }

    if ctx.dry_run {
        let duration = workspace.project.sequence(&sequence)?.duration();
        let cells = (duration.as_secs_f64() / every.as_secs_f64()).ceil().max(0.0) as u64;
        let value = json!({
            "dryRun": true,
            "output": args.out.display().to_string(),
            "cells": cells,
            "every": every,
        });
        ctx.out.report(&value, || {
            format!("would sample {cells} frame(s) into {}", args.out.display())
        });
        return Ok(());
    }

    let report = dvs_inspect::contact_sheet(
        &workspace,
        tool,
        &sequence,
        every,
        args.cols,
        args.width,
        &args.out,
    )?;
    let value = serde_json::to_value(&report).map_err(|e| Error::json(&args.out, e))?;
    ctx.out.report(&value, || {
        format!("wrote {}", args.out.display())
    });
    Ok(())
}
