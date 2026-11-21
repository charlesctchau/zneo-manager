// Z-NEO Manager - portable manager for the HM LAB Z-NEO 8K USB hub LCD.
// Copyright (C) 2026 charlesctchau
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// The device protocol (packet layout, flash layout, config block) is derived from
// hm-hub by Creationsss (https://github.com/Creationsss/hm-hub),
// licensed AGPL-3.0-or-later.
//
// This program is free software: you can redistribute it and/or modify it under
// the terms of the GNU Affero General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option) any
// later version. It is distributed WITHOUT ANY WARRANTY; see the LICENSE file.

//! Image decoding and conversion to the device's RGB565 320x170 frames.

use anyhow::{bail, Context, Result};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};
use std::path::{Path, PathBuf};

use crate::proto::{Album, DISPLAY_HEIGHT, DISPLAY_WIDTH};

pub const IMAGE_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "bmp", "webp", "gif"];

pub fn is_image_path(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// List supported images in `dir`, sorted case-insensitively by path.
pub fn list_images(dir: &Path, recursive: bool) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect(dir, recursive, &mut out, 0)?;
    out.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    Ok(out)
}

fn collect(dir: &Path, recursive: bool, out: &mut Vec<PathBuf>, depth: u32) -> Result<()> {
    let rd = std::fs::read_dir(dir).with_context(|| format!("cannot read folder {}", dir.display()))?;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recursive && depth < 6 {
                let _ = collect(&path, true, out, depth + 1);
            }
        } else if is_image_path(&path) {
            out.push(path);
        }
    }
    Ok(())
}

pub fn rgba_to_rgb565(rgba: &[u8]) -> Vec<u8> {
    let n = rgba.len() / 4;
    let mut buf = vec![0u8; n * 2];
    for i in 0..n {
        // Composite over black so transparent pixels do not leak stray colour.
        let a = rgba[4 * i + 3] as u16;
        let r = rgba[4 * i] as u16 * a / 255;
        let g = rgba[4 * i + 1] as u16 * a / 255;
        let b = rgba[4 * i + 2] as u16 * a / 255;
        let px = ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | ((b & 0xF8) >> 3);
        buf[2 * i] = (px >> 8) as u8;
        buf[2 * i + 1] = (px & 0xFF) as u8;
    }
    buf
}

pub fn rgb565_to_rgba(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() * 2);
    for px in data.chunks_exact(2) {
        let v = ((px[0] as u16) << 8) | px[1] as u16;
        let (r, g, b) = ((v >> 11) & 0x1F, (v >> 5) & 0x3F, v & 0x1F);
        out.push(((r << 3) | (r >> 2)) as u8);
        out.push(((g << 2) | (g >> 4)) as u8);
        out.push(((b << 3) | (b >> 2)) as u8);
        out.push(255);
    }
    out
}

/// Load an image or GIF as an album. `max_frames` bounds GIF decoding.
pub fn load_album(path: &Path, crop: bool, max_frames: usize) -> Result<Album> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "gif" => load_gif(path, crop, max_frames),
        e if IMAGE_EXTS.contains(&e) => load_static(path, crop),
        _ => bail!("unsupported file type: {}", path.display()),
    }
}

fn load_static(path: &Path, crop: bool) -> Result<Album> {
    let reader = ImageReader::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .with_guessed_format()?;
    let mut decoder = reader.into_decoder().with_context(|| format!("cannot decode {}", path.display()))?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)
        .with_context(|| format!("cannot decode {}", path.display()))?;
    img.apply_orientation(orientation);
    let fitted = fit(&img, crop);
    Ok(Album { frames: vec![rgba_to_rgb565(fitted.as_raw())], delay_ms: 0 })
}

fn load_gif(path: &Path, crop: bool, max_frames: usize) -> Result<Album> {
    let file = std::fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut dec = opts.read_info(file).context("invalid GIF")?;
    let (gw, gh) = (dec.width() as u32, dec.height() as u32);
    let mut canvas = vec![0u8; (gw * gh * 4) as usize];
    let mut frames = Vec::new();
    let mut delay_ms = 0u16;

    while let Some(frame) = dec.read_next_frame().context("invalid GIF frame")? {
        if frames.len() >= max_frames {
            bail!("GIF has more than {max_frames} frames (device limit)");
        }
        if delay_ms == 0 && frame.delay > 0 {
            delay_ms = frame.delay.saturating_mul(10);
        }
        let (fx, fy, fw, fh) = (frame.left as u32, frame.top as u32, frame.width as u32, frame.height as u32);
        for y in 0..fh {
            for x in 0..fw {
                let (dx, dy) = (fx + x, fy + y);
                if dx >= gw || dy >= gh {
                    continue;
                }
                let s = ((y * fw + x) * 4) as usize;
                if frame.buffer[s + 3] > 0 {
                    let d = ((dy * gw + dx) * 4) as usize;
                    canvas[d..d + 4].copy_from_slice(&frame.buffer[s..s + 4]);
                }
            }
        }
        let img = RgbaImage::from_raw(gw, gh, canvas.clone()).context("bad GIF frame size")?;
        let fitted = fit(&DynamicImage::from(img), crop);
        frames.push(rgba_to_rgb565(fitted.as_raw()));

        if frame.dispose == gif::DisposalMethod::Background {
            for y in 0..fh {
                for x in 0..fw {
                    let (dx, dy) = (fx + x, fy + y);
                    if dx < gw && dy < gh {
                        let d = ((dy * gw + dx) * 4) as usize;
                        canvas[d..d + 4].copy_from_slice(&[0, 0, 0, 0]);
                    }
                }
            }
        }
    }
    if frames.is_empty() {
        bail!("GIF has no frames");
    }
    Ok(Album { frames, delay_ms })
}

/// Scale to 320x170: crop-to-fill or letterbox on black.
fn fit(img: &DynamicImage, crop: bool) -> RgbaImage {
    use image::imageops::FilterType::Lanczos3;
    let (tw, th) = (DISPLAY_WIDTH, DISPLAY_HEIGHT);
    let (sw, sh) = (img.width().max(1) as f64, img.height().max(1) as f64);
    let target_aspect = tw as f64 / th as f64;
    if crop {
        let (cw, ch, cx, cy) = if sw / sh > target_aspect {
            let cw = sh * target_aspect;
            (cw, sh, (sw - cw) / 2.0, 0.0)
        } else {
            let ch = sw / target_aspect;
            (sw, ch, 0.0, (sh - ch) / 2.0)
        };
        img.crop_imm(cx as u32, cy as u32, (cw as u32).max(1), (ch as u32).max(1))
            .resize_exact(tw, th, Lanczos3)
            .to_rgba8()
    } else {
        let scaled = img.resize(tw, th, Lanczos3).to_rgba8();
        let mut canvas = RgbaImage::new(tw, th);
        let ox = (tw - scaled.width()) / 2;
        let oy = (th - scaled.height()) / 2;
        image::imageops::overlay(&mut canvas, &scaled, ox as i64, oy as i64);
        canvas
    }
}

/// Render exactly what the device will show (first frame) as RGBA 320x170.
pub fn preview_rgba(path: &Path, crop: bool) -> Result<Vec<u8>> {
    let album = load_album(path, crop, 1000)?;
    Ok(rgb565_to_rgba(&album.frames[0]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb565_roundtrip_is_close() {
        let rgba = vec![255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 0];
        let back = rgb565_to_rgba(&rgba_to_rgb565(&rgba));
        assert_eq!(&back[0..4], &[255, 0, 0, 255]);
        assert_eq!(&back[4..8], &[0, 255, 0, 255]);
        assert_eq!(&back[8..12], &[0, 0, 255, 255]);
        // fully transparent white becomes black
        assert_eq!(&back[12..16], &[0, 0, 0, 255]);
    }

    #[test]
    fn crop_and_letterbox_produce_display_size() {
        let img = DynamicImage::from(RgbaImage::from_pixel(100, 400, image::Rgba([10, 20, 30, 255])));
        for crop in [true, false] {
            let out = fit(&img, crop);
            assert_eq!((out.width(), out.height()), (DISPLAY_WIDTH, DISPLAY_HEIGHT));
        }
    }

    #[test]
    fn png_loads_end_to_end() {
        let dir = std::env::temp_dir().join("zneo_test_png");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.png");
        RgbaImage::from_pixel(64, 64, image::Rgba([200, 100, 50, 255])).save(&p).unwrap();
        let album = load_album(&p, true, 77).unwrap();
        assert_eq!(album.frames.len(), 1);
        assert_eq!(album.frames[0].len(), crate::proto::FRAME_PIXEL_SIZE);
        assert!(list_images(&dir, false).unwrap().contains(&p));
    }

    #[test]
    fn corrupt_file_is_rejected() {
        let dir = std::env::temp_dir().join("zneo_test_bad");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("bad.jpg");
        std::fs::write(&p, b"not an image").unwrap();
        assert!(load_album(&p, true, 77).is_err());
    }
}
