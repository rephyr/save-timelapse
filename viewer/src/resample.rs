//! Averaging an oversized render back down to the size asked for.
//!
//! At 1080p a 2,900 tile base puts about three tiles behind every pixel, so at
//! one sample per pixel whichever entity a pixel lands on wins it outright,
//! and which one that is changes frame to frame as the camera creeps.

use macroquad::texture::Image;

/// Averages `src` down by `factor` on each axis. `factor` of 1 is a copy.
///
/// A box filter, which here is the correct one rather than the cheap one: the
/// samples are a regular grid covering exactly one output pixel's area, so
/// their unweighted mean is that pixel's coverage. Bicubic would reconstruct a
/// continuous signal, and the source is flat quads with hard edges.
///
/// A leftover row or column is dropped rather than averaged over a short
/// window, which would be dimmer than its neighbours and read as a dark
/// fringe. Callers size the target as an exact multiple.
pub fn downsample(src: &Image, factor: u32) -> Image {
    if factor <= 1 {
        return src.clone();
    }
    let f = factor as usize;
    let (sw, sh) = (src.width as usize, src.height as usize);
    let (dw, dh) = (sw / f, sh / f);
    if dw == 0 || dh == 0 {
        return src.clone();
    }
    let area = (f * f) as u32;

    let mut bytes = vec![0u8; dw * dh * 4];
    for y in 0..dh {
        for x in 0..dw {
            // u32, because at 4x sixteen channel samples of 255 overflow a
            // u8 sixteen times over. Alpha is averaged with the rest even
            // though every pixel here is opaque: special-casing it would
            // mean rebuilding the buffer rather than reducing it.
            let mut sum = [0u32; 4];
            for sy in y * f..(y + 1) * f {
                let row = sy * sw * 4;
                for sx in x * f..(x + 1) * f {
                    let i = row + sx * 4;
                    for (channel, total) in sum.iter_mut().enumerate() {
                        *total += src.bytes[i + channel] as u32;
                    }
                }
            }
            let out = (y * dw + x) * 4;
            for (channel, total) in sum.iter().enumerate() {
                bytes[out + channel] = (total / area) as u8;
            }
        }
    }

    Image { bytes, width: dw as u16, height: dh as u16 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `width * height` pixels, each channel taken from `pixel(x, y)`.
    fn image(width: u16, height: u16, pixel: impl Fn(u16, u16) -> [u8; 4]) -> Image {
        let mut bytes = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height {
            for x in 0..width {
                bytes.extend_from_slice(&pixel(x, y));
            }
        }
        Image { bytes, width, height }
    }

    fn pixel_at(img: &Image, x: usize, y: usize) -> [u8; 4] {
        let i = (y * img.width as usize + x) * 4;
        [img.bytes[i], img.bytes[i + 1], img.bytes[i + 2], img.bytes[i + 3]]
    }

    #[test]
    fn a_factor_of_one_changes_nothing() {
        let src = image(3, 2, |x, y| [x as u8, y as u8, 7, 255]);
        let out = downsample(&src, 1);
        assert_eq!(out.width, 3);
        assert_eq!(out.height, 2);
        assert_eq!(out.bytes, src.bytes);
    }

    #[test]
    fn dimensions_divide_by_the_factor() {
        let out = downsample(&image(8, 4, |_, _| [0, 0, 0, 255]), 4);
        assert_eq!((out.width, out.height), (2, 1));
    }

    /// The whole point: a quarter-covered pixel comes out a quarter as
    /// bright, rather than winning or losing the pixel outright.
    #[test]
    fn one_lit_sample_in_four_averages_to_a_quarter() {
        let src = image(2, 2, |x, y| if (x, y) == (0, 0) { [200, 200, 200, 255] } else { [0, 0, 0, 255] });
        let out = downsample(&src, 2);
        assert_eq!((out.width, out.height), (1, 1));
        assert_eq!(pixel_at(&out, 0, 0), [50, 50, 50, 255], "200/4 = 50");
    }

    /// Guards the row stride: getting it wrong averages across the wrong
    /// neighbours, which still produces a plausible-looking image.
    #[test]
    fn each_output_pixel_averages_only_its_own_block() {
        // Left half fully lit, right half dark. Two independent 2x2 blocks.
        let src = image(4, 2, |x, _| if x < 2 { [255, 0, 0, 255] } else { [0, 0, 255, 255] });
        let out = downsample(&src, 2);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(pixel_at(&out, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel_at(&out, 1, 0), [0, 0, 255, 255]);
    }

    /// Sixteen samples at full brightness overflow a u8 accumulator many
    /// times over, so this fails loudly if the sum ever narrows.
    #[test]
    fn a_four_times_reduction_of_white_stays_white() {
        let out = downsample(&image(4, 4, |_, _| [255, 255, 255, 255]), 4);
        assert_eq!(pixel_at(&out, 0, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn a_leftover_edge_is_dropped_rather_than_averaged_short() {
        // 5 wide at 2x: the last column has no partner and must not appear.
        let out = downsample(&image(5, 2, |_, _| [80, 80, 80, 255]), 2);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(pixel_at(&out, 1, 0), [80, 80, 80, 255], "no dark fringe from a short window");
    }
}
