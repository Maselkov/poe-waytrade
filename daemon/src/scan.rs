//! Item displays read off the screen: vault reliquaries and the like show
//! items in world-space tooltips that can't be copied, so the layer sends
//! us its frames to find them in (SCANFIND) and their pixels to read and
//! price (SCANITEM).
//!
//! Finding: a display's tooltip has a title plate, opaque game art that
//! looks the same on any background. A vault reliquary's ("Forbidden
//! Reliquary") sits between two stone pillars: we match the left pillar and
//! its mirror image (the right one, give or take its lighting) in a grey,
//! scaled-down frame, and pair them up row by row. A Heist curio display's
//! is a small wooden plate that always reads "Curio Display": matched whole.
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
/// The curio display plate (grey; the plate's inside, without its edges,
/// averaged over the two in a Grand Heist screenshot so it isn't one
/// plate's lighting), at its size in frames CURIO_HEIGHT_AT high: measured
/// in a 1440p game, where it's 1.35 times this. It's sensitive to scale (2%
/// off costs a lot of its score). On half-size frames real plates score
/// 0.75-0.9, anything else under 0.5.
const CURIO_PNG: &[u8] = include_bytes!("../assets/curio-plate.png");
const CURIO_HEIGHT_AT: f32 = 1440.0 / 1.35;
const CURIO_SCORE: f32 = 0.6;
/// A curio display's tooltip is about three times its plate's width, and
/// tall (weapons list many lines): its crop, in frame heights.
const CURIO_CROP: (f32, f32) = (0.75, 0.6);

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

fn curio() -> &'static Gray {
    static CURIO: OnceLock<Gray> = OnceLock::new();
    CURIO.get_or_init(|| decode_gray_png(CURIO_PNG))
}

/// Where `art` (sized for frames `art_at` high) is in the frame: coarse at
/// half the frame's resolution everywhere, then each candidate's best spot
/// nearby, best first, one per place. Returns the matches and the art's
/// size in the frame.
fn find_art(
    frame: &Gray,
    ii: &Integral,
    half: &Gray,
    half_ii: &Integral,
    art: &Gray,
    size: f32,
    min_score: f32,
) -> (Vec<(f32, usize, usize)>, usize, usize) {
    let (tw, th) = ((art.width as f32 * size).round() as usize, (art.height as f32 * size).round() as usize);
    let (hw, hh) = ((tw / 2).max(2), (th / 2).max(4));
    if tw < 4 || th < 8 || tw >= frame.width || th >= frame.height || hw >= half.width || hh >= half.height {
        return (Vec::new(), tw, th);
    }
    let full = Template::new(&art.resized(tw, th));
    let coarse = Template::new(&art.resized(hw, hh));
    let mut candidates: Vec<(f32, usize, usize)> = Vec::new();
    if hw >= 24 && hh >= 8 {
        // A big template (the curio plate): coarser still first, a quarter of
        // the work per place and a quarter of the places, then each
        // candidate's best spot nearby at half.
        let quarter = half.resized(half.width / 2, half.height / 2);
        let quarter_ii = Integral::new(&quarter);
        let rough = Template::new(&art.resized(hw / 2, hh / 2));
        for y in 0..=quarter.height.saturating_sub(hh / 2) {
            for x in 0..=quarter.width.saturating_sub(hw / 2) {
                if score(&quarter, &quarter_ii, &rough, x, y) >= COARSE_SCORE - 0.1
                    && let Some(b) = best_near(half, half_ii, &coarse, x * 2, y * 2, 2).filter(|b| b.0 >= COARSE_SCORE)
                {
                    candidates.push((b.0, b.1 * 2, b.2 * 2));
                }
            }
        }
    } else {
        for y in 0..=half.height - hh {
            for x in 0..=half.width - hw {
                let s = score(half, half_ii, &coarse, x, y);
                if s >= COARSE_SCORE {
                    candidates.push((s, x * 2, y * 2));
                }
            }
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut found: Vec<(f32, usize, usize)> = Vec::new();
    for (_, cx, cy) in candidates {
        if found.iter().any(|&(_, x, y)| x.abs_diff(cx) < tw && y.abs_diff(cy) < th) {
            continue;
        }
        if let Some(best) = best_near(frame, ii, &full, cx, cy, 2).filter(|b| b.0 >= min_score)
            && !found.iter().any(|&(_, x, y)| x.abs_diff(best.1) < tw && y.abs_diff(best.2) < th)
        {
            found.push(best);
        }
    }
    (found, tw, th)
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
    pub kind: PlateKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlateKind {
    /// A vault reliquary's, between stone pillars.
    Vault,
    /// A Heist curio display's.
    Curio,
}

impl PlateKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlateKind::Vault => "vault",
            PlateKind::Curio => "curio",
        }
    }
}

/// Finds title plates in a grey frame that is `scale` times smaller than the
/// game's (2: half resolution), whose full height is `full_height`.
pub fn find_plates(frame: &Gray, full_width: u32, full_height: u32) -> Vec<Plate> {
    let scale = full_height as f32 / frame.height as f32;
    let ii = Integral::new(frame);
    let half = frame.resized(frame.width / 2, frame.height / 2);
    let half_ii = Integral::new(&half);
    let mut plates = Vec::new();

    // Vault plates: left pillars, then each one's right pillar in the same
    // row, a plate's width away.
    let size = full_height as f32 / PILLAR_HEIGHT_AT / scale;
    let (lefts, tw, th) = find_art(frame, &ii, &half, &half_ii, pillar(), size, LEFT_SCORE);
    if !lefts.is_empty() {
        let right = Template::new(&pillar().resized(tw, th).mirrored());
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
            plates.push(Plate {
                x,
                y,
                w,
                h,
                crop: crop_for(x, y, w, h, full_width, full_height),
                kind: PlateKind::Vault,
            });
        }
    }

    // Curio display plates: whole.
    let size = full_height as f32 / CURIO_HEIGHT_AT / scale;
    let (curios, tw, th) = find_art(frame, &ii, &half, &half_ii, curio(), size, CURIO_SCORE);
    for (_, cx, cy) in curios {
        let (x, y, w, h) = (cx as f32 * scale, cy as f32 * scale, tw as f32 * scale, th as f32 * scale);
        let fh = full_height as f32;
        let crop = crop_sized(x + w / 2.0, y, CURIO_CROP.0 * fh, CURIO_CROP.1 * fh, full_width, full_height);
        plates.push(Plate { x, y, w, h, crop, kind: PlateKind::Curio });
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

/// A vault tooltip's crop: centred under the plate, from its top down.
fn crop_for(x: f32, y: f32, w: f32, h: f32, full_width: u32, full_height: u32) -> (u32, u32, u32, u32) {
    crop_sized(x + w / 2.0, y, w * CROP_WIDTH, h * CROP_HEIGHT, full_width, full_height)
}

/// A crop `cw` x `ch`, centred on `cx`, from `y` down, kept in the frame.
fn crop_sized(cx: f32, y: f32, cw: f32, ch: f32, full_width: u32, full_height: u32) -> (u32, u32, u32, u32) {
    let cw = cw.min(full_width as f32);
    let x0 = (cx - cw / 2.0).clamp(0.0, full_width as f32 - cw);
    let y0 = y.max(0.0);
    let y1 = (y + ch).min(full_height as f32);
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
        } else if matches!(item.rarity.as_str(), "Rare" | "Magic" | "Normal")
            && let Some((kind, text)) = canonical(&line.text)
        {
            let mut m = crate::item::mod_line(&text);
            m.kind = kind;
            item.mods.push(m);
        }
    }
    Ok(ScreenItem { item, stored_experience, lines: texts })
}

/// Whether any of these lines is an item's name (a unique's or a base's),
/// even though read_item couldn't make an item of them.
pub fn names_an_item(lines: &[Line], vocabulary: &Vocabulary) -> bool {
    lines.iter().take(4).any(|l| {
        vocabulary.closest(&vocabulary.uniques, &l.text).is_some()
            || vocabulary.closest(&vocabulary.bases, &l.text).is_some()
    })
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
            named(None, "Tenderizer", "weapon"),
            named(None, "Primordial Staff", "weapon"),
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
        let ls = lines(&["ATTACK, PROJECTILE", "HITS ENEMIES"]);
        assert!(read_item(&ls, &v, |_| None).is_err());
        assert!(!names_an_item(&ls, &v));
        // A unique's name without its base (the rest covered): not an item
        // yet, but one to read again.
        let covered = lines(&["MOTHER'S EMBRACE", "00:00"]);
        assert!(read_item(&covered, &v, |_| None).is_err());
        assert!(names_an_item(&covered, &v));
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

    /// A Grand Heist screenshot (Records Office): two curio displays. It's a
    /// crop of a 1080p frame: padded back to 1080 rows, so the UI's scale is
    /// right, and halved like the layer's frames.
    fn curio_frame() -> (Vec<u8>, usize, usize) {
        let (rgb, w, h) = load_png("curio-records-office.png");
        let mut padded = rgb;
        padded.resize(w * 1080 * 3, 0);
        (padded, w, 1080.max(h))
    }

    #[test]
    fn finds_curio_plates() {
        let (rgb, w, h) = curio_frame();
        let grey: Vec<u8> =
            rgb.chunks(3).map(|p| ((p[0] as u32 * 3 + p[1] as u32 * 6 + p[2] as u32) / 10) as u8).collect();
        let full = Gray { width: w, height: h, pixels: grey };
        let half = full.resized(w / 2, h / 2);
        let plates = find_plates(&half, w as u32, h as u32);
        let curios: Vec<&Plate> = plates.iter().filter(|p| p.kind == PlateKind::Curio).collect();
        assert_eq!(curios.len(), 2, "{plates:?}");
        let near = |x: f32, y: f32| curios.iter().any(|p| (p.x - x).abs() < 8.0 && (p.y - y).abs() < 8.0);
        assert!(near(350.0, 447.0) && near(803.0, 36.0), "{curios:?}");
        // The crop holds the whole tooltip (the lower one: x 170-720, y 430-890).
        let lower = curios.iter().find(|p| p.y > 300.0).unwrap();
        let (cx, cy, cw, ch) = lower.crop;
        assert!(cx <= 175 && cx + cw >= 715 && cy <= 450 && cy + ch >= 880, "{:?}", lower.crop);
    }

    /// Half of a frame recorded in a Grand Heist at 3456x1440: three curio
    /// displays (Vortex Remnant, Replica Veil of the Night, Soul Remnant).
    #[test]
    fn finds_curio_plates_in_game() {
        let (rgb, w, h) = load_png("heist-half.png");
        let gray = Gray { width: w, height: h, pixels: rgb.chunks(3).map(|p| p[0]).collect() };
        let started = std::time::Instant::now();
        let plates = find_plates(&gray, 3456, 1440);
        eprintln!("found {plates:?} in {}ms", started.elapsed().as_millis());
        let near = |x: f32, y: f32| {
            plates.iter().any(|p| p.kind == PlateKind::Curio && (p.x - x).abs() < 8.0 && (p.y - y).abs() < 8.0)
        };
        assert!(near(1472.0, 122.0) && near(314.0, 826.0) && near(2118.0, 554.0), "{plates:?}");
        assert_eq!(plates.len(), 3, "{plates:?}");
    }

    #[test]
    fn reads_curio_tooltips() {
        if !models_available() {
            return;
        }
        let (rgb, w, h) = curio_frame();
        let grey: Vec<u8> =
            rgb.chunks(3).map(|p| ((p[0] as u32 * 3 + p[1] as u32 * 6 + p[2] as u32) / 10) as u8).collect();
        let half = Gray { width: w, height: h, pixels: grey }.resized(w / 2, h / 2);
        let v = vocabulary();
        let mut names = Vec::new();
        for p in find_plates(&half, w as u32, h as u32).into_iter().filter(|p| p.kind == PlateKind::Curio) {
            let (cx, cy, cw, ch) = (p.crop.0 as usize, p.crop.1 as usize, p.crop.2 as usize, p.crop.3 as usize);
            let mut crop = Vec::new();
            for y in cy..cy + ch {
                crop.extend_from_slice(&rgb[(y * w + cx) * 3..(y * w + cx + cw) * 3]);
            }
            let all = crate::ocr::read(&crop, cw, ch).unwrap();
            let lines = tooltip_lines(&all, p.x - cx as f32 + p.w / 2.0, p.y - cy as f32 + p.h, cw as f32);
            let read = read_item(&lines, &v, |_| None).unwrap();
            assert_eq!(read.item.rarity, "Rare", "{:?}", read.lines);
            names.push((read.item.name.clone().unwrap_or_default(), read.item.base.clone()));
        }
        names.sort();
        assert_eq!(
            names,
            [
                ("Damnation Bane".to_string(), "Tenderizer".to_string()),
                ("Rapture Cry".into(), "Primordial Staff".into())
            ]
        );
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
