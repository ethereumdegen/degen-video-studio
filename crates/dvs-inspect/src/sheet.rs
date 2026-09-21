//! Contact sheets and annotated frames: handles for a vision model.
//!
//! A vision model can describe a picture, but it cannot address one. "The text near the top
//! is too close to the edge" is not actionable — there is no way back from that sentence to
//! a clip id. Both functions here exist to close that gap by burning identity into the
//! image and returning the same identity as data.
//!
//! [`contact_sheet`] lays sampled frames out in a grid with the timecode and the clip id of
//! whatever is on top written into each cell, so a model can say "cell 7, `clp_01J8…`, the
//! lower third is cut off" and an agent can act on it directly.
//!
//! [`annotate_frame`] draws a numbered box around every layer the compositor placed and
//! returns the legend as [`Annotation`] values. The numbers are what make it work: the model
//! answers with "box 2", and box 2 is a clip id, a track, and a rectangle in pixels.
//!
//! Both render through the same compositor and the same title rasterizer as the export, so
//! the frames on the sheet are the frames that will be encoded — a preview path that could
//! disagree with the render would make a review round worthless.

use crate::analyze;
use dvs_comp::geometry::{self, Placement, Rect};
use dvs_comp::title::Rasterizer;
use dvs_core::color::Rgba;
use crate::Subject;
use dvs_core::error::{Error, Result};
use dvs_core::ids::{ClipId, SequenceId, TrackId};
use dvs_core::project::{escape_xml, Blend};
use dvs_core::time::{Span, Time};
use dvs_media::{write_png, Frame, Toolchain};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Label type size as a fraction of cell height, and the floor it cannot go below. Small
/// enough not to cover the picture, large enough that a vision model reads it reliably —
/// which is the only reason the label exists.
const LABEL_FRACTION: f32 = 1.0 / 11.0;
const MIN_LABEL_PX: f32 = 9.0;

/// Box colours for annotations, cycled. Chosen to stay distinguishable over arbitrary
/// footage and from each other when described in words.
const ANNOTATION_COLORS: &[&str] = &[
    "#ff2d55", "#00e5ff", "#ffd60a", "#34c759", "#bf5af2", "#ff9f0a",
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetCell {
    /// Cell number, reading order, starting at 1 — the handle a reviewer quotes.
    pub index: u32,
    pub row: u32,
    pub column: u32,
    pub at: Time,
    pub timecode: String,
    /// Clips visible in this frame, topmost last.
    pub clips: Vec<ClipId>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetReport {
    pub output: PathBuf,
    pub columns: u32,
    pub rows: u32,
    /// Pixel size of the written image.
    pub size: [u32; 2],
    pub cell_size: [u32; 2],
    pub every: Time,
    pub cells: Vec<SheetCell>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotation {
    /// Box number as drawn, starting at 1.
    pub index: u32,
    pub clip: ClipId,
    pub track: TrackId,
    pub name: String,
    /// What the clip shows, as the selector grammar names it.
    pub source: String,
    /// `[x, y, width, height]` in frame pixels.
    pub bbox: [f32; 4],
    pub opacity: f32,
    /// Colour the box was drawn in, so a description can be matched back without counting.
    pub color: String,
}

/// Sample a sequence into a labelled grid and write it as a PNG.
///
/// Frames are composited at cell size rather than full size and then downscaled: the decode
/// happens once, in ffmpeg's scaler, at the size the cell needs. A 3-minute timeline on a
/// 6-wide sheet is 36 small composites, which is why this is fast enough to run after every
/// edit.
///
/// `width` is the width of the whole sheet, not of a cell: a caller picks the size of the
/// image it is going to look at, and the cell size falls out of `width / columns`.
pub fn contact_sheet<'a>(
    subject: impl Into<Subject<'a>>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    every: Time,
    columns: u32,
    width: u32,
    output: &Path,
) -> Result<SheetReport> {
    analyze::require_interval(every, "every")?;
    if columns == 0 {
        return Err(Error::bad_args("a contact sheet needs at least one column"));
    }
    let subject = subject.into();
    let seq = subject.project.sequence(sequence)?;
    let fps = seq.fps;
    let duration = seq.duration();
    if duration.is_zero() {
        return Err(Error::op(format!(
            "sequence '{}' is empty, so there is nothing to sheet",
            seq.name
        )));
    }
    // Cells are a whole number of pixels wide so the grid has no seams, which means the
    // sheet is at most `columns - 1` pixels narrower than requested.
    let cell_w = (width / columns).max(16);
    let cell_h = ((cell_w as f64 * seq.size[1] as f64 / seq.size[0].max(1) as f64).round() as u32)
        .max(16);

    let instants = analyze::instants(Span::new(Time::ZERO, duration), every, fps);
    let count = instants.len() as u32;
    let rows = count.div_ceil(columns).max(1);
    let mut sheet = Frame::filled(cell_w * columns, cell_h * rows, Rgba::BLACK);

    let scale = f64::from(cell_w) / f64::from(seq.size[0].max(1));
    let mut comp = analyze::compositor(subject, tool, sequence, scale, true)?;
    let rasterizer = Rasterizer::new();
    let mut cells = Vec::with_capacity(instants.len());

    for (index, at) in instants.into_iter().enumerate() {
        let (frame, report) = comp.frame_with_report(at.frame_floor(fps))?;
        let frame = if frame.size() == [cell_w, cell_h] {
            frame
        } else {
            // The compositor rounds each axis independently, so it can land a pixel off the
            // cell; resampling here keeps the grid exact.
            geometry::resample(&frame, [cell_w, cell_h])
        };
        let column = index as u32 % columns;
        let row = index as u32 / columns;
        let origin = Rect {
            x: (column * cell_w) as f32,
            y: (row * cell_h) as f32,
            w: cell_w as f32,
            h: cell_h as f32,
        };
        blit_cell(&mut sheet, &frame, origin);

        let clips: Vec<ClipId> = report.layers.iter().map(|layer| layer.clip.clone()).collect();
        let timecode = at.timecode(fps);
        let top = report.layers.last().map(|layer| layer.clip.to_string());
        let label = label_svg(cell_w, cell_h, &timecode, top.as_deref());
        let overlay = rasterizer.rasterize(&label, [cell_w, cell_h])?;
        blit_cell(&mut sheet, &overlay, origin);

        cells.push(SheetCell {
            index: index as u32 + 1,
            row,
            column,
            at,
            timecode,
            clips,
        });
    }

    write_png(tool, &sheet, output)?;
    Ok(SheetReport {
        output: output.to_path_buf(),
        columns,
        rows,
        size: sheet.size(),
        cell_size: [cell_w, cell_h],
        every,
        cells,
    })
}

fn blit_cell(sheet: &mut Frame, source: &Frame, dest: Rect) {
    geometry::blit(
        sheet,
        source,
        &Placement {
            dest,
            rotation: 0.0,
        },
        [0.0, 0.0],
        source.size(),
        1.0,
        Blend::Normal,
    );
}

/// The burned-in label: a translucent band with the timecode and the topmost clip id.
///
/// Built as SVG and rasterized through the same text renderer titles use, so there is one
/// font path in the engine and a label cannot render on a machine where a title would not.
fn label_svg(width: u32, height: u32, timecode: &str, clip: Option<&str>) -> String {
    let size = (height as f32 * LABEL_FRACTION).max(MIN_LABEL_PX);
    let lines = if clip.is_some() { 2.15 } else { 1.15 };
    let band = size * lines + size * 0.5;
    let top = height as f32 - band;
    let pad = size * 0.35;
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" \
         viewBox=\"0 0 {width} {height}\">\
         <rect x=\"0\" y=\"{top:.2}\" width=\"{width}\" height=\"{band:.2}\" fill=\"#000\" fill-opacity=\"0.62\"/>\
         <text x=\"{pad:.2}\" y=\"{:.2}\" font-family=\"sans-serif\" font-size=\"{size:.2}\" fill=\"#ffffff\">{}</text>",
        top + size,
        escape_xml(timecode)
    );
    if let Some(clip) = clip {
        svg.push_str(&format!(
            "<text x=\"{pad:.2}\" y=\"{:.2}\" font-family=\"sans-serif\" font-size=\"{:.2}\" fill=\"#8be9fd\">{}</text>",
            top + size * 2.05,
            size * 0.85,
            escape_xml(clip)
        ));
    }
    svg.push_str("</svg>");
    svg
}

/// Render one frame with every layer's bounding box numbered, and return the legend.
///
/// Full scale and no proxies: this image is meant to be looked at closely, and a proxy's
/// softness is exactly the kind of thing a reviewer would report as a problem with the edit.
pub fn annotate_frame<'a>(
    subject: impl Into<Subject<'a>>,
    tool: &'a Toolchain,
    sequence: &SequenceId,
    at: Time,
    output: &Path,
) -> Result<Vec<Annotation>> {
    let subject = subject.into();
    let seq = subject.project.sequence(sequence)?;
    let fps = seq.fps;
    let mut comp = analyze::compositor(subject, tool, sequence, 1.0, false)?;
    let size = comp.size();
    let (mut frame, report) = comp.frame_with_report(at.frame_floor(fps))?;

    let annotations: Vec<Annotation> = report
        .layers
        .iter()
        .enumerate()
        .map(|(index, layer)| Annotation {
            index: index as u32 + 1,
            clip: layer.clip.clone(),
            track: layer.track.clone(),
            name: layer.name.clone(),
            source: layer.source.clone(),
            bbox: layer.bbox,
            opacity: layer.opacity,
            color: ANNOTATION_COLORS[index % ANNOTATION_COLORS.len()].to_string(),
        })
        .collect();

    let overlay = Rasterizer::new().rasterize(&annotation_svg(size, &annotations), size)?;
    geometry::blit(
        &mut frame,
        &overlay,
        &Placement {
            dest: Rect {
                x: 0.0,
                y: 0.0,
                w: size[0] as f32,
                h: size[1] as f32,
            },
            rotation: 0.0,
        },
        [0.0, 0.0],
        overlay.size(),
        1.0,
        Blend::Normal,
    );
    write_png(tool, &frame, output)?;
    Ok(annotations)
}

/// Boxes and badges for every layer.
///
/// The box is clamped into the frame so a layer that hangs off the edge still gets a visible
/// marker — an invisible annotation for the one layer that is mispositioned would be the
/// worst possible failure of this function.
fn annotation_svg(size: [u32; 2], annotations: &[Annotation]) -> String {
    let (fw, fh) = (size[0] as f32, size[1] as f32);
    let badge = (fh * 0.035).max(14.0);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">",
        size[0], size[1], size[0], size[1]
    );
    for annotation in annotations {
        let [x, y, w, h] = annotation.bbox;
        let x0 = x.clamp(0.0, fw - 1.0);
        let y0 = y.clamp(0.0, fh - 1.0);
        let x1 = (x + w).clamp(x0 + 1.0, fw);
        let y1 = (y + h).clamp(y0 + 1.0, fh);
        let stroke = (fh * 0.004).max(2.0);
        svg.push_str(&format!(
            "<rect x=\"{x0:.2}\" y=\"{y0:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"none\" \
             stroke=\"{}\" stroke-width=\"{stroke:.2}\"/>",
            x1 - x0,
            y1 - y0,
            annotation.color
        ));
        let bx = x0 + stroke;
        let by = y0 + stroke;
        svg.push_str(&format!(
            "<rect x=\"{bx:.2}\" y=\"{by:.2}\" width=\"{:.2}\" height=\"{:.2}\" fill=\"{}\" fill-opacity=\"0.85\"/>\
             <text x=\"{:.2}\" y=\"{:.2}\" font-family=\"sans-serif\" font-size=\"{:.2}\" \
             font-weight=\"bold\" fill=\"#000000\">{}</text>",
            badge * 1.4,
            badge,
            annotation.color,
            bx + badge * 0.35,
            by + badge * 0.78,
            badge * 0.8,
            annotation.index
        ));
        svg.push_str(&format!(
            "<text x=\"{:.2}\" y=\"{:.2}\" font-family=\"sans-serif\" font-size=\"{:.2}\" \
             fill=\"{}\">{}</text>",
            bx + badge * 1.6,
            by + badge * 0.78,
            badge * 0.62,
            annotation.color,
            escape_xml(annotation.clip.as_str())
        ));
    }
    svg.push_str("</svg>");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::*;
    use dvs_core::project::{Clip, Source};
    use crate::Subject;
    use dvs_media::read_png;

    const RED: Rgba = Rgba::opaque(200, 40, 40);
    const BLUE: Rgba = Rgba::opaque(30, 60, 200);

    fn five_second_timeline(fixture: &mut Fixture) {
        let track = fixture.video();
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(5, 2)));
        fixture.push(&track, color_clip(BLUE, secs(5, 2), secs(5, 2)));
    }

    #[test]
    fn the_grid_is_the_one_the_arguments_asked_for() {
        let mut fixture = Fixture::new([320, 180]);
        five_second_timeline(&mut fixture);
        let output = fixture.dir.path().join("sheet.png");
        let sequence = fixture.seq();
        let report = contact_sheet(
            &fixture.ws,
            tool(),
            &sequence,
            Time::from_secs(1),
            3,
            480,
            &output,
        )
        .expect("contact sheet");

        assert_eq!(report.cells.len(), 5, "five one-second samples over 5 s");
        assert_eq!(report.columns, 3);
        assert_eq!(report.rows, 2, "five cells three wide need two rows");
        // 480 / 3 columns, and the cell keeps the sequence's 16:9 aspect.
        assert_eq!(report.cell_size, [160, 90]);
        assert_eq!(report.size, [480, 180]);

        // The written file has to have those dimensions, not just the report.
        let written = read_png(tool(), &output).expect("read the sheet back");
        assert_eq!(written.size(), [480, 180]);

        assert_eq!(report.cells[0].timecode, "00:00:00:00");
        assert_eq!(report.cells[4].index, 5);
        assert_eq!(report.cells[4].row, 1);
        assert_eq!(report.cells[4].column, 1);
        assert!(
            report.cells.iter().all(|cell| !cell.clips.is_empty()),
            "every cell names the clip it shows"
        );
    }

    #[test]
    fn a_cell_names_the_clip_that_is_actually_on_screen() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.video();
        let first = fixture.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
        let second = fixture.push(&track, color_clip(BLUE, secs(2, 1), secs(2, 1)));
        let output = fixture.dir.path().join("sheet.png");
        let sequence = fixture.seq();
        let report = contact_sheet(
            &fixture.ws,
            tool(),
            &sequence,
            Time::from_secs(1),
            4,
            320,
            &output,
        )
        .expect("contact sheet");
        assert_eq!(report.cells[0].clips, vec![first.clone()]);
        assert_eq!(report.cells[1].clips, vec![first]);
        assert_eq!(report.cells[2].clips, vec![second.clone()]);
        assert_eq!(report.cells[3].clips, vec![second]);
    }

    #[test]
    fn an_empty_sequence_cannot_be_sheeted() {
        let fixture = Fixture::new([320, 180]);
        let output = fixture.dir.path().join("sheet.png");
        let sequence = fixture.seq();
        let error = contact_sheet(
            &fixture.ws,
            tool(),
            &sequence,
            Time::from_secs(1),
            3,
            480,
            &output,
        )
        .expect_err("nothing to sheet");
        assert!(
            error.to_string().contains("empty"),
            "the error says why: {error}"
        );
    }

    #[test]
    fn every_visible_layer_gets_a_numbered_box_carrying_its_clip_id() {
        let mut fixture = Fixture::new([320, 180]);
        let under = fixture.video();
        let over = fixture.video();
        let background = fixture.push(&under, color_clip(RED, Time::ZERO, secs(2, 1)));
        let title = fixture.title(
            "lower-third",
            [320, 180],
            text_title([320, 180], 40.0, 120.0, 32.0, "sans-serif", "#ffffff", "NAME"),
        );
        let overlay = fixture.push(
            &over,
            Clip::new(Source::Title { title }, Time::ZERO, secs(2, 1)),
        );

        let output = fixture.dir.path().join("frame.png");
        let sequence = fixture.seq();
        let annotations =
            annotate_frame(&fixture.ws, tool(), &sequence, secs(1, 1), &output).expect("annotate");

        assert_eq!(annotations.len(), 2, "two layers are on screen at 1 s");
        assert_eq!(annotations[0].index, 1);
        assert_eq!(annotations[0].clip, background);
        assert_eq!(annotations[1].index, 2);
        assert_eq!(annotations[1].clip, overlay, "layers are bottom track first");
        assert_ne!(
            annotations[0].color, annotations[1].color,
            "two boxes a vision model has to tell apart cannot share a colour"
        );
        // The bottom layer is a full-frame colour clip, so its box is the whole frame.
        assert_eq!(annotations[0].bbox, [0.0, 0.0, 320.0, 180.0]);

        let written = read_png(tool(), &output).expect("read the frame back");
        assert_eq!(written.size(), [320, 180]);
    }

    #[test]
    fn an_annotated_frame_actually_has_the_boxes_drawn_on_it() {
        let mut fixture = Fixture::new([320, 180]);
        let track = fixture.video();
        fixture.push(&track, color_clip(RED, Time::ZERO, secs(2, 1)));
        let plain = fixture.dir.path().join("plain.png");
        let marked = fixture.dir.path().join("marked.png");
        let sequence = fixture.seq();

        let mut comp = crate::analyze::compositor(Subject::from(&fixture.ws), tool(), &sequence, 1.0, false)
            .expect("compositor");
        let frame = comp.frame(30).expect("frame");
        write_png(tool(), &frame, &plain).expect("write the unannotated frame");
        annotate_frame(&fixture.ws, tool(), &sequence, secs(1, 1), &marked).expect("annotate");

        let before = read_png(tool(), &plain).expect("read plain");
        let after = read_png(tool(), &marked).expect("read marked");
        // The overlay has to reach the pixels, not just the report: an annotation nobody can
        // see is worse than none, because the legend claims it is there.
        assert!(
            crate::analyze::mean_abs_diff(&before, &after) > 0.0,
            "the annotated frame must differ from the bare one"
        );
    }
}
