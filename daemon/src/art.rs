//! Which unique an unidentified item is, from its art.
//!
//! Unidentified uniques show their own art in the game, so the layer sends
//! the pixels around the cursor from when the item was checked, and we
//! compare them with the art of each unique on that base (poe.ninja's
//! `icon`). Only a handful of uniques share a base, so this picks one of
//! a few pictures, not one of every unique.
//!
//! The comparison is a masked normalized cross-correlation: the art's
//! transparent pixels are ignored, so the cell's background (and its hover
//! highlight) doesn't count. The item must cover the cursor, which leaves
//! few places to try. The art's size on screen is known up to the stash
//! layout: inventory and normal stash cells, or a quad tab's half-size ones.

use std::io::Cursor;

/// Art pixels per inventory cell in poe.ninja's icons (a ring is 47x47).
const ART_CELL: f32 = 47.0;
/// The art's size on screen, as a fraction of a cell's pitch (the grid
/// lines take the rest), with some slack for how the layer estimates the
/// pitch: about 0.94 in the game.
const SCALES: (f32, f32) = (0.84, 1.06);
/// The same in a quad stash tab, where cells are half as big.
const QUAD_SCALES: (f32, f32) = (0.42, 0.53);
/// Step between tried sizes (as a ratio).
const SCALE_STEP: f32 = 1.03;
/// The coarse pass works at this size: the art's longer side, in pixels...
const COARSE_SIDE: f32 = 24.0;
/// ...but with cells no smaller than this: a thin staff's six sockets leave
/// little of its art to compare.
const COARSE_MIN_CELL: f32 = 20.0;
/// The fine pass works at most at this cell size (pixels), the art's own.
const FINE_CELL: f32 = 48.0;
/// The fine pass refines this many of the coarse pass's best places.
const REFINE: usize = 3;
/// The fine pass scores the best-fitting share of the art's pixels, so a
/// little in front of the art (a stack size, a quality outline) doesn't
/// count against it. Only a little: the more is left out, the better wrong
/// art scores too. Sockets, which cover much more, are left out by where
/// they are (see socket_centres).
const KEEP: f32 = 0.9;
/// A socket's radius, and half its link's width, in cells. Generous: what's
/// left out of the art around them costs little.
const SOCKET_RADIUS: f32 = 0.40;
const LINK_HALF_WIDTH: f32 = 0.16;

/// A match must score at least this (1.0: identical). In the fixtures, the
/// hovered item's own art scores 0.93-0.97, other arts at most 0.71, and a
/// staff's art, mostly hidden by six sockets, 0.72 on a robe....
const MIN_SCORE: f32 = 0.8;
/// ...and beat every other art by this much.
const MIN_LEAD: f32 = 0.15;

/// The pixels the layer sent: RGB rows, the cursor in them, and an
/// inventory cell's pitch in the same pixels.
pub struct Capture {
    pub width: usize,
    pub height: usize,
    pub rgb: Vec<u8>,
    pub cursor: (f32, f32),
    pub cell: f32,
}

/// A unique's art: RGBA (0-255) and its size in inventory cells.
pub struct Art {
    width: usize,
    height: usize,
    rgba: Vec<[f32; 4]>,
    cells: (u32, u32),
}

impl Art {
    /// `cells` from the art's URL if known (see cells_from_url), else from
    /// the image's size.
    pub fn from_png(bytes: &[u8], cells: Option<(u32, u32)>) -> Result<Art, String> {
        let mut decoder = png::Decoder::new(Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().map_err(|e| format!("art: {e}"))?;
        let mut buf = vec![0u8; reader.output_buffer_size().ok_or("art: image too big")?];
        let info = reader.next_frame(&mut buf).map_err(|e| format!("art: {e}"))?;
        let (width, height) = (info.width as usize, info.height as usize);
        let channels = info.color_type.samples();
        let rgba: Vec<[f32; 4]> = buf[..info.buffer_size()]
            .chunks_exact(channels)
            .map(|p| match channels {
                4 => [p[0] as f32, p[1] as f32, p[2] as f32, p[3] as f32],
                3 => [p[0] as f32, p[1] as f32, p[2] as f32, 255.0],
                2 => [p[0] as f32, p[0] as f32, p[0] as f32, p[1] as f32],
                _ => [p[0] as f32, p[0] as f32, p[0] as f32, 255.0],
            })
            .collect();
        if width == 0 || height == 0 || rgba.len() != width * height {
            return Err("art: unexpected image layout".into());
        }
        let cells = cells.unwrap_or_else(|| {
            let cells = |px: usize| ((px as f32 / ART_CELL).round() as u32).clamp(1, 4);
            (cells(width), cells(height))
        });
        Ok(Art { width, height, rgba, cells })
    }

    /// The art at `w`x`h` pixels: colours and a mask of its opaque pixels,
    /// less where the game draws the item's `sockets` over it.
    fn template(&self, w: usize, h: usize, sockets: u32) -> Template {
        let centres = socket_centres(self.cells, sockets);
        let cell = w as f32 / self.cells.0 as f32;
        let covered = |x: usize, y: usize| {
            let p = ((x as f32 + 0.5) / cell, (y as f32 + 0.5) / cell);
            let near = |c: &(f32, f32)| (p.0 - c.0).hypot(p.1 - c.1) < SOCKET_RADIUS;
            let on_link = |(a, b): (&(f32, f32), &(f32, f32))| {
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy) < LINK_HALF_WIDTH
            };
            centres.iter().any(near) || centres.iter().zip(centres.iter().skip(1)).any(on_link)
        };
        let premultiplied: Vec<[f32; 4]> =
            self.rgba.iter().map(|p| [p[0] * p[3] / 255.0, p[1] * p[3] / 255.0, p[2] * p[3] / 255.0, p[3]]).collect();
        let small = resize(&premultiplied, self.width, self.height, w, h);
        let mut pixels = Vec::new();
        for (i, p) in small.iter().enumerate() {
            if p[3] >= 240.0 && !covered(i % w, i / w) {
                let a = p[3] / 255.0;
                pixels.push(((i % w) as i32, (i / w) as i32, [p[0] / a, p[1] / a, p[2] / a]));
            }
        }
        Template::new(pixels)
    }
}

/// An art at one size: its opaque pixels as offsets, centred on their mean.
struct Template {
    offsets: Vec<(i32, i32)>,
    values: Vec<[f32; 3]>,
    norm: f32,
}

impl Template {
    fn new(pixels: Vec<(i32, i32, [f32; 3])>) -> Template {
        let n = (pixels.len() * 3).max(1) as f32;
        let mean = pixels.iter().map(|p| p.2[0] + p.2[1] + p.2[2]).sum::<f32>() / n;
        let values: Vec<[f32; 3]> = pixels.iter().map(|p| [p.2[0] - mean, p.2[1] - mean, p.2[2] - mean]).collect();
        let norm = values.iter().map(|v| v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sum::<f32>().sqrt();
        Template { offsets: pixels.iter().map(|p| (p.0, p.1)).collect(), values, norm }
    }

    /// Correlation with the image at (x, y), -1 to 1. Channels share one
    /// mean, so a different hue counts, not just a different pattern.
    fn score(&self, img: &Image, x: i32, y: i32) -> f32 {
        if self.offsets.len() < 16 || self.norm <= 0.0 {
            return -1.0;
        }
        let (mut sum, mut sq, mut dot) = (0.0f32, 0.0f32, 0.0f32);
        for (&(dx, dy), t) in self.offsets.iter().zip(&self.values) {
            let p = img.px[(y + dy) as usize * img.width + (x + dx) as usize];
            sum += p[0] + p[1] + p[2];
            sq += p[0] * p[0] + p[1] * p[1] + p[2] * p[2];
            dot += p[0] * t[0] + p[1] * t[1] + p[2] * t[2];
        }
        let n = (self.offsets.len() * 3) as f32;
        let var = sq - sum * sum / n;
        if var <= 1e-3 {
            return -1.0;
        }
        dot / (var.sqrt() * self.norm)
    }

    /// Like `score`, over the `KEEP` share of pixels that fit best (after
    /// matching the image's brightness and contrast to the art's).
    fn score_trimmed(&self, img: &Image, x: i32, y: i32) -> f32 {
        let n = self.offsets.len();
        if n < 16 || self.norm <= 0.0 {
            return -1.0;
        }
        let px: Vec<[f32; 3]> =
            self.offsets.iter().map(|&(dx, dy)| img.px[(y + dy) as usize * img.width + (x + dx) as usize]).collect();
        let mean = px.iter().map(|p| p[0] + p[1] + p[2]).sum::<f32>() / (n * 3) as f32;
        let dot: f32 = px.iter().zip(&self.values).map(|(p, t)| p[0] * t[0] + p[1] * t[1] + p[2] * t[2]).sum();
        let gain = dot / (self.norm * self.norm);
        if gain <= 0.0 {
            return self.score(img, x, y);
        }
        let misfit = |p: &[f32; 3], t: &[f32; 3]| (0..3).map(|c| (p[c] - mean - gain * t[c]).powi(2)).sum::<f32>();
        let mut misfits: Vec<f32> = px.iter().zip(&self.values).map(|(p, t)| misfit(p, t)).collect();
        let kept = ((n as f32 * KEEP) as usize).clamp(16, n);
        let limit = *misfits.select_nth_unstable_by(kept - 1, f32::total_cmp).1;

        let (mut sp, mut st, mut spp, mut stt, mut spt, mut m) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for (p, t) in px.iter().zip(&self.values) {
            if misfit(p, t) > limit {
                continue;
            }
            for c in 0..3 {
                sp += p[c];
                st += t[c];
                spp += p[c] * p[c];
                stt += t[c] * t[c];
                spt += p[c] * t[c];
            }
            m += 3.0;
        }
        let (vp, vt) = (spp - sp * sp / m, stt - st * st / m);
        if vp <= 1e-3 || vt <= 1e-3 {
            return -1.0;
        }
        (spt - sp * st / m) / (vp * vt).sqrt()
    }
}

struct Image {
    width: usize,
    height: usize,
    px: Vec<[f32; 3]>,
}

impl Image {
    fn scaled(&self, factor: f32) -> Image {
        if (factor - 1.0).abs() < 1e-3 {
            return Image { width: self.width, height: self.height, px: self.px.clone() };
        }
        let width = ((self.width as f32 * factor).round() as usize).max(1);
        let height = ((self.height as f32 * factor).round() as usize).max(1);
        Image { width, height, px: resize(&self.px, self.width, self.height, width, height) }
    }
}

fn rgb_pixels(rgb: &[u8]) -> Vec<[f32; 3]> {
    rgb.as_chunks::<3>().0.iter().map(|p| p.map(f32::from)).collect()
}

/// Where the game draws sockets over a hovered item, as centres in cells
/// from its top left, in socket order (links join neighbours): down a
/// one-cell-wide item, or two to a row snaking left-right, right-left on a
/// wider one; centred vertically when they don't fill it.
fn socket_centres(cells: (u32, u32), sockets: u32) -> Vec<(f32, f32)> {
    let (w, h) = cells;
    let n = sockets.min(w.min(2) * h);
    let per_row = w.min(2);
    let rows = n.div_ceil(per_row.max(1));
    let top = (h - rows) as f32 / 2.0;
    let left = (w - per_row) as f32 / 2.0;
    (0..n)
        .map(|k| {
            let row = k / per_row;
            let col = if row % 2 == 0 { k % per_row } else { per_row - 1 - k % per_row };
            (left + col as f32 + 0.5, top + row as f32 + 0.5)
        })
        .collect()
}

/// Box-filter resize: each output pixel averages the source pixels under it
/// (weighted by overlap). Done one axis at a time.
fn resize<const C: usize>(src: &[[f32; C]], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<[f32; C]> {
    let weights = |from: usize, to: usize| -> Vec<Vec<(usize, f32)>> {
        let step = from as f32 / to as f32;
        (0..to)
            .map(|o| {
                let (start, end) = (o as f32 * step, (o + 1) as f32 * step);
                let mut taps = Vec::new();
                let mut i = start.floor() as usize;
                while (i as f32) < end && i < from {
                    let w = end.min(i as f32 + 1.0) - start.max(i as f32);
                    if w > 0.0 {
                        taps.push((i, w));
                    }
                    i += 1;
                }
                let total: f32 = taps.iter().map(|t| t.1).sum();
                taps.iter().map(|&(i, w)| (i, w / total)).collect()
            })
            .collect()
    };
    let (wx, wy) = (weights(sw, dw), weights(sh, dh));
    let mut rows = vec![[0.0; C]; dw * sh];
    for y in 0..sh {
        for (x, taps) in wx.iter().enumerate() {
            let out = &mut rows[y * dw + x];
            for &(i, w) in taps {
                for c in 0..C {
                    out[c] += src[y * sw + i][c] * w;
                }
            }
        }
    }
    let mut out = vec![[0.0; C]; dw * dh];
    for (y, taps) in wy.iter().enumerate() {
        for x in 0..dw {
            let o = &mut out[y * dw + x];
            for &(i, w) in taps {
                for c in 0..C {
                    o[c] += rows[i * dw + x][c] * w;
                }
            }
        }
    }
    out
}

/// The sizes to try, as fractions of a cell's pitch.
fn scales() -> Vec<f32> {
    let mut out = Vec::new();
    for (lo, hi) in [SCALES, QUAD_SCALES] {
        let mut s = lo;
        while s <= hi * 1.001 {
            out.push(s);
            s *= SCALE_STEP;
        }
    }
    out
}

/// Positions along one axis where an item `size` long covers `cursor` and
/// stays inside `0..limit`.
fn span(cursor: f32, size: usize, limit: usize) -> std::ops::RangeInclusive<i32> {
    let c = cursor.floor() as i32;
    let lo = (c - size as i32 + 1).max(0);
    let hi = c.min(limit as i32 - size as i32);
    lo..=hi
}

/// How well `art` matches the item under the cursor: the best score over
/// the sizes and places it could be at. `sockets`: how many the item has
/// (the game draws them over it while it's hovered).
pub fn score(capture: &Capture, art: &Art, sockets: u32) -> f32 {
    let full = Image { width: capture.width, height: capture.height, px: rgb_pixels(&capture.rgb) };
    if full.px.len() != capture.width * capture.height || capture.cell <= 0.0 {
        return -1.0;
    }
    let (cw, ch) = (art.cells.0 as f32, art.cells.1 as f32);
    // Pixel size of the art at `scale` (of a cell) in an image scaled by `factor`.
    let size = |scale: f32, factor: f32| {
        let px = capture.cell * scale * factor;
        ((cw * px).round() as usize, (ch * px).round() as usize)
    };

    // Coarse: every size and place, small.
    let factor = (COARSE_SIDE / (cw.max(ch) * capture.cell)).max(COARSE_MIN_CELL / capture.cell).min(1.0);
    let coarse = full.scaled(factor);
    let cursor = (capture.cursor.0 * factor, capture.cursor.1 * factor);
    // The best place at each size: (score, scale, x, y in full pixels).
    let mut best: Vec<(f32, f32, f32, f32)> = Vec::new();
    for scale in scales() {
        let (w, h) = size(scale, factor);
        if w < 4 || h < 4 {
            continue;
        }
        let t = art.template(w, h, sockets);
        let mut top = (-1.0f32, scale, 0.0f32, 0.0f32);
        for y in span(cursor.1, h, coarse.height) {
            for x in span(cursor.0, w, coarse.width) {
                let s = t.score(&coarse, x, y);
                if s > top.0 {
                    top = (s, scale, x as f32 / factor, y as f32 / factor);
                }
            }
        }
        if top.0 > -1.0 {
            best.push(top);
        }
    }
    // Something covering the art can put the right place a little behind.
    best.sort_by(|a, b| b.0.total_cmp(&a.0));
    best.truncate(REFINE);

    // Fine: around those, at up to the art's own size, minding only the
    // pixels that fit.
    let factor_fine = (FINE_CELL / capture.cell).min(1.0);
    let fine = full.scaled(factor_fine);
    let cursor = (capture.cursor.0 * factor_fine, capture.cursor.1 * factor_fine);
    let reach = (factor_fine / factor).ceil() as i32 + 1;
    let mut score = -1.0f32;
    for &(_, scale, bx, by) in &best {
        for s in [scale / SCALE_STEP, scale, scale * SCALE_STEP] {
            let (w, h) = size(s, factor_fine);
            if w < 4 || h < 4 {
                continue;
            }
            let t = art.template(w, h, sockets);
            let (cx, cy) = ((bx * factor_fine).round() as i32, (by * factor_fine).round() as i32);
            let (xs, ys) = (span(cursor.0, w, fine.width), span(cursor.1, h, fine.height));
            for y in (cy - reach).max(*ys.start())..=(cy + reach).min(*ys.end()) {
                for x in (cx - reach).max(*xs.start())..=(cx + reach).min(*xs.end()) {
                    score = score.max(t.score_trimmed(&fine, x, y));
                }
            }
        }
    }
    score
}

/// Which of these scores (one per distinct art) is a match, if any: a
/// good score, well ahead of the rest. None if any art couldn't be scored
/// (-1: too little of it left to compare), since it can't be ruled out.
pub fn decide(scores: &[f32]) -> Option<usize> {
    if scores.iter().any(|&s| s <= -1.0) {
        return None;
    }
    let (best, &top) = scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
    let runner_up = scores.iter().enumerate().filter(|&(i, _)| i != best).map(|(_, &s)| s).fold(-1.0f32, f32::max);
    (top >= MIN_SCORE && top - runner_up >= MIN_LEAD).then_some(best)
}

/// The size in cells poe.ninja's art URLs carry: they look like
/// `.../gen/image/<base64 JSON: [25,14,{"f":...,"w":1,"h":1,...}]>/<hash>/<file>.png`.
pub fn cells_from_url(url: &str) -> Option<(u32, u32)> {
    let encoded = url.split("/gen/image/").nth(1)?.split('/').next()?;
    let json: serde_json::Value = serde_json::from_slice(&base64_decode(encoded)?).ok()?;
    let spec = json.as_array()?.iter().find(|v| v.is_object())?;
    let cells = |k: &str| spec[k].as_u64().filter(|n| (1..=4).contains(n)).map(|n| n as u32);
    Some((cells("w")?, cells("h")?))
}

/// Standard or URL-safe base64, padding optional.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/art");

    fn art(name: &str) -> Art {
        Art::from_png(&std::fs::read(format!("{FIXTURES}/{name}.png")).unwrap(), None).unwrap()
    }

    /// A 1440p inventory (cells 70 px apart) with an unidentified Coral Ring
    /// (Sibyl's Lament) hovered, under the game's tooltip. The crop's
    /// bottom edge is the screenshot's.
    fn inventory(cursor: (f32, f32)) -> Capture {
        let bytes = std::fs::read(format!("{FIXTURES}/coral-ring-inventory.png")).unwrap();
        let art = Art::from_png(&bytes, Some((1, 1))).unwrap();
        let rgb = art.rgba.iter().flat_map(|p| [p[0] as u8, p[1] as u8, p[2] as u8]).collect();
        Capture { width: art.width, height: art.height, rgb, cursor, cell: 70.2 }
    }

    // Sibyl's Lament, Kaom's Sign, Winterweave.
    const CORAL_RINGS: [&str; 3] = ["SibylsLament", "Ring2Unique", "BloodBoil"];

    #[test]
    fn picks_the_hovered_unique() {
        for cursor in [(106.0, 90.0), (80.0, 70.0), (130.0, 115.0)] {
            let capture = inventory(cursor);
            let scores: Vec<f32> = CORAL_RINGS.iter().map(|n| score(&capture, &art(n), 0)).collect();
            assert_eq!(decide(&scores), Some(0), "cursor {cursor:?}: {scores:?}");
        }
    }

    #[test]
    fn no_match_on_another_item() {
        // The gem in the next cell: none of the rings.
        let capture = inventory((170.0, 90.0));
        let scores: Vec<f32> = CORAL_RINGS.iter().map(|n| score(&capture, &art(n), 0)).collect();
        assert_eq!(decide(&scores), None, "{scores:?}");
    }

    #[test]
    fn quad_tab_cells() {
        // The same inventory at half size: a quad stash tab's cells.
        let full = inventory((0.0, 0.0));
        let small = Image { width: full.width, height: full.height, px: rgb_pixels(&full.rgb) }.scaled(0.5);
        let rgb = small.px.iter().flat_map(|p| p.map(|c| c.round() as u8)).collect();
        let capture = Capture { width: small.width, height: small.height, rgb, cursor: (53.0, 45.0), cell: 70.2 };
        let scores: Vec<f32> = CORAL_RINGS.iter().map(|n| score(&capture, &art(n), 0)).collect();
        assert_eq!(decide(&scores), Some(0), "{scores:?}");
    }

    /// Paints over the hovered ring (73..139, 57..123 in the crop) what can
    /// cover a hovered item: a socket where the game would draw a 1x1
    /// item's (rings have none, but other items do), and an arrow cursor
    /// in case the game draws its own.
    fn covered(mut capture: Capture) -> Capture {
        let mut paint = |x: usize, y: usize, rgb: [u8; 3]| {
            let i = (y * capture.width + x) * 3;
            capture.rgb[i..i + 3].copy_from_slice(&rgb);
        };
        for y in 60..120 {
            for x in 76..136 {
                let d = (x as f32 - 106.0).hypot(y as f32 - 90.0);
                if d < 10.0 {
                    paint(x, y, [200, 40, 30]); // the gem
                } else if d < 19.0 {
                    paint(x, y, [70, 60, 50]); // its rim
                }
            }
        }
        for y in 0..22usize {
            for x in 0..=(y * 2 / 3) {
                paint(112 + x, 96 + y, if x == 0 || x == y * 2 / 3 { [0, 0, 0] } else { [235, 235, 235] });
            }
        }
        capture
    }

    #[test]
    fn socket_and_cursor() {
        let capture = covered(inventory((106.0, 90.0)));
        let scores: Vec<f32> = CORAL_RINGS.iter().map(|n| score(&capture, &art(n), 1)).collect();
        assert_eq!(decide(&scores), Some(0), "{scores:?}");
    }

    /// The game at 1440p: an unidentified Soul Mantle hovered, four sockets
    /// drawn over it (as on any hovered item), the cursor at
    /// its bottom right. Its Foulborn version is the same art with a purple
    /// overlay; The Covenant's two are the other arts on the base.
    #[test]
    fn socketed_body_armour() {
        let bytes = std::fs::read(format!("{FIXTURES}/spidersilk-robe-sockets.png")).unwrap();
        let png = Art::from_png(&bytes, Some((1, 1))).unwrap();
        let rgb = png.rgba.iter().flat_map(|p| [p[0] as u8, p[1] as u8, p[2] as u8]).collect();
        let capture = Capture { width: png.width, height: png.height, rgb, cursor: (156.0, 206.0), cell: 70.23 };
        let robes = ["SoulMantle", "FoulbornSoulMantle", "TheCovenant", "FoulbornTheCovenant"];
        let scores: Vec<f32> = robes.iter().map(|n| score(&capture, &art(n), 4)).collect();
        assert_eq!(decide(&scores), Some(0), "{scores:?}");
    }

    /// The game at 1440p, a vendor's gamble window: an unidentified Foul
    /// Staff (The Broken Elegy) with two sockets across its middle, the
    /// cursor on its upper half; the item's tooltip above. The Winds of
    /// Fate is the other art on the base, a thin staff.
    #[test]
    fn socketed_staff_at_a_vendor() {
        let bytes = std::fs::read(format!("{FIXTURES}/foul-staff-vendor.png")).unwrap();
        let png = Art::from_png(&bytes, Some((1, 1))).unwrap();
        let rgb = png.rgba.iter().flat_map(|p| [p[0] as u8, p[1] as u8, p[2] as u8]).collect();
        let capture = Capture { width: png.width, height: png.height, rgb, cursor: (76.2, 116.5), cell: 70.23 };
        let scores: Vec<f32> = ["BrokenElegy", "AnwensStaff"].iter().map(|n| score(&capture, &art(n), 2)).collect();
        assert_eq!(decide(&scores), Some(0), "{scores:?}");
    }

    #[test]
    fn socket_layout() {
        // A body armour's six: snaking down two columns.
        let armour = [(0.5, 0.5), (1.5, 0.5), (1.5, 1.5), (0.5, 1.5), (0.5, 2.5), (1.5, 2.5)];
        assert_eq!(socket_centres((2, 3), 6), armour);
        // A two-hander's six, centred on its four rows.
        assert_eq!(socket_centres((2, 4), 6)[0], (0.5, 1.0));
        // Two across its middle (as on the fixture's Foul Staff).
        assert_eq!(socket_centres((2, 4), 2), [(0.5, 2.0), (1.5, 2.0)]);
        // A wand's three, down its one column; a ring's none.
        assert_eq!(socket_centres((1, 3), 3), [(0.5, 0.5), (0.5, 1.5), (0.5, 2.5)]);
        assert_eq!(socket_centres((1, 1), 0), []);
        // Helmet with three: two rows, centred, the second from the right.
        assert_eq!(socket_centres((2, 2), 3), [(0.5, 0.5), (1.5, 0.5), (1.5, 1.5)]);
    }

    #[test]
    fn decide_needs_a_clear_lead() {
        assert_eq!(decide(&[0.9, 0.4, 0.3]), Some(0));
        assert_eq!(decide(&[0.5, 0.2]), None); // too weak
        assert_eq!(decide(&[0.85, 0.78]), None); // too close
        assert_eq!(decide(&[0.2, 0.9]), Some(1));
        assert_eq!(decide(&[0.96, -1.0]), None); // one art couldn't be compared
    }

    #[test]
    fn cells_in_art_urls() {
        let ring = "https://web.poecdn.com/gen/image/WzI1LDE0LHsiZiI6IjJESXRlbXMvUmluZ3MvU2lieWxzTGFtZW50IiwidyI6MSwiaCI6MSwic2NhbGUiOjF9XQ/73ccf7d7c4/SibylsLament.png";
        assert_eq!(cells_from_url(ring), Some((1, 1)));
        assert_eq!(cells_from_url("https://web.poecdn.com/image/Art/2DItems/Rings/x.png"), None);
    }

    #[test]
    fn base64_both_alphabets() {
        assert_eq!(base64_decode("aGk/Pz8").unwrap(), b"hi???");
        assert_eq!(base64_decode("aGk_Pz8=").unwrap(), b"hi???");
        assert_eq!(base64_decode("a b"), None);
    }
}
