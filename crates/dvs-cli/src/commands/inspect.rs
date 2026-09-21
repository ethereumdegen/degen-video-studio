//! `dvs digest`, `dvs lint`, `dvs diff`: the feedback channel.
//!
//! These commands answer "what did that actually do?" for an operator who cannot watch
//! the video. Two of them can fail on purpose: `lint` exits 4 when any finding is an
//! error, and `diff --threshold` exits 4 when a sampled frame falls below it. That shared
//! code is deliberate — both mean "the content is wrong", which is a different branch
//! from "the command was wrong" (2) and "the edit could not be applied" (1).

use crate::cli::{DiffArgs, DigestArgs, LintArgs};
use crate::commands::Ctx;
use dvs_core::error::{Error, Result};
use dvs_core::time::Time;
use dvs_inspect::{DigestOptions, LintOptions, LoudnessProfile};
use dvs_mcp::tools;
use serde_json::json;

pub fn digest(ctx: &Ctx, args: DigestArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let tool = ctx.toolchain()?;
    let sequence = ctx.sequence(&workspace)?;
    let fps = ctx.fps(&workspace, &sequence)?;

    let mut options = DigestOptions {
        with_audio: !args.no_audio,
        ..DigestOptions::default()
    };
    options.sample_every = Time::parse_with_fps(&args.every, fps)?;
    if !options.sample_every.is_positive() {
        return Err(Error::bad_args("--every must be greater than zero"));
    }

    let digest = dvs_inspect::digest(&workspace, tool, &sequence, &options)?;
    let value = serde_json::to_value(&digest)
        .map_err(|e| Error::json(workspace.paths.project_json(), e))?;
    if let Some(path) = &args.out {
        let mut bytes = serde_json::to_vec_pretty(&value).map_err(|e| Error::json(path, e))?;
        bytes.push(b'\n');
        std::fs::write(path, bytes).map_err(|e| Error::io(path, e))?;
    }
    ctx.out.report(&value, || {
        serde_json::to_string_pretty(&value).unwrap_or_default()
    });
    Ok(())
}

pub fn lint(ctx: &Ctx, args: LintArgs) -> Result<()> {
    let workspace = ctx.workspace()?;
    let tool = ctx.toolchain()?;
    let sequence = ctx.sequence(&workspace)?;
    let options = LintOptions {
        profile: LoudnessProfile::parse(args.profile.as_str())?,
        render: !args.no_render,
        ..LintOptions::default()
    };

    let findings = dvs_inspect::lint(&workspace, tool, &sequence, &options)?;
    let value = tools::lint_json(&findings, options.profile.name());
    let errors = value["counts"]["error"].as_u64().unwrap_or_default();
    ctx.out.report(&value, || {
        if findings.is_empty() {
            return format!("no findings ({} profile)", options.profile.name());
        }
        findings
            .iter()
            .map(|finding| {
                format!(
                    "{:<8} {:<22} {}: {}",
                    format!("{:?}", finding.severity).to_lowercase(),
                    finding.rule,
                    finding.target,
                    finding.detail
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    });

    // The report is printed first: a caller that branches on exit code 4 still wants to
    // read what failed, and an error return would send it to stderr as one line.
    if tools::has_errors(&findings) {
        return Err(Error::Lint {
            count: errors as usize,
            summary: findings
                .iter()
                .filter(|finding| finding.severity == dvs_inspect::Severity::Error)
                .map(|finding| finding.rule)
                .collect::<Vec<_>>()
                .join(", "),
        });
    }
    Ok(())
}

pub fn diff(ctx: &Ctx, args: DiffArgs) -> Result<()> {
    let tool = ctx.toolchain()?;
    let every = Time::parse(&args.every)?;
    if !every.is_positive() {
        return Err(Error::bad_args("--every must be greater than zero"));
    }
    for path in [&args.a, &args.b] {
        if !path.is_file() {
            return Err(Error::bad_args(format!("'{}' is not a file", path.display())));
        }
    }

    let report = dvs_inspect::diff(tool, &args.a, &args.b, every)?;
    let mut value = serde_json::to_value(&report).map_err(|e| Error::json(&args.a, e))?;
    if let Some(threshold) = args.threshold {
        value["threshold"] = json!(threshold);
        value["passed"] = json!(report.min_ssim >= threshold);
    }
    ctx.out.report(&value, || {
        format!(
            "{} sample(s) · min SSIM {:.4} · mean {:.4} · {} changed range(s)",
            report.samples.len(),
            report.min_ssim,
            report.mean_ssim,
            report.changed_ranges.len()
        )
    });

    if let Some(threshold) = args.threshold {
        if report.min_ssim < threshold {
            return Err(Error::Lint {
                count: report
                    .samples
                    .iter()
                    .filter(|sample| sample.ssim < threshold)
                    .count(),
                summary: format!(
                    "min SSIM {:.4} is below the required {threshold:.4}",
                    report.min_ssim
                ),
            });
        }
    }
    Ok(())
}
