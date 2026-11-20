//! Reading text off the screen, for items that can't be copied (vault and
//! Heist displays): ocrs, a pure-Rust OCR engine, with its two models (text
//! detection and line recognition, ~12 MB together).
//!
//! The models are looked for in POE_WAYTRADE_OCR_MODELS, then where the
//! package installs them, then in our cache, where they're downloaded on
//! first use.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ocrs::{ImageSource, OcrEngine, OcrEngineParams, TextItem};

use crate::ninja;

const MODELS: [(&str, &str); 2] = [
    ("text-detection.onnx", "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.onnx"),
    ("text-recognition.onnx", "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.onnx"),
];
/// Where the package puts the models.
const SYSTEM_DIR: &str = "/usr/share/poe-waytrade/ocrs";
/// Bigger than either model: a bad reply can't fill the disk.
const MAX_MODEL: u64 = 64 << 20;

/// One recognised line, in the input image's pixels.
#[derive(Debug, Clone)]
pub struct Line {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    /// The text's colour: the mean of the line's brightest pixels (the game
    /// draws light text on a dark panel).
    pub color: [u8; 3],
}

impl Line {
    pub fn center_x(&self) -> f32 {
        self.x + self.w / 2.0
    }
}

static ENGINE: Mutex<Option<Arc<OcrEngine>>> = Mutex::new(None);

/// The engine, loading (and first downloading) its models if need be.
/// Concurrent callers wait for the one load.
fn engine() -> Result<Arc<OcrEngine>, String> {
    let mut slot = ENGINE.lock().unwrap();
    if let Some(e) = slot.as_ref() {
        return Ok(e.clone());
    }
    let started = std::time::Instant::now();
    let mut models = Vec::new();
    for (name, url) in MODELS {
        let path = model_path(name, url)?;
        let model = rten::Model::load_file(&path).map_err(|e| {
            // A cached file that doesn't load is fetched again next time.
            if path.starts_with(cache_dir()) {
                let _ = std::fs::remove_file(&path);
            }
            format!("OCR model {}: {e}", path.display())
        })?;
        models.push(model);
    }
    let recognition = models.pop();
    let detection = models.pop();
    let engine = OcrEngine::new(OcrEngineParams {
        detection_model: detection,
        recognition_model: recognition,
        ..Default::default()
    })
    .map_err(|e| format!("OCR engine: {e}"))?;
    eprintln!("ocr: models loaded in {}ms", started.elapsed().as_millis());
    let engine = Arc::new(engine);
    *slot = Some(engine.clone());
    Ok(engine)
}

/// Loads the models in the background, so the first read doesn't wait.
pub fn prepare() {
    std::thread::spawn(|| {
        if let Err(e) = engine() {
            eprintln!("ocr: {e}");
        }
    });
}

fn cache_dir() -> PathBuf {
    crate::trade::cache_dir().join("ocrs")
}

fn model_path(name: &str, url: &str) -> Result<PathBuf, String> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("POE_WAYTRADE_OCR_MODELS") {
        dirs.push(dir.into());
    }
    dirs.push(SYSTEM_DIR.into());
    dirs.push(cache_dir());
    if let Some(found) = dirs.iter().map(|d| d.join(name)).find(|p| p.is_file()) {
        return Ok(found);
    }
    let path = cache_dir().join(name);
    eprintln!("ocr: downloading {url}");
    download(url, &path)?;
    Ok(path)
}

fn download(url: &str, path: &Path) -> Result<(), String> {
    let resp = ureq::get(url)
        .set("User-Agent", ninja::USER_AGENT)
        .timeout(Duration::from_secs(120))
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let mut bytes = Vec::new();
    resp.into_reader().take(MAX_MODEL).read_to_end(&mut bytes).map_err(|e| format!("{url}: {e}"))?;
    std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    // Written whole, then renamed: another thread may look for it meanwhile.
    let part = path.with_extension("part");
    std::fs::write(&part, &bytes).map_err(|e| format!("{}: {e}", part.display()))?;
    std::fs::rename(&part, path).map_err(|e| format!("{}: {e}", path.display()))
}

/// Text lines in an RGB8 image (rows top to bottom), in reading order.
/// The image is read at twice its size: the game's small caps come out best
/// that way.
pub fn read(rgb: &[u8], width: usize, height: usize) -> Result<Vec<Line>, String> {
    if width == 0 || height == 0 || rgb.len() != width * height * 3 {
        return Err("bad image".into());
    }
    let engine = engine()?;
    let (big, bw, bh) = upscale2(rgb, width, height);
    let source = ImageSource::from_bytes(&big, (bw as u32, bh as u32)).map_err(|e| e.to_string())?;
    let input = engine.prepare_input(source).map_err(|e| e.to_string())?;
    let words = engine.detect_words(&input).map_err(|e| e.to_string())?;
    let lines = engine.find_text_lines(&input, &words);
    let texts = engine.recognize_text(&input, &lines).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for line in texts.into_iter().flatten() {
        let text = line.to_string().trim().to_string();
        if text.is_empty() {
            continue;
        }
        let r = line.bounding_rect();
        let (x, y, w, h) =
            (r.left() as f32 / 2.0, r.top() as f32 / 2.0, r.width() as f32 / 2.0, r.height() as f32 / 2.0);
        let color = text_color(rgb, width, height, x, y, w, h);
        out.push(Line { text, x, y, w, h, color });
    }
    Ok(out)
}

/// Bilinear 2x.
fn upscale2(rgb: &[u8], width: usize, height: usize) -> (Vec<u8>, usize, usize) {
    let (bw, bh) = (width * 2, height * 2);
    let mut out = vec![0u8; bw * bh * 3];
    for y in 0..bh {
        let sy = ((y as f32 + 0.5) / 2.0 - 0.5).max(0.0);
        let y0 = (sy as usize).min(height - 1);
        let y1 = (y0 + 1).min(height - 1);
        let fy = sy - y0 as f32;
        for x in 0..bw {
            let sx = ((x as f32 + 0.5) / 2.0 - 0.5).max(0.0);
            let x0 = (sx as usize).min(width - 1);
            let x1 = (x0 + 1).min(width - 1);
            let fx = sx - x0 as f32;
            for c in 0..3 {
                let p = |xx: usize, yy: usize| rgb[(yy * width + xx) * 3 + c] as f32;
                let top = p(x0, y0) * (1.0 - fx) + p(x1, y0) * fx;
                let bottom = p(x0, y1) * (1.0 - fx) + p(x1, y1) * fx;
                out[(y * bw + x) * 3 + c] = (top * (1.0 - fy) + bottom * fy).round() as u8;
            }
        }
    }
    (out, bw, bh)
}

/// The mean colour of the brightest tenth of a box's pixels (thin strokes
/// cover only part of a line's box).
fn text_color(rgb: &[u8], width: usize, height: usize, x: f32, y: f32, w: f32, h: f32) -> [u8; 3] {
    let (x0, y0) = (x.max(0.0) as usize, y.max(0.0) as usize);
    let (x1, y1) = (((x + w) as usize).min(width), ((y + h) as usize).min(height));
    let mut pixels: Vec<[u8; 3]> = Vec::new();
    for yy in y0..y1 {
        for xx in x0..x1 {
            let i = (yy * width + xx) * 3;
            pixels.push([rgb[i], rgb[i + 1], rgb[i + 2]]);
        }
    }
    if pixels.is_empty() {
        return [0, 0, 0];
    }
    let luma = |p: &[u8; 3]| p[0] as u32 * 3 + p[1] as u32 * 6 + p[2] as u32;
    pixels.sort_by_key(|p| std::cmp::Reverse(luma(p)));
    let top = &pixels[..pixels.len().div_ceil(10)];
    let mut sum = [0u32; 3];
    for p in top {
        for c in 0..3 {
            sum[c] += p[c] as u32;
        }
    }
    let n = top.len() as u32;
    [(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fixture PNG as RGB8.
    pub(crate) fn load_png(name: &str) -> (Vec<u8>, usize, usize) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/ocr").join(name);
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        let (w, h) = (info.width as usize, info.height as usize);
        let rgb = match info.color_type {
            png::ColorType::Rgb => buf[..w * h * 3].to_vec(),
            png::ColorType::Rgba => buf[..w * h * 4].chunks(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
            png::ColorType::Grayscale => buf[..w * h].iter().flat_map(|&p| [p, p, p]).collect(),
            other => panic!("{name}: {other:?}"),
        };
        (rgb, w, h)
    }

    /// The models, if they can be had: tests that need them skip otherwise
    /// (offline package builds without them).
    pub(crate) fn models_available() -> bool {
        let found = MODELS.iter().all(|(name, _)| {
            std::env::var_os("POE_WAYTRADE_OCR_MODELS").is_some_and(|d| Path::new(&d).join(name).is_file())
                || Path::new(SYSTEM_DIR).join(name).is_file()
                || cache_dir().join(name).is_file()
        });
        if !found {
            eprintln!("OCR models not found: skipping (set POE_WAYTRADE_OCR_MODELS)");
        }
        found
    }

    #[test]
    fn upscale_keeps_flat_colour() {
        let (big, w, h) = upscale2(&[10, 20, 30].repeat(6), 3, 2);
        assert_eq!((w, h), (6, 4));
        assert!(big.chunks(3).all(|p| p == [10, 20, 30]));
    }

    #[test]
    fn colour_is_the_bright_pixels() {
        // A dark panel with an orange stroke.
        let mut rgb = [20u8, 20, 20].repeat(100);
        for i in 40..50 {
            rgb[i * 3..i * 3 + 3].copy_from_slice(&[175, 96, 37]);
        }
        assert_eq!(text_color(&rgb, 10, 10, 0.0, 0.0, 10.0, 10.0), [175, 96, 37]);
    }

    #[test]
    fn reads_a_unique_tooltip() {
        if !models_available() {
            return;
        }
        let (rgb, w, h) = load_png("mothers-embrace.png");
        let lines = read(&rgb, w, h).unwrap();
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        // Names needn't come out exact: they're matched against known ones.
        assert!(texts.iter().any(|t| t.starts_with("MOTHER")), "{texts:?}");
        assert!(texts.contains(&"HEAVY BELT"), "{texts:?}");
        assert!(texts.contains(&"+67 TO MAXIMUM LIFE"), "{texts:?}");
        let name = lines.iter().find(|l| l.text.starts_with("MOTHER")).unwrap();
        // Unique orange: red well above blue.
        assert!(name.color[0] > name.color[2] + 60, "{:?}", name.color);
    }
}
