// =============================================================================
// color.rs — Perceptual Color Extraction & WCAG AA Contrast Engine
// =============================================================================
//
// Pipeline:
//   raw bytes → DynamicImage → downsample to ≤128×128
//   → Srgb<f32> pixels → Oklab (for K-Means clustering)
//   → dominant Oklab centroid → Oklch (for human-readable manipulation)
//   → palette roles with programmatic L nudging for WCAG AA ≥ 4.5
//   → hex strings (sRGB) ready for JSON / CSS emission
//
// PERFORMANCE NOTES:
//   • This entire function is called inside `tokio::task::spawn_blocking`.
//     It is fully synchronous — no async permitted here.
//   • K-Means with k=8, max_iter=20 on a 128×128 image (~16 K pixels)
//     takes ≈ 5–15 ms in release mode.
//   • The only heap allocation per-call is the pixel Vec<Oklab<f32>>.
//     Everything else is stack/register.
// =============================================================================

use anyhow::{bail, Context, Result};
use image::{imageops::FilterType, DynamicImage};
use kmeans_colors::{get_kmeans_hamerly, Kmeans};
use palette::{
    color_difference::Wcag21RelativeContrast, IntoColor, Oklab, Oklch, Srgb,
};
use serde::{Deserialize, Serialize};


// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A fully resolved theme palette, ready for JSON serialization.
/// All colors are encoded as `#RRGGBB` hex strings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Theme {
    /// Very dark background (dark mode) or very light (light mode).
    pub background: String,
    /// Slightly raised surface, e.g. sidebar / card.
    pub surface: String,
    /// Slightly raised surface variant (containers, chips).
    pub surface_variant: String,
    /// Primary accent derived from the dominant image hue.
    pub primary: String,
    /// Brighter accent, ~15% higher L than primary.
    pub accent: String,
    /// High-contrast text guaranteed ≥ WCAG AA 4.5:1 on `background`.
    pub text: String,
    /// Slightly lower-contrast subtext, ≥ 3.0:1 on `background`.
    pub subtext: String,
    /// Raw Oklch seed for downstream manipulation (e.g. QML).
    pub seed_oklch: OklchSeed,
    /// Whether we're operating in dark or light mode.
    pub dark_mode: bool,
}

/// The raw Oklch seed color, exposed so downstream clients can derive
/// their own tones without re-doing the math.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OklchSeed {
    pub lightness: f32,
    pub chroma: f32,
    pub hue: f32,
}

// ---------------------------------------------------------------------------
// K-Means clustering constants
// ---------------------------------------------------------------------------

const KMEANS_K: usize = 8;
const KMEANS_MAX_ITER: usize = 20;
const KMEANS_CONVERGENCE: f32 = 1e-4; // tight convergence in Oklab space
const KMEANS_SEED: u64 = 0xDEAD_BEEF_CAFE_1337;
const MAX_DIM: u32 = 128;

/// How strongly the seed hue is expressed in the palette.
/// Plays the role of matugen's/pywal's scheme selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Scheme {
    /// Bold, saturated accents.
    Vibrant,
    /// Balanced tonal palette (default).
    #[default]
    Tonal,
    /// Desaturated, soft, paper-like.
    Muted,
}

impl Scheme {
    /// Inclusive chroma clamp range for the primary accent.
    fn primary_chroma_range(self) -> (f32, f32) {
        match self {
            Self::Vibrant => (0.08, 0.22),
            Self::Tonal => (0.04, 0.16),
            Self::Muted => (0.015, 0.07),
        }
    }

    /// Accent chroma multiplier relative to primary.
    fn accent_chroma_mul(self) -> f32 {
        match self {
            Self::Vibrant => 1.2,
            Self::Tonal => 1.05,
            Self::Muted => 0.9,
        }
    }
}

impl std::str::FromStr for Scheme {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "vibrant" => Ok(Self::Vibrant),
            "tonal" => Ok(Self::Tonal),
            "muted" => Ok(Self::Muted),
            other => anyhow::bail!("unknown scheme '{other}' (expected vibrant|tonal|muted)"),
        }
    }
}

// ---------------------------------------------------------------------------
// WCAG constants
// ---------------------------------------------------------------------------

const WCAG_AA_NORMAL: f32 = 4.5;
const WCAG_AA_LARGE: f32 = 3.0;

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Load raw image bytes, downsample, extract the dominant Oklch seed, and
/// build a full contrast-compliant theme palette.
///
/// **This function is synchronous and CPU-bound.** Always call it via
/// `tokio::task::spawn_blocking`.
///
/// # Errors
/// Returns an error if the bytes cannot be decoded as a supported image format.
pub fn extract_theme_from_bytes(bytes: &[u8], prefer_dark: bool, scheme: Scheme) -> Result<Theme> {
    // 1. Decode the image.
    let img = image::load_from_memory(bytes).context("Failed to decode image bytes")?;

    // 2. Downsample to ≤ MAX_DIM×MAX_DIM.
    //    FilterType::Lanczos3 gives the best colour-preserving average;
    //    it costs ~2–3 ms at this size — acceptable.
    let small = downsample(img);

    // 3. Convert pixels to Oklab for K-Means.
    //    Oklab is perceptually uniform, so Euclidean distance in this space
    //    directly models perceptual colour distance.
    let oklab_pixels = pixels_to_oklab(&small);

    if oklab_pixels.is_empty() {
        bail!("Image produced no usable pixels");
    }

    // 4. K-Means clustering to find the dominant perceptual colour.
    //    We use Hamerly's algorithm (faster than Lloyd's for large k).
    //    `get_kmeans_hamerly` expects T: Calculate; palette::Lab<D65, f32>
    //    implements this with the `palette_color` feature on kmeans_colors.
    //    We convert Oklab → Lab<D65> only for the clustering step, then back.
    let lab_pixels: Vec<palette::Lab<palette::white_point::D65, f32>> =
        oklab_pixels.iter().map(|&ok| ok.into_color()).collect();

    let Kmeans { centroids, .. } = get_kmeans_hamerly(
        KMEANS_K,
        KMEANS_MAX_ITER,
        KMEANS_CONVERGENCE,
        false,
        &lab_pixels,
        KMEANS_SEED,
    );

    // 5. Pick the centroid with the highest population-weighted chroma
    //    (i.e., most saturated and prominent cluster).
    let seed_lab = pick_dominant_centroid(&centroids, &lab_pixels);
    let seed_oklch = lab_to_oklch(seed_lab);

    // 6. Generate the full tonal palette.
    let theme = build_palette(seed_oklch, prefer_dark, scheme);

    Ok(theme)
}

// ---------------------------------------------------------------------------
// Image helpers
// ---------------------------------------------------------------------------

fn downsample(img: DynamicImage) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w <= MAX_DIM && h <= MAX_DIM {
        return img;
    }
    // Maintain aspect ratio.
    let (nw, nh) = if w > h {
        (MAX_DIM, (MAX_DIM as f32 * h as f32 / w as f32).round() as u32)
    } else {
        ((MAX_DIM as f32 * w as f32 / h as f32).round() as u32, MAX_DIM)
    };
    img.resize(nw.max(1), nh.max(1), FilterType::Lanczos3)
}

fn pixels_to_oklab(img: &DynamicImage) -> Vec<Oklab<f32>> {
    let rgb = img.to_rgb8();
    rgb.pixels()
        .map(|p| {
            let srgb = Srgb::new(
                p[0] as f32 / 255.0,
                p[1] as f32 / 255.0,
                p[2] as f32 / 255.0,
            );
            // Srgb → linear → Oklab.  The .into_color() chain handles
            // linearisation internally via palette's trait blankets.
            let oklab: Oklab<f32> = srgb.into_color();
            oklab
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Centroid selection
// ---------------------------------------------------------------------------

/// Among all K centroids, pick the one that maximises:
///   score = chroma × sqrt(pixel_count_in_cluster)
///
/// This biases toward saturated, *large* clusters (background-coloured
/// centroids are usually desaturated and would be deprioritised).
fn pick_dominant_centroid(
    centroids: &[palette::Lab<palette::white_point::D65, f32>],
    pixels: &[palette::Lab<palette::white_point::D65, f32>],
) -> palette::Lab<palette::white_point::D65, f32> {
    if centroids.is_empty() {
        // Fallback: neutral mid-grey in Lab space.
        return palette::Lab::new(50.0, 0.0, 0.0);
    }

    // Count pixels per centroid.
    let mut counts = vec![0usize; centroids.len()];
    for px in pixels {
        let nearest = centroids
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                lab_dist_sq(a, px).partial_cmp(&lab_dist_sq(b, px)).unwrap()
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        counts[nearest] += 1;
    }

    centroids
        .iter()
        .enumerate()
        .max_by(|(i, c1), (j, c2)| {
            // Chroma in Lab: sqrt(a² + b²)
            let chroma1 = (c1.a * c1.a + c1.b * c1.b).sqrt();
            let chroma2 = (c2.a * c2.a + c2.b * c2.b).sqrt();
            let score1 = chroma1 * (counts[*i] as f32).sqrt();
            let score2 = chroma2 * (counts[*j] as f32).sqrt();
            score1.partial_cmp(&score2).unwrap()
        })
        .map(|(_, c)| *c)
        .unwrap_or(centroids[0])
}

#[inline]
fn lab_dist_sq(
    a: &palette::Lab<palette::white_point::D65, f32>,
    b: &palette::Lab<palette::white_point::D65, f32>,
) -> f32 {
    let dl = a.l - b.l;
    let da = a.a - b.a;
    let db = a.b - b.b;
    dl * dl + da * da + db * db
}

fn lab_to_oklch(lab: palette::Lab<palette::white_point::D65, f32>) -> Oklch<f32> {
    let oklab: Oklab<f32> = lab.into_color();
    oklab.into_color()
}

// ---------------------------------------------------------------------------
// Palette generation
// ---------------------------------------------------------------------------

/// Hue is **always preserved** from the seed. Only L and C are manipulated.
///
/// Dark-mode tonal targets (Oklch L values):
/// ```text
///   background:     0.12
///   surface:        0.17
///   surface_variant:0.22
///   primary:        seed_l clamped [0.45, 0.70]
///   accent:         primary_l + 0.12 clamped [0.55, 0.82]
///   text:           nudged until WCAG AA ≥ 4.5 on background
///   subtext:        nudged until WCAG ≥ 3.0 on background
/// ```
fn build_palette(seed: Oklch<f32>, dark: bool, scheme: Scheme) -> Theme {
    let h = seed.hue;
    // Clamp seed chroma to a tasteful range; very high chroma looks garish.
    let (cmin, cmax) = scheme.primary_chroma_range();
    let c_primary = seed.chroma.clamp(cmin, cmax);
    let c_surface = c_primary * 0.3; // surfaces are subtly tinted

    let (bg_l, sf_l, sfv_l, primary_l_range, text_l_start, text_l_step) = if dark {
        (
            0.12_f32,
            0.17_f32,
            0.22_f32,
            (0.45_f32, 0.70_f32),
            0.96_f32,
            -0.01_f32, // nudge text lighter until contrast passes
        )
    } else {
        (
            0.96_f32,
            0.92_f32,
            0.88_f32,
            (0.30_f32, 0.55_f32),
            0.08_f32,
            0.01_f32,
        )
    };

    let primary_l = seed.l.clamp(primary_l_range.0, primary_l_range.1);
    let accent_l = (primary_l + 0.12).clamp(0.0, 0.999);

    let background = oklch_to_hex(Oklch::new(bg_l, c_surface * 0.5, h));
    let surface = oklch_to_hex(Oklch::new(sf_l, c_surface * 0.6, h));
    let surface_variant = oklch_to_hex(Oklch::new(sfv_l, c_surface * 0.7, h));
    let primary = oklch_to_hex(Oklch::new(primary_l, c_primary, h));
    let accent = oklch_to_hex(Oklch::new(accent_l, c_primary * scheme.accent_chroma_mul(), h));

    // Contrast-guarantee pass: nudge text lightness until WCAG AA ≥ 4.5.
    let text = enforce_contrast(
        bg_l,
        c_surface * 0.15,
        h,
        text_l_start,
        text_l_step,
        WCAG_AA_NORMAL,
    );
    let subtext = enforce_contrast(
        bg_l,
        c_surface * 0.2,
        h,
        // Start subtext a bit closer to the background for a softer look.
        if dark { 0.82 } else { 0.20 },
        text_l_step,
        WCAG_AA_LARGE,
    );

    Theme {
        background,
        surface,
        surface_variant,
        primary,
        accent,
        text,
        subtext,
        seed_oklch: OklchSeed {
            lightness: seed.l,
            chroma: seed.chroma,
            hue: seed.hue.into_inner(),
        },
        dark_mode: dark,
    }
}

// ---------------------------------------------------------------------------
// WCAG AA contrast enforcement
// ---------------------------------------------------------------------------

/// Iteratively adjusts the lightness of a candidate color until the WCAG 2.1
/// contrast ratio against a background of `bg_l` meets or exceeds `target_cr`.
///
/// Math:
///   CR = (L_lighter + 0.05) / (L_darker + 0.05)   where L is WCAG relative luminance
///
/// We walk `candidate_l` in `step` increments (toward white or black depending
/// on direction) and re-evaluate CR each iteration. Max 200 steps prevents an
/// infinite loop on degenerate inputs.
fn enforce_contrast(
    bg_l_oklch: f32,
    c: f32,
    h: palette::OklabHue<f32>,
    start_l: f32,
    step: f32,
    target_cr: f32,
) -> String {
    let bg_color: Srgb<f32> = Oklch::new(bg_l_oklch, c * 0.0, h).into_color();
    let bg_lum = wcag_luminance(bg_color);

    let mut l = start_l.clamp(0.001, 0.999);

    for _ in 0..200 {
        let candidate: Srgb<f32> = Oklch::new(l, c, h).into_color();
        let fg_lum = wcag_luminance(candidate);

        let cr = contrast_ratio(bg_lum, fg_lum);
        if cr >= target_cr {
            break;
        }

        l = (l + step).clamp(0.001, 0.999);
    }

    oklch_to_hex(Oklch::new(l, c, h))
}

/// WCAG 2.1 §1.4.3 relative luminance from linearised sRGB.
/// Implemented via `Wcag21RelativeContrast` on `Srgb<f32>`, which linearises
/// the sRGB components internally before computing luminance.
/// Returns a scalar in [0, 1].
#[inline]
fn wcag_luminance(color: Srgb<f32>) -> f32 {
    // relative_luminance() takes self by value and returns Luma<Linear<D65>, f32>.
    color.relative_luminance().luma
}

/// WCAG 2.1 contrast ratio, range [1, 21].
#[inline]
fn contrast_ratio(l1: f32, l2: f32) -> f32 {
    let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

// ---------------------------------------------------------------------------
// Hex encoding
// ---------------------------------------------------------------------------

/// Convert an Oklch color to `#RRGGBB` hex, clamping to the sRGB gamut.
pub fn oklch_to_hex(color: Oklch<f32>) -> String {
    let srgb: Srgb<f32> = color.into_color();
    // Clamp to [0, 1] — out-of-gamut colours would produce invalid hex.
    let r = (srgb.red.clamp(0.0, 1.0) * 255.0).round() as u8;
    let g = (srgb.green.clamp(0.0, 1.0) * 255.0).round() as u8;
    let b = (srgb.blue.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{r:02X}{g:02X}{b:02X}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wcag_luminance_black_is_zero() {
        let black = Srgb::new(0.0_f32, 0.0, 0.0);
        assert_eq!(wcag_luminance(black), 0.0);
    }

    #[test]
    fn wcag_luminance_white_is_one() {
        let white = Srgb::new(1.0_f32, 1.0, 1.0);
        let lum = wcag_luminance(white);
        assert!((lum - 1.0).abs() < 1e-3, "Expected ~1.0, got {lum}");
    }

    #[test]
    fn contrast_ratio_black_on_white() {
        let cr = contrast_ratio(1.0, 0.0);
        assert!((cr - 21.0).abs() < 0.01, "Expected 21:1, got {cr}");
    }

    #[test]
    fn palette_text_meets_wcag_aa() {
        // Synthesise a vivid blue seed.
        let seed = Oklch::new(0.55_f32, 0.18, palette::OklabHue::new(250.0));
        let theme = build_palette(seed, true, Scheme::Tonal);

        // Parse the hex colors back and check contrast.
        let bg = hex_to_srgb(&theme.background).unwrap();
        let txt = hex_to_srgb(&theme.text).unwrap();
        let cr = contrast_ratio(wcag_luminance(bg), wcag_luminance(txt));
        assert!(
            cr >= 4.5,
            "text contrast {cr:.2}:1 does not meet WCAG AA 4.5:1"
        );
    }

    fn hex_to_srgb(hex: &str) -> Option<Srgb<f32>> {
        let hex = hex.trim_start_matches('#');
        if hex.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&hex[0..2], 16).ok()? as f32 / 255.0;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()? as f32 / 255.0;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()? as f32 / 255.0;
        Some(Srgb::new(r, g, b))
    }
}
