use std::{fs, io::Cursor, path::Path};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};
use anyhow::{Context, Result, bail};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};

const HORIZONTAL_PADDING: u32 = 24;
const VERTICAL_PADDING: u32 = 20;
const LINE_SPACING: f32 = 5.0;
const BACKGROUND: Rgba<u8> = Rgba([13, 17, 23, 255]);
const FOREGROUND: [u8; 3] = [224, 230, 237];

pub fn render_status_png(status: &str, font_path: &Path, font_size: f32) -> Result<Vec<u8>> {
    let font_data = fs::read(font_path)
        .with_context(|| format!("failed to read status font {}", font_path.display()))?;
    let font = FontArc::try_from_vec(font_data).context("failed to parse status font")?;
    let scale = PxScale::from(font_size);
    let scaled = font.as_scaled(scale);
    let line_height = scaled.height() + LINE_SPACING;
    let lines: Vec<&str> = status.lines().collect();
    if lines.is_empty() {
        bail!("cannot render an empty status card");
    }

    let text_width = lines
        .iter()
        .map(|line| line_width(&scaled, line))
        .fold(0.0_f32, f32::max);
    let width = (text_width.ceil() as u32 + HORIZONTAL_PADDING * 2).max(1);
    let height = ((line_height * lines.len() as f32).ceil() as u32 + VERTICAL_PADDING * 2).max(1);
    let mut image = RgbaImage::from_pixel(width, height, BACKGROUND);

    for (line_index, line) in lines.iter().enumerate() {
        let baseline = VERTICAL_PADDING as f32 + scaled.ascent() + line_index as f32 * line_height;
        draw_line(
            &mut image,
            &font,
            &scaled,
            scale,
            line,
            HORIZONTAL_PADDING as f32,
            baseline,
        );
    }

    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut encoded, ImageFormat::Png)
        .context("failed to encode status PNG")?;
    Ok(encoded.into_inner())
}

fn line_width<F: Font>(font: &ab_glyph::PxScaleFont<&F>, line: &str) -> f32 {
    let mut width = 0.0;
    let mut previous = None;
    for character in line.chars() {
        let glyph = font.glyph_id(character);
        if let Some(previous) = previous {
            width += font.kern(previous, glyph);
        }
        width += font.h_advance(glyph);
        previous = Some(glyph);
    }
    width
}

#[allow(clippy::too_many_arguments)]
fn draw_line<F: Font>(
    image: &mut RgbaImage,
    font: &F,
    scaled: &ab_glyph::PxScaleFont<&F>,
    scale: PxScale,
    line: &str,
    mut x: f32,
    baseline: f32,
) {
    let mut previous = None;
    for character in line.chars() {
        let glyph_id = scaled.glyph_id(character);
        if let Some(previous) = previous {
            x += scaled.kern(previous, glyph_id);
        }
        let glyph = glyph_id.with_scale_and_position(scale, point(x, baseline));
        if let Some(outlined) = font.outline_glyph(glyph) {
            let bounds = outlined.px_bounds();
            outlined.draw(|glyph_x, glyph_y, coverage| {
                let pixel_x = bounds.min.x.floor() as i32 + glyph_x as i32;
                let pixel_y = bounds.min.y.floor() as i32 + glyph_y as i32;
                if pixel_x < 0 || pixel_y < 0 {
                    return;
                }
                let Some(pixel) = image.get_pixel_mut_checked(pixel_x as u32, pixel_y as u32)
                else {
                    return;
                };
                let alpha = coverage.clamp(0.0, 1.0);
                for (channel, foreground) in pixel.0[..3].iter_mut().zip(FOREGROUND) {
                    *channel = (f32::from(*channel) * (1.0 - alpha) + f32::from(foreground) * alpha)
                        .round() as u8;
                }
            });
        }
        x += scaled.h_advance(glyph_id);
        previous = Some(glyph_id);
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::render_status_png;

    #[test]
    fn renders_box_drawing_and_progress_characters_to_png() {
        let png = render_status_png(
            "╭────────╮\n│ [██░░] │\n╰────────╯",
            Path::new("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
            18.0,
        )
        .unwrap();
        assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(png.len() > 500);
    }
}
