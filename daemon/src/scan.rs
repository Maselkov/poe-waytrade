//! Item displays read off the screen: vault reliquaries and the like show
//! items in world-space tooltips that can't be copied, so the layer sends
//! us its frames to find them in (SCANFIND) and their pixels to read and
//! price (SCANITEM).
//!
//! Finding: a display's tooltip has a title plate ("Forbidden Reliquary")
//! between two stone pillars, opaque game art that looks the same on any
//! background. We match the left pillar and its mirror image (the right
//! one, give or take its lighting) in a grey, scaled-down frame, and pair
//! them up row by row.
//!
//! Reading: the tooltip's lines are centred under the plate, so of what OCR
//! finds in the crop we keep the centred lines below the plate, read the
//! item's name off the first of them (matched against every name the trade
//! site knows, since OCR gets a letter wrong now and then), and its
//! properties off the rest.

use std::sync::{Arc, Mutex, OnceLock};

use crate::item::{Item, ModKind};
use crate::ocr::Line;
use crate::trade::NamedItem;

/// The left pillar, as the game draws it at 1440 pixels high (grey).
const PILLAR_PNG: &[u8] = include_bytes!("../assets/pillar-left.png");
/// The UI is scaled to the frame's height: the pillar's size is for this.
const PILLAR_HEIGHT_AT: f32 = 1440.0;
/// Scores (normalized cross-correlation, -1 to 1) a pillar needs: the left
/// one is the template itself; the right one is its mirror image, lit a
/// little differently.
const LEFT_SCORE: f32 = 0.75;
const RIGHT_SCORE: f32 = 0.6;
/// The coarse pass's (half resolution) bar: candidates for the fine pass.
const COARSE_SCORE: f32 = 0.55;
/// A plate is this many pillar widths wide, give or take (between the
/// pillars' left edges).
const PLATE_WIDTHS: (f32, f32) = (4.0, 30.0);
/// The crop around a plate that holds its tooltip, in plate widths and
/// pillar heights (a tooltip is wider than its plate, and as tall as its
/// lines need).
const CROP_WIDTH: f32 = 2.3;
const CROP_HEIGHT: f32 = 11.0;

/// An 8-bit grey image.
pub struct Gray {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Gray {
    fn at(&self, x: usize, y: usize) -> f32 {
        self.pixels[y * self.width + x] as f32
    }

    /// Box-filtered down to `width` x `height`.
    fn resized(&self, width: usize, height: usize) -> Gray {
        let mut pixels = vec![0u8; width * height];
        let (sx, sy) = (self.width as f32 / width as f32, self.height as f32 / height as f32);
        for y in 0..height {
            let (y0, y1) =
                ((y as f32 * sy) as usize, (((y + 1) as f32 * sy) as usize).max((y as f32 * sy) as usize + 1));
            for x in 0..width {
                let (x0, x1) =
                    ((x as f32 * sx) as usize, (((x + 1) as f32 * sx) as usize).max((x as f32 * sx) as usize + 1));
                let (mut sum, mut n) = (0u32, 0u32);
                for yy in y0..y1.min(self.height) {
                    for xx in x0..x1.min(self.width) {
                        sum += self.pixels[yy * self.width + xx] as u32;
                        n += 1;
                    }
                }
                pixels[y * width + x] = (sum / n.max(1)) as u8;
            }
        }
        Gray { width, height, pixels }
    }

    fn mirrored(&self) -> Gray {
        let mut pixels = self.pixels.clone();
        for row in pixels.chunks_mut(self.width) {
            row.reverse();
        }
        Gray { width: self.width, height: self.height, pixels }
    }
}

fn decode_gray_png(bytes: &[u8]) -> Gray {
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("pillar png");
    let mut buf = vec![0; reader.output_buffer_size().expect("pillar png size")];
    let info = reader.next_frame(&mut buf).expect("pillar png");
    assert_eq!(info.color_type, png::ColorType::Grayscale, "pillar png is grey");
    let (width, height) = (info.width as usize, info.height as usize);
    buf.truncate(width * height);
    Gray { width, height, pixels: buf }
}

fn pillar() -> &'static Gray {
    static PILLAR: OnceLock<Gray> = OnceLock::new();
    PILLAR.get_or_init(|| decode_gray_png(PILLAR_PNG))
}

/// A template, zero-mean, with its norm.
struct Template {
    width: usize,
    height: usize,
    values: Vec<f32>,
    norm: f32,
}

impl Template {
    fn new(g: &Gray) -> Template {
        let n = (g.width * g.height) as f32;
        let mean = g.pixels.iter().map(|&p| p as f32).sum::<f32>() / n;
        let values: Vec<f32> = g.pixels.iter().map(|&p| p as f32 - mean).collect();
        let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        Template { width: g.width, height: g.height, values, norm }
    }
}

/// Sums and sums of squares, for any rectangle's mean and variance.
struct Integral {
    width: usize,
    sum: Vec<f64>,
    sq: Vec<f64>,
}

impl Integral {
    fn new(g: &Gray) -> Integral {
        let w = g.width + 1;
        let mut sum = vec![0.0; w * (g.height + 1)];
        let mut sq = vec![0.0; w * (g.height + 1)];
        for y in 0..g.height {
            let (mut row, mut row_sq) = (0.0, 0.0);
            for x in 0..g.width {
                let v = g.at(x, y) as f64;
                row += v;
                row_sq += v * v;
                sum[(y + 1) * w + x + 1] = sum[y * w + x + 1] + row;
                sq[(y + 1) * w + x + 1] = sq[y * w + x + 1] + row_sq;
            }
        }
        Integral { width: w, sum, sq }
    }

    /// (sum, sum of squares) over w x h at (x, y).
    fn rect(&self, x: usize, y: usize, w: usize, h: usize) -> (f64, f64) {
        let at = |t: &Vec<f64>, xx: usize, yy: usize| t[yy * self.width + xx];
        let get = |t: &Vec<f64>| at(t, x + w, y + h) - at(t, x, y + h) - at(t, x + w, y) + at(t, x, y);
        (get(&self.sum), get(&self.sq))
    }
}

/// Normalized cross-correlation of `t` with the image at (x, y).
fn score(g: &Gray, ii: &Integral, t: &Template, x: usize, y: usize) -> f32 {
    let n = (t.width * t.height) as f64;
    let (sum, sq) = ii.rect(x, y, t.width, t.height);
    let var = (sq - sum * sum / n).max(0.0);
    if var < 1.0 || t.norm < 1e-3 {
        return 0.0;
    }
    let mut dot = 0.0f32;
    for ty in 0..t.height {
        let row = &g.pixels[(y + ty) * g.width + x..][..t.width];
        let trow = &t.values[ty * t.width..][..t.width];
        for (p, v) in row.iter().zip(trow) {
            dot += *p as f32 * v;
        }
    }
    dot / (var.sqrt() as f32 * t.norm)
}

/// A title plate, in the frame's full-resolution pixels: the pillars' box
/// (left pillar's left edge to right pillar's right edge), and the crop that
/// holds its tooltip.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plate {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub crop: (u32, u32, u32, u32),
}

/// Finds title plates in a grey frame that is `scale` times smaller than the
/// game's (2: half resolution), whose full height is `full_height`.
pub fn find_plates(frame: &Gray, full_width: u32, full_height: u32) -> Vec<Plate> {
    let scale = full_height as f32 / frame.height as f32;
    // The pillar's size in this frame.
    let size = full_height as f32 / PILLAR_HEIGHT_AT / scale;
    let (tw, th) = ((pillar().width as f32 * size).round() as usize, (pillar().height as f32 * size).round() as usize);
    if tw < 4 || th < 8 || tw >= frame.width || th >= frame.height {
        return Vec::new();
    }
    let left = Template::new(&pillar().resized(tw, th));
    let right = Template::new(&pillar().resized(tw, th).mirrored());
    let ii = Integral::new(frame);

    // Coarse: the left pillar at half this resolution, everywhere.
    let half = frame.resized(frame.width / 2, frame.height / 2);
    let (hw, hh) = ((tw / 2).max(2), (th / 2).max(4));
    let half_left = Template::new(&pillar().resized(hw, hh));
    let half_ii = Integral::new(&half);
    let mut candidates: Vec<(f32, usize, usize)> = Vec::new();
    for y in 0..=half.height - hh {
        for x in 0..=half.width - hw {
            let s = score(&half, &half_ii, &half_left, x, y);
            if s >= COARSE_SCORE {
                candidates.push((s, x * 2, y * 2));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));

    // Fine: each candidate's best spot nearby, best first, one per pillar.
    let mut lefts: Vec<(f32, usize, usize)> = Vec::new();
    for (_, cx, cy) in candidates {
        if lefts.iter().any(|&(_, x, y)| x.abs_diff(cx) < tw && y.abs_diff(cy) < th) {
            continue;
        }
        if let Some(best) = best_near(frame, &ii, &left, cx, cy, 2).filter(|b| b.0 >= LEFT_SCORE)
            && !lefts.iter().any(|&(_, x, y)| x.abs_diff(best.1) < tw && y.abs_diff(best.2) < th)
        {
            lefts.push(best);
        }
    }

    // The right pillar: in the same row, a plate's width away.
    let mut plates = Vec::new();
    for (_, lx, ly) in lefts {
        let from = lx + (PLATE_WIDTHS.0 * tw as f32) as usize;
        let to = (lx + (PLATE_WIDTHS.1 * tw as f32) as usize).min(frame.width - tw);
        let mut best: Option<(f32, usize, usize)> = None;
        for y in ly.saturating_sub(2)..=(ly + 2).min(frame.height - th) {
            for x in from..=to {
                let s = score(frame, &ii, &right, x, y);
                if s >= RIGHT_SCORE && best.is_none_or(|b| s > b.0) {
                    best = Some((s, x, y));
                }
            }
        }
        let Some((_, rx, _)) = best else { continue };
        let (x, y, w, h) = (lx as f32 * scale, ly as f32 * scale, (rx + tw - lx) as f32 * scale, th as f32 * scale);
        plates.push(Plate { x, y, w, h, crop: crop_for(x, y, w, h, full_width, full_height) });
    }
    plates
}

fn best_near(g: &Gray, ii: &Integral, t: &Template, cx: usize, cy: usize, r: usize) -> Option<(f32, usize, usize)> {
    let mut best: Option<(f32, usize, usize)> = None;
    for y in cy.saturating_sub(r)..=(cy + r).min(g.height - t.height) {
        for x in cx.saturating_sub(r)..=(cx + r).min(g.width - t.width) {
            let s = score(g, ii, t, x, y);
            if best.is_none_or(|b| s > b.0) {
                best = Some((s, x, y));
            }
        }
    }
    best
}

/// The tooltip's crop: centred under the plate, from its top down.
fn crop_for(x: f32, y: f32, w: f32, h: f32, full_width: u32, full_height: u32) -> (u32, u32, u32, u32) {
    let cx = x + w / 2.0;
    let cw = (w * CROP_WIDTH).min(full_width as f32);
    let x0 = (cx - cw / 2.0).clamp(0.0, full_width as f32 - cw);
    let y0 = y.max(0.0);
    let y1 = (y + h * CROP_HEIGHT).min(full_height as f32);
    (x0 as u32, y0 as u32, cw as u32, (y1 - y0).max(1.0) as u32)
}

// ---- reading a tooltip -------------------------------------------------------

/// What a tooltip was read as.
#[derive(Debug)]
pub struct ScreenItem {
    pub item: Item,
    /// Facetor's Lens.
    pub stored_experience: Option<u64>,
    /// The lines that made it, for the log.
    pub lines: Vec<String>,
}

/// Names normalised for matching: OCR reads small caps as capitals and loses
/// apostrophes and spaces now and then, so only letters and digits count.
fn key(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()).collect()
}

/// The trade site's names, ready to match against.
pub struct Vocabulary {
    names: Arc<Vec<NamedItem>>,
    /// (key of a unique's name, its index)
    uniques: Vec<(String, usize)>,
    /// (key of a base type / gem / currency, its index), one per name.
    bases: Vec<(String, usize)>,
}

impl Vocabulary {
    pub fn new(names: Arc<Vec<NamedItem>>) -> Vocabulary {
        let mut uniques = Vec::new();
        let mut bases: Vec<(String, usize)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (i, n) in names.iter().enumerate() {
            match &n.name {
                Some(name) => uniques.push((key(name), i)),
                None => {
                    if seen.insert(key(&n.base)) {
                        bases.push((key(&n.base), i));
                    }
                }
            }
        }
        Vocabulary { names, uniques, bases }
    }

    /// The one shared vocabulary for this list of names.
    pub fn shared(names: Arc<Vec<NamedItem>>) -> Arc<Vocabulary> {
        static CACHE: Mutex<Option<Arc<Vocabulary>>> = Mutex::new(None);
        let mut cache = CACHE.lock().unwrap();
        if let Some(v) = cache.as_ref().filter(|v| Arc::ptr_eq(&v.names, &names)) {
            return v.clone();
        }
        let v = Arc::new(Vocabulary::new(names));
        *cache = Some(v.clone());
        v
    }

    /// The entries closest to `text`, if close enough: within about a fifth
    /// of its letters, and not tied with a different name.
    fn closest(&self, list: &[(String, usize)], text: &str) -> Option<Vec<usize>> {
        let k = key(text);
        if k.len() < 3 {
            return None;
        }
        let limit = (k.len() / 5).max(1);
        let mut best = limit + 1;
        let mut found: Vec<usize> = Vec::new();
        for (name, i) in list {
            if name.len().abs_diff(k.len()) > best.min(limit) {
                continue;
            }
            if let Some(d) = distance(&k, name, best.min(limit)) {
                if d < best {
                    best = d;
                    found.clear();
                }
                if d == best {
                    found.push(*i);
                }
            }
        }
        // Ties between different names: can't tell which it is.
        let first = found.first().map(|&i| key(&self.label(i)))?;
        found.iter().all(|&i| key(&self.label(i)) == first).then_some(found)
    }

    fn label(&self, i: usize) -> String {
        let n = &self.names[i];
        n.name.clone().unwrap_or_else(|| n.base.clone())
    }
}

/// Levenshtein distance, or None past `limit`.
fn distance(a: &str, b: &str, limit: usize) -> Option<usize> {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        if row_min > limit {
            return None;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    Some(prev[b.len()]).filter(|&d| d <= limit)
}

/// The tooltip's own lines: centred under the plate (x = `center`), from the
/// plate's bottom (`top`) down, until a gap no tooltip has.
pub fn tooltip_lines(lines: &[Line], center: f32, top: f32, crop_width: f32) -> Vec<Line> {
    let tolerance = (crop_width * 0.04).max(12.0);
    let mut aligned: Vec<Line> =
        lines.iter().filter(|l| (l.center_x() - center).abs() <= tolerance && l.y + l.h / 2.0 > top).cloned().collect();
    aligned.sort_by(|a, b| a.y.total_cmp(&b.y));
    let mut heights: Vec<f32> = aligned.iter().map(|l| l.h).collect();
    heights.sort_by(f32::total_cmp);
    let Some(&line_height) = heights.get(heights.len() / 2) else { return aligned };
    let mut out: Vec<Line> = Vec::new();
    for l in aligned {
        if let Some(last) = out.last()
            && l.y - (last.y + last.h) > line_height * 3.0
        {
            break;
        }
        out.push(l);
    }
    out
}

/// Rarity from a name's colour (the game's: unique orange, rare yellow,
/// magic blue, gem teal, currency beige, normal white).
fn rarity_from_color(c: [u8; 3]) -> &'static str {
    let [r, g, b] = c.map(|v| v as i32);
    if r > g + 40 && r > b + 60 {
        "Unique"
    } else if r > 170 && g > 170 && b < r - 50 {
        "Rare"
    } else if b > r + 40 && b > g + 20 {
        "Magic"
    } else if g > r + 40 && b > r + 30 {
        "Gem"
    } else {
        "Normal"
    }
}

/// Reads the item off a tooltip's lines (see `tooltip_lines`). `canonical`
/// turns a mod line read in small caps into the game's text and kind.
pub fn read_item(
    lines: &[Line],
    vocabulary: &Vocabulary,
    mut canonical: impl FnMut(&str) -> Option<(ModKind, String)>,
) -> Result<ScreenItem, String> {
    let texts: Vec<String> = lines.iter().map(|l| l.text.clone()).collect();
    let mut item = Item::default();
    let mut body_from = None;
    // The name is in the first few lines (some displays put a line above).
    for i in 0..lines.len().min(3) {
        let first = &lines[i];
        let second = lines.get(i + 1);
        // A unique: its name, then its base.
        if let (Some(found), Some(second)) = (vocabulary.closest(&vocabulary.uniques, &first.text), second) {
            let base = vocabulary.closest(&vocabulary.bases, &second.text);
            let pair = found.iter().map(|&u| &vocabulary.names[u]).find(|u| {
                base.as_ref().is_some_and(|b| b.iter().any(|&b| key(&vocabulary.names[b].base) == key(&u.base)))
            });
            if let Some(u) = pair {
                item.rarity = "Unique".into();
                item.name = u.name.clone();
                item.base = u.base.clone();
                body_from = Some(i + 2);
                break;
            }
        }
        // A gem, currency, card or base type on its own.
        if let Some(found) = vocabulary.closest(&vocabulary.bases, &first.text) {
            let n = &vocabulary.names[found[0]];
            item.base = n.base.clone();
            item.rarity = match n.group.as_str() {
                "gem" => "Gem".into(),
                "currency" | "card" | "map" => "Currency".into(),
                // A base type: white, blue or yellow. Any other colour (a
                // unique's, a gem's) is a misread: still a base.
                _ => match rarity_from_color(first.color) {
                    r @ ("Magic" | "Rare") => r.into(),
                    _ => "Normal".into(),
                },
            };
            body_from = Some(i + 1);
            break;
        }
        // A rare (a Thief's Trinket): a made-up name, then the base.
        if let Some(found) = second.and_then(|s| vocabulary.closest(&vocabulary.bases, &s.text)) {
            let n = &vocabulary.names[found[0]];
            if !matches!(n.group.as_str(), "gem" | "currency" | "card" | "map") {
                item.rarity = "Rare".into();
                item.name = Some(title_case(&first.text));
                item.base = n.base.clone();
                body_from = Some(i + 2);
                break;
            }
        }
    }
    let Some(body_from) = body_from else {
        return Err(format!("No item name in: {}", texts.join(" | ")));
    };

    let mut stored_experience = None;
    for line in &lines[body_from..] {
        let up = line.text.to_ascii_uppercase();
        let k = key(&up);
        if k == "CORRUPTED" {
            item.corrupted = true;
        } else if k == "MIRRORED" {
            item.mirrored = true;
        } else if k == "UNIDENTIFIED" {
            item.unidentified = true;
        } else if let Some(v) = value_after(&up, "STORED EXPERIENCE") {
            stored_experience = number(v).map(|n| n as u64);
        } else if let Some(v) = value_after(&up, "STACK SIZE") {
            item.stack_size = number(v.split('/').next().unwrap_or(v)).map(|n| n as u32);
        } else if let Some(v) = value_after(&up, "ITEM LEVEL") {
            item.item_level = number(v).map(|n| n as u32);
        } else if let Some(v) = value_after(&up, "QUALITY") {
            item.quality = number(v).map_or(0, |n| n as u32);
        } else if item.rarity == "Gem"
            && let Some(v) = value_after(&up, "LEVEL")
        {
            item.gem_max_level = up.contains("MAX");
            item.gem_level = number(v).map(|n| n as u32);
        } else if item.rarity == "Rare"
            && let Some((kind, text)) = canonical(&line.text)
        {
            let mut m = crate::item::mod_line(&text);
            m.kind = kind;
            item.mods.push(m);
        }
    }
    Ok(ScreenItem { item, stored_experience, lines: texts })
}

/// "LEVEL: 4 (MAX)" after "LEVEL" -> "4 (MAX)". A colon OCR dropped is fine;
/// another word after the label isn't ("LEVEL" doesn't start "REQUIRES
/// LEVEL 40", nor "LEVELLED ...").
fn value_after<'a>(line: &'a str, label: &str) -> Option<&'a str> {
    let rest = line.trim().strip_prefix(label)?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').unwrap_or(rest).trim_start();
    rest.starts_with(|c: char| c.is_ascii_digit() || matches!(c, '+' | 'O' | 'I' | 'L')).then_some(rest)
}

/// The leading number of a property value, read loosely: thousands
/// separators either way ("750.000,000"), letters OCR mistakes for digits.
fn number(v: &str) -> Option<f64> {
    let mut digits = String::new();
    for c in v.trim_start_matches('+').chars() {
        match c {
            '0'..='9' => digits.push(c),
            'O' | 'o' => digits.push('0'),
            'I' | 'l' | 'L' | '|' => digits.push('1'),
            ',' | '.' => {}
            _ => break,
        }
    }
    digits.parse().ok()
}

/// "DOOM LOOP" -> "Doom Loop", for a rare's name we can't look up.
fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c.flat_map(char::to_lowercase)).collect()).unwrap_or_default()
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::tests::{load_png, models_available};

    fn named(name: Option<&str>, base: &str, group: &str) -> NamedItem {
        NamedItem { name: name.map(str::to_string), base: base.to_string(), group: group.to_string() }
    }

    fn vocabulary() -> Vocabulary {
        Vocabulary::new(Arc::new(vec![
            named(Some("Mother's Embrace"), "Heavy Belt", "accessory"),
            named(Some("Replica Dreamfeather"), "Eternal Sword", "weapon"),
            named(None, "Heavy Belt", "accessory"),
            named(None, "Eternal Sword", "weapon"),
            named(None, "Thief's Trinket", "accessory"),
            named(None, "Simplex Amulet", "accessory"),
            named(None, "Facetor's Lens", "currency"),
            named(None, "Chaos Orb", "currency"),
            named(None, "Greater Multistrike Support", "gem"),
            named(None, "Multistrike Support", "gem"),
        ]))
    }

    fn line(text: &str, y: f32) -> Line {
        Line {
            text: text.into(),
            x: 100.0 - text.len() as f32 * 4.0,
            y,
            w: text.len() as f32 * 8.0,
            h: 14.0,
            color: [200; 3],
        }
    }

    fn lines(texts: &[&str]) -> Vec<Line> {
        texts.iter().enumerate().map(|(i, t)| line(t, i as f32 * 20.0)).collect()
    }

    #[test]
    fn distances() {
        assert_eq!(distance("MOTHERSEMBRACE", "MOTHERSEMBRACE", 2), Some(0));
        assert_eq!(distance("MOTHERSMBRACE", "MOTHERSEMBRACE", 2), Some(1));
        assert_eq!(distance("CHAOSORB", "FACETORSLENS", 2), None);
    }

    #[test]
    fn unique_with_misread_name() {
        let v = vocabulary();
        let read = read_item(&lines(&["MOTHER S ?MBRACE", "HEAVY BELT", "REQUIRES LEVEL 40"]), &v, |_| None).unwrap();
        assert_eq!(read.item.rarity, "Unique");
        assert_eq!(read.item.name.as_deref(), Some("Mother's Embrace"));
        assert_eq!(read.item.base, "Heavy Belt");
    }

    #[test]
    fn line_above_the_name_is_skipped() {
        let v = vocabulary();
        let read =
            read_item(&lines(&["FORBIDDEN RELIQUARY", "FACETOR'S LENS", "STORED EXPERIENCE: 750.000,000"]), &v, |_| {
                None
            })
            .unwrap();
        assert_eq!(read.item.base, "Facetor's Lens");
        assert_eq!(read.item.rarity, "Currency");
        assert_eq!(read.stored_experience, Some(750_000_000));
    }

    #[test]
    fn gem_properties() {
        let v = vocabulary();
        let read = read_item(
            &lines(&[
                "GREATER MULTISTRIKE SUPPORT",
                "SUPPORT, ATTACK, MELEE",
                "LEVEL: 4 (MAX)",
                "QUALITY: +23%",
                "CORRUPTED",
            ]),
            &v,
            |_| None,
        )
        .unwrap();
        assert_eq!(read.item.rarity, "Gem");
        assert_eq!(read.item.base, "Greater Multistrike Support", "not the shorter gem");
        assert_eq!(read.item.gem_level, Some(4));
        assert!(read.item.gem_max_level);
        assert_eq!(read.item.quality, 23);
        assert!(read.item.corrupted);
    }

    #[test]
    fn currency_stack_and_base() {
        let v = vocabulary();
        let read = read_item(&lines(&["CHAOS ORB", "STACK SIZE: 12/20"]), &v, |_| None).unwrap();
        assert_eq!(read.item.stack_size, Some(12));
        let read = read_item(&lines(&["SIMPLEX AMULET", "ITEM LEVEL: 84", "REQUIRES LEVEL 64"]), &v, |_| None).unwrap();
        assert_eq!((read.item.rarity.as_str(), read.item.item_level), ("Normal", Some(84)));
    }

    #[test]
    fn rare_trinket_with_mods() {
        let v = vocabulary();
        let mut ls = lines(&["GLOOM BAUBLE", "THIEF'S TRINKET", "+12% TO COLD RESISTANCE", "SOMETHING ELSE"]);
        ls[0].color = [255, 255, 119];
        let read = read_item(&ls, &v, |t| {
            (t == "+12% TO COLD RESISTANCE").then(|| (ModKind::Explicit, "+12% to Cold Resistance".to_string()))
        })
        .unwrap();
        assert_eq!(read.item.rarity, "Rare");
        assert_eq!(read.item.name.as_deref(), Some("Gloom Bauble"));
        assert_eq!(read.item.base, "Thief's Trinket");
        assert_eq!(read.item.mods.len(), 1);
        assert_eq!(read.item.mods[0].text, "+12% to Cold Resistance");
    }

    #[test]
    fn nothing_known() {
        let v = vocabulary();
        assert!(read_item(&lines(&["ATTACK, PROJECTILE", "HITS ENEMIES"]), &v, |_| None).is_err());
    }

    #[test]
    fn values_after_labels() {
        assert_eq!(value_after("LEVEL: 4", "LEVEL"), Some("4"));
        assert_eq!(value_after("LEVEL 20 (MAX)", "LEVEL"), Some("20 (MAX)"));
        assert_eq!(value_after("REQUIRES LEVEL 40", "LEVEL"), None);
        assert_eq!(number("750.000,000"), Some(750_000_000.0));
        assert_eq!(number("+2O%"), Some(20.0));
    }

    #[test]
    fn colours() {
        assert_eq!(rarity_from_color([175, 96, 37]), "Unique");
        assert_eq!(rarity_from_color([255, 255, 119]), "Rare");
        assert_eq!(rarity_from_color([136, 136, 255]), "Magic");
        assert_eq!(rarity_from_color([27, 162, 155]), "Gem");
        assert_eq!(rarity_from_color([200, 200, 200]), "Normal");
    }

    #[test]
    fn centred_lines_below_the_plate() {
        let mut ls = lines(&["FORBIDDEN RELIQUARY", "MOTHER'S EMBRACE", "HEAVY BELT"]);
        // Off-centre junk from the background, and a line far below.
        ls.push(Line { text: "ABERRATION".into(), x: 400.0, y: 30.0, w: 80.0, h: 14.0, color: [200; 3] });
        ls.push(line("FAR AWAY", 400.0));
        let kept: Vec<String> = tooltip_lines(&ls, 100.0, 15.0, 300.0).into_iter().map(|l| l.text).collect();
        assert_eq!(kept, ["MOTHER'S EMBRACE", "HEAVY BELT"]);
    }

    /// The fixture frame: half of a 3440x1440 vault screenshot.
    #[test]
    fn finds_both_plates() {
        let (rgb, w, h) = load_png("vault-half.png");
        let gray = Gray { width: w, height: h, pixels: rgb.chunks(3).map(|p| p[0]).collect() };
        let started = std::time::Instant::now();
        let plates = find_plates(&gray, 3440, 1440);
        eprintln!("found {plates:?} in {}ms", started.elapsed().as_millis());
        let near = |x: f32, y: f32| plates.iter().any(|p| (p.x - x).abs() < 8.0 && (p.y - y).abs() < 8.0);
        assert!(near(654.0, 536.0), "Mother's Embrace: {plates:?}");
        assert!(near(1606.0, 0.0), "Facetor's Lens: {plates:?}");
        // The skill tooltip at the right edge has a plate too, but its right
        // pillar is off screen.
        assert_eq!(plates.len(), 2, "{plates:?}");
        let embrace = plates.iter().find(|p| p.y > 100.0).unwrap();
        assert!((embrace.w - 393.0).abs() < 10.0, "{embrace:?}");
        // Its crop holds the whole tooltip (x 520-1180, y 500-980).
        let (cx, cy, cw, ch) = embrace.crop;
        assert!(cx <= 530 && cx + cw >= 1170 && cy <= 540 && cy + ch >= 960, "{:?}", embrace.crop);
    }

    #[test]
    fn reads_the_fixture_tooltips() {
        if !models_available() {
            return;
        }
        let v = vocabulary();
        for (png, name, base) in [
            ("mothers-embrace.png", Some("Mother's Embrace"), "Heavy Belt"),
            ("facetors-lens.png", None, "Facetor's Lens"),
        ] {
            let (rgb, w, h) = load_png(png);
            let all = crate::ocr::read(&rgb, w, h).unwrap();
            // The fixtures are cropped around the plate's centre, with the
            // plate at the top.
            let plate_bottom = all.iter().find(|l| l.text.contains("RELIQUARY")).map_or(0.0, |l| l.y + l.h);
            let lines = tooltip_lines(&all, w as f32 / 2.0, plate_bottom, w as f32);
            let read = read_item(&lines, &v, |_| None).unwrap();
            assert_eq!(read.item.name.as_deref(), name, "{png}: {:?}", read.lines);
            assert_eq!(read.item.base, base, "{png}");
            if name.is_none() {
                assert_eq!(read.stored_experience, Some(750_000_000), "{:?}", read.lines);
            }
        }
    }
}
