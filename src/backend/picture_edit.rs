//! Draws on, crops and turns a picture on its way out, so an attachment can
//! be marked up and trimmed before it is sent.

use std::path::{Path, PathBuf};

use crate::model::{Mark, PictureBlocks, PictureCrop, PictureSource};

/// Quality of the picture written back. The send path passes a JPEG through
/// untouched, so this is the only lossy step for one.
const QUALITY: u8 = 92;

/// A picture's size, read without keeping its pixels.
pub fn inspect(bytes: &[u8]) -> Result<(u32, u32), String> {
    let picture = image::load_from_memory(bytes)
        .map_err(|error| format!("This picture could not be read: {error}"))?;
    Ok((picture.width(), picture.height()))
}

/// The colours a pixelated region of this picture is painted in, read
/// from the same pixels writing the edit reads.
pub fn sample(source: &PictureSource, width: u32, height: u32) -> Result<PictureBlocks, String> {
    let picture = match source {
        PictureSource::File(path) => {
            let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
            image::load_from_memory(&bytes)
                .map_err(|error| format!("This picture could not be read: {error}"))?
                .to_rgba8()
        }
        PictureSource::Pasted(rgba) => {
            image::RgbaImage::from_raw(width.max(1), height.max(1), rgba.to_vec())
                .ok_or_else(|| "This picture could not be read".to_owned())?
        }
    };
    Ok(blocks(&picture))
}

/// Every pixelation block of the picture, in its average colour.
fn blocks(picture: &image::RgbaImage) -> PictureBlocks {
    let (width, height) = picture.dimensions();
    PictureBlocks::new(width, height, |cell| average(picture, cell))
}

/// The picture with its marks drawn, then turned by whole quarter turns
/// clockwise, then cropped. The marks are in the source's own pixels; the
/// crop is measured in the turned picture, which is what the editor showed.
pub fn edit(bytes: &[u8], crop: PictureCrop, turns: u8, marks: &[Mark]) -> Result<Vec<u8>, String> {
    let picture = image::load_from_memory(bytes)
        .map_err(|error| format!("This picture could not be read: {error}"))?
        .to_rgba8();
    turned_and_cropped(marked(picture, marks)?, crop, turns)
}

/// The same for a pasted picture, which has no file to read back.
pub fn edit_pasted(
    rgba: &[u8],
    width: u32,
    height: u32,
    crop: PictureCrop,
    turns: u8,
    marks: &[Mark],
) -> Result<Vec<u8>, String> {
    let picture = image::RgbaImage::from_raw(width.max(1), height.max(1), rgba.to_vec())
        .ok_or_else(|| "This picture could not be read".to_owned())?;
    turned_and_cropped(marked(picture, marks)?, crop, turns)
}

/// Draws on, turns and crops a staged picture, writes the result in `dir`
/// beside the other media, and answers with the file it wrote. The name comes
/// from the picture's own bytes, so the same edit of the same picture is
/// written once.
pub fn write(
    source: &PictureSource,
    width: u32,
    height: u32,
    crop: PictureCrop,
    turns: u8,
    marks: &[Mark],
    dir: &Path,
) -> Result<PathBuf, String> {
    let picture = match source {
        PictureSource::File(path) => std::fs::read(path)
            .map_err(|error| error.to_string())
            .and_then(|bytes| edit(&bytes, crop, turns, marks)),
        PictureSource::Pasted(rgba) => edit_pasted(rgba, width, height, crop, turns, marks),
    }?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let name = super::sticker_store::content_hash(&picture);
    let path = dir.join(format!("{name}.jpg"));
    std::fs::write(&path, picture).map_err(|error| error.to_string())?;
    Ok(path)
}

/// The picture with its marks drawn on in order. Lines are drawn with
/// anti-aliasing onto a see-through layer and laid over the picture; a
/// pixelated region paints each block in the average of the untouched
/// picture under it, from the same `PictureBlocks` the editor's preview
/// paints with.
fn marked(mut picture: image::RgbaImage, marks: &[Mark]) -> Result<image::RgbaImage, String> {
    if marks.is_empty() {
        return Ok(picture);
    }
    let (width, height) = picture.dimensions();
    let mut layer = blank_layer(width, height)?;
    let untouched = marks
        .iter()
        .any(|mark| matches!(mark, Mark::Pixelate { .. }))
        .then(|| blocks(&picture));
    let mut drawn = false;
    for mark in marks {
        match (mark, &untouched) {
            (Mark::Pixelate { from, to }, Some(untouched)) => {
                if std::mem::take(&mut drawn) {
                    lay_over(&mut picture, &layer);
                    layer.fill(tiny_skia::Color::TRANSPARENT);
                }
                for cell in crate::model::pixel_cells(*from, *to, width, height) {
                    let Some(colour) = untouched.colour(cell) else {
                        continue;
                    };
                    let colour = image::Rgba(colour);
                    for y in cell[1]..cell[3] {
                        for x in cell[0]..cell[2] {
                            picture.put_pixel(x, y, colour);
                        }
                    }
                }
            }
            _ => drawn |= stroke(&mut layer, mark, tiny_skia::Transform::identity()),
        }
    }
    if drawn {
        lay_over(&mut picture, &layer);
    }
    Ok(picture)
}

/// A see-through layer this big to draw marks on. Sending the picture
/// without the marks the person drew, a pixelated face among them, is worse
/// than not sending it, so a layer that cannot be made is an error.
fn blank_layer(width: u32, height: u32) -> Result<tiny_skia::Pixmap, String> {
    tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| format!("Could not draw on a picture {width} by {height} pixels"))
}

/// The marks as the editor shows them over the picture, drawn by the same
/// strokes writing draws: on a see-through layer `size` pixels big that
/// covers the source turned `turns` quarters clockwise. A pixelated block
/// is opaque, in `cell`'s colour, and hides whatever was drawn under it as
/// writing it does.
pub(crate) fn preview_layer<'a>(
    marks: impl IntoIterator<Item = &'a Mark>,
    (width, height): (u32, u32),
    turns: u8,
    [wide, tall]: [u32; 2],
    cell: impl Fn([u32; 4]) -> [u8; 3],
) -> Option<tiny_skia::Pixmap> {
    let mut layer = tiny_skia::Pixmap::new(wide, tall)?;
    let (shown_wide, shown_tall) = if turns.is_multiple_of(2) {
        (width, height)
    } else {
        (height, width)
    };
    let transform = turned(turns, width, height).post_scale(
        wide as f32 / shown_wide.max(1) as f32,
        tall as f32 / shown_tall.max(1) as f32,
    );
    // Without anti-aliasing, blocks that meet share their edge exactly,
    // and no seam lets the picture through.
    let mut paint = tiny_skia::Paint {
        anti_alias: false,
        ..Default::default()
    };
    for mark in marks {
        let Mark::Pixelate { from, to } = mark else {
            stroke(&mut layer, mark, transform);
            continue;
        };
        for block in crate::model::pixel_cells(*from, *to, width, height) {
            let [r, g, b] = cell(block);
            paint.set_color_rgba8(r, g, b, u8::MAX);
            let [left, top, right, bottom] = block.map(|edge| edge as f32);
            if let Some(rect) = tiny_skia::Rect::from_ltrb(left, top, right, bottom) {
                layer.fill_rect(rect, &paint, transform, None);
            }
        }
    }
    Some(layer)
}

/// Carries the source's pixels to the picture turned `turns` quarters
/// clockwise, as `crate::model::turned_point` does.
fn turned(turns: u8, width: u32, height: u32) -> tiny_skia::Transform {
    let (w, h) = (width as f32, height as f32);
    match turns % 4 {
        1 => tiny_skia::Transform::from_row(0.0, 1.0, -1.0, 0.0, h, 0.0),
        2 => tiny_skia::Transform::from_row(-1.0, 0.0, 0.0, -1.0, w, h),
        3 => tiny_skia::Transform::from_row(0.0, -1.0, 1.0, 0.0, 0.0, w),
        _ => tiny_skia::Transform::identity(),
    }
}

/// Draws one inked mark on `layer` through `transform`, answering whether
/// it drew anything.
fn stroke(layer: &mut tiny_skia::Pixmap, mark: &Mark, transform: tiny_skia::Transform) -> bool {
    use tiny_skia::{LineCap, LineJoin, PathBuilder};
    let Some([r, g, b, a]) = mark.rgba() else {
        return false;
    };
    let mut paint = tiny_skia::Paint::default();
    paint.set_color_rgba8(r, g, b, a);
    paint.anti_alias = true;
    let line = |points: &[[f32; 2]]| {
        let mut path = PathBuilder::new();
        for (at, [x, y]) in points.iter().enumerate() {
            if at == 0 {
                path.move_to(*x, *y);
            } else {
                path.line_to(*x, *y);
            }
        }
        path
    };
    let (path, width, line_cap, line_join) = match mark {
        Mark::Pen { points, width, .. } | Mark::Highlighter { points, width, .. }
            if points.len() < 2 =>
        {
            // A click leaves a dot as wide as the line.
            let Some(&[x, y]) = points.first() else {
                return false;
            };
            let Some(dot) = PathBuilder::from_circle(x, y, width / 2.0) else {
                return false;
            };
            layer.fill_path(&dot, &paint, tiny_skia::FillRule::Winding, transform, None);
            return true;
        }
        Mark::Pen { points, width, .. } => (line(points), *width, LineCap::Round, LineJoin::Round),
        Mark::Highlighter { points, width, .. } => {
            (line(points), *width, LineCap::Butt, LineJoin::Round)
        }
        Mark::Arrow {
            from, to, width, ..
        } => {
            let mut path = line(&[*from, *to]);
            for barb in crate::model::arrow_barbs(*from, *to, *width) {
                path.move_to(barb[0], barb[1]);
                path.line_to(to[0], to[1]);
            }
            (path, *width, LineCap::Round, LineJoin::Round)
        }
        Mark::Rectangle {
            from, to, width, ..
        } => {
            let mut path = line(&[*from, [to[0], from[1]], *to, [from[0], to[1]]]);
            path.close();
            (path, *width, LineCap::Butt, LineJoin::Miter)
        }
        Mark::Pixelate { .. } => return false,
    };
    let Some(path) = path.finish() else {
        return false;
    };
    let stroke = tiny_skia::Stroke {
        width,
        line_cap,
        line_join,
        ..Default::default()
    };
    layer.stroke_path(&path, &paint, &stroke, transform, None);
    true
}

/// Lays the premultiplied layer over the picture's straight colours.
fn lay_over(picture: &mut image::RgbaImage, layer: &tiny_skia::Pixmap) {
    for (pixel, over) in picture.pixels_mut().zip(layer.pixels()) {
        let over_alpha = f32::from(over.alpha()) / 255.0;
        if over_alpha == 0.0 {
            continue;
        }
        let under_alpha = f32::from(pixel[3]) / 255.0;
        let alpha = over_alpha + under_alpha * (1.0 - over_alpha);
        let blend = |over: u8, under: u8| {
            let premultiplied = f32::from(over) / 255.0
                + f32::from(under) / 255.0 * under_alpha * (1.0 - over_alpha);
            (premultiplied / alpha * 255.0).round().clamp(0.0, 255.0) as u8
        };
        *pixel = image::Rgba([
            blend(over.red(), pixel[0]),
            blend(over.green(), pixel[1]),
            blend(over.blue(), pixel[2]),
            (alpha * 255.0).round() as u8,
        ]);
    }
}

/// The average colour of a block, weighed by how opaque each pixel is, so the
/// hidden colour of see-through pixels does not tint it.
fn average(picture: &image::RgbaImage, [left, top, right, bottom]: [u32; 4]) -> [u8; 4] {
    let (mut colour, mut alpha, mut count) = ([0u64; 3], 0u64, 0u64);
    for y in top..bottom {
        for x in left..right {
            let pixel = picture.get_pixel(x, y);
            let weight = u64::from(pixel[3]);
            for (sum, channel) in colour.iter_mut().zip(&pixel.0[..3]) {
                *sum += u64::from(*channel) * weight;
            }
            alpha += weight;
            count += 1;
        }
    }
    if alpha == 0 {
        return [0; 4];
    }
    let [r, g, b] = colour.map(|sum| ((sum + alpha / 2) / alpha) as u8);
    [r, g, b, ((alpha + count / 2) / count) as u8]
}

/// Turns the picture, keeps the region, and writes it out.
fn turned_and_cropped(
    picture: image::RgbaImage,
    crop: PictureCrop,
    turns: u8,
) -> Result<Vec<u8>, String> {
    let picture = match turns % 4 {
        1 => image::imageops::rotate90(&picture),
        2 => image::imageops::rotate180(&picture),
        3 => image::imageops::rotate270(&picture),
        _ => picture,
    };
    let (width, height) = picture.dimensions();
    let crop = crop.clamped(width, height);
    let kept =
        image::imageops::crop_imm(&picture, crop.x, crop.y, crop.width, crop.height).to_image();
    encode(&kept)
}

/// Writes the picture as a JPEG. JPEG has no alpha, so a picture that had
/// see-through pixels is flattened onto white, as making a sticker from one
/// does.
fn encode(picture: &image::RgbaImage) -> Result<Vec<u8>, String> {
    use image::ImageEncoder;
    let (width, height) = picture.dimensions();
    let mut flat = image::RgbImage::new(width, height);
    for (flat, source) in flat.pixels_mut().zip(picture.pixels()) {
        let alpha = f32::from(source[3]) / 255.0;
        for channel in 0..3 {
            let value = f32::from(source[channel]) * alpha + 255.0 * (1.0 - alpha);
            flat[channel] = value.round() as u8;
        }
    }
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, QUALITY)
        .write_image(flat.as_raw(), width, height, image::ExtendedColorType::Rgb8)
        .map_err(|error| format!("Could not write the picture: {error}"))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A picture whose top-left pixel is red, so a turn can be told apart
    /// from the other direction.
    fn png(width: u32, height: u32) -> Vec<u8> {
        use image::ImageEncoder;
        let picture = image::RgbaImage::from_fn(width, height, |x, y| {
            if x == 0 && y == 0 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&picture, width, height, image::ExtendedColorType::Rgba8)
            .expect("encodes");
        bytes
    }

    fn decoded(bytes: &[u8]) -> image::RgbaImage {
        image::load_from_memory(bytes).expect("decodes").to_rgba8()
    }

    #[test]
    fn a_picture_reports_its_size() {
        assert_eq!(inspect(&png(800, 600)).expect("reads"), (800, 600));
        assert!(inspect(b"not a picture").is_err());
    }

    #[test]
    fn a_crop_keeps_only_the_region_it_names() {
        let bytes = edit(
            &png(800, 600),
            PictureCrop {
                x: 100,
                y: 50,
                width: 200,
                height: 120,
            },
            0,
            &[],
        )
        .expect("edits");
        assert_eq!(decoded(&bytes).dimensions(), (200, 120));
    }

    #[test]
    fn a_quarter_turn_clockwise_moves_the_top_left_to_the_top_right() {
        let bytes = edit(&png(4, 2), PictureCrop::full(2, 4), 1, &[]).expect("edits");
        let picture = decoded(&bytes);
        assert_eq!(picture.dimensions(), (2, 4));
        // The red corner started top-left of a wide picture, so turning
        // clockwise carries it to the top-right of a tall one. The write is
        // lossy, so the channels are compared rather than their exact values.
        let top_right = picture.get_pixel(1, 0);
        assert!(
            top_right[0] > 200 && top_right[2] < 60,
            "red at the top right, got {top_right:?}"
        );
        let top_left = picture.get_pixel(0, 0);
        assert!(
            top_left[2] > 200 && top_left[0] < 60,
            "blue at the top left, got {top_left:?}"
        );
    }

    #[test]
    fn turning_and_cropping_measure_the_crop_after_the_turn() {
        // The same region the editor showed, in the turned picture.
        let bytes = edit(
            &png(4, 2),
            PictureCrop {
                x: 1,
                y: 0,
                width: 1,
                height: 2,
            },
            1,
            &[],
        )
        .expect("edits");
        let picture = decoded(&bytes);
        assert_eq!(picture.dimensions(), (1, 2));
        let corner = picture.get_pixel(0, 0);
        assert!(
            corner[0] > 200 && corner[2] < 60,
            "the red corner, got {corner:?}"
        );
    }

    #[test]
    fn see_through_pixels_are_flattened_onto_white() {
        use image::ImageEncoder;
        let mut source = Vec::new();
        image::codecs::png::PngEncoder::new(&mut source)
            .write_image(
                image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 0, 0])).as_raw(),
                8,
                8,
                image::ExtendedColorType::Rgba8,
            )
            .expect("encodes");
        let picture = decoded(&edit(&source, PictureCrop::full(8, 8), 0, &[]).expect("edits"));
        let corner = picture.get_pixel(2, 2);
        assert!(corner[0] > 240 && corner[1] > 240 && corner[2] > 240);
    }

    #[test]
    fn a_pasted_picture_is_turned_and_cropped_like_a_file() {
        // The same red corner as the encoded fixture, as straight RGBA.
        let (width, height) = (4u32, 2u32);
        let mut rgba = [0u8, 0, 255, 255].repeat((width * height) as usize);
        rgba[..4].copy_from_slice(&[255, 0, 0, 255]);
        let bytes =
            edit_pasted(&rgba, width, height, PictureCrop::full(2, 4), 1, &[]).expect("edits");
        let picture = decoded(&bytes);
        assert_eq!(picture.dimensions(), (2, 4));
        let top_right = picture.get_pixel(1, 0);
        assert!(
            top_right[0] > 200 && top_right[2] < 60,
            "red at the top right, got {top_right:?}"
        );
    }

    #[test]
    fn a_pasted_picture_that_is_not_rgba_is_refused() {
        assert!(edit_pasted(&[0, 0, 0], 4, 4, PictureCrop::full(4, 4), 0, &[]).is_err());
    }

    #[test]
    fn writing_a_crop_puts_it_under_its_own_bytes() {
        let dir = std::env::temp_dir().join("zapfasterk-picture-edit-write");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("makes the directory");
        let source = dir.join("source.png");
        std::fs::write(&source, png(800, 600)).expect("writes the source");
        let crop = PictureCrop {
            x: 100,
            y: 50,
            width: 200,
            height: 120,
        };
        let written = write(
            &PictureSource::File(source.clone()),
            800,
            600,
            crop,
            0,
            &[],
            &dir,
        )
        .expect("writes the crop");
        assert_eq!(
            written.extension().and_then(|kind| kind.to_str()),
            Some("jpg")
        );
        assert_eq!(
            decoded(&std::fs::read(&written).expect("reads")).dimensions(),
            (200, 120)
        );
        // The same crop of the same picture lands on the same name, so a
        // second look at it writes nothing new.
        let again = write(&PictureSource::File(source), 800, 600, crop, 0, &[], &dir)
            .expect("writes it again");
        assert_eq!(again, written);
        // A pasted picture goes the same way, from the pixels it was given.
        let pasted = std::sync::Arc::new([0u8, 0, 255, 255].repeat(800 * 600));
        let pasted = write(&PictureSource::Pasted(pasted), 800, 600, crop, 0, &[], &dir)
            .expect("writes the pasted crop");
        assert_eq!(
            decoded(&std::fs::read(&pasted).expect("reads")).dimensions(),
            (200, 120)
        );
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }

    #[test]
    fn writing_a_crop_of_a_missing_picture_says_so() {
        let dir = std::env::temp_dir().join("zapfasterk-picture-edit-missing");
        let error = write(
            &PictureSource::File(dir.join("gone.png")),
            4,
            4,
            PictureCrop::full(4, 4),
            0,
            &[],
            &dir,
        )
        .expect_err("refuses");
        assert!(!error.is_empty());
    }

    fn red_pen() -> crate::model::Mark {
        crate::model::Mark::Pen {
            points: vec![[10.0, 10.0], [30.0, 10.0]],
            ink: crate::model::Ink::Red,
            width: 4.0,
        }
    }

    /// A drawn line paints the picture's pixels, and nothing else changes.
    #[test]
    fn marks_are_drawn_onto_the_picture_before_it_is_cropped() {
        let bytes =
            edit(&png(100, 100), PictureCrop::full(100, 100), 0, &[red_pen()]).expect("edits");
        let picture = decoded(&bytes);
        let line = picture.get_pixel(20, 10);
        assert!(
            line[0] > 180 && line[1] < 100 && line[2] < 100,
            "red line, got {line:?}"
        );
        let away = picture.get_pixel(20, 50);
        assert!(
            away[2] > 200 && away[0] < 60,
            "untouched blue, got {away:?}"
        );
    }

    /// A pixelated region paints each block in the average of the untouched
    /// picture under it.
    #[test]
    fn pixelated_regions_paint_averages_of_the_untouched_picture() {
        use crate::model::Mark;
        use image::ImageEncoder;
        let image = image::RgbaImage::from_pixel(64, 64, image::Rgba([255, 0, 0, 255]));
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), 64, 64, image::ExtendedColorType::Rgba8)
            .expect("encodes");
        let bytes = edit(
            &bytes,
            PictureCrop::full(64, 64),
            0,
            &[Mark::Pixelate {
                from: [0.0, 0.0],
                to: [64.0, 64.0],
            }],
        )
        .expect("edits");
        let picture = decoded(&bytes);
        let cell = picture.get_pixel(4, 4);
        assert!(
            cell[0] > 200 && cell[1] < 60 && cell[2] < 60,
            "red average, got {cell:?}"
        );
    }
}
