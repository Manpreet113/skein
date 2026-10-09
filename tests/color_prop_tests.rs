//! Property-based tests for the skein color pipeline.
//!
//! These generate thousands of random seeds/schemes/modes and verify the
//! invariants the daemon guarantees to downstream consumers (e.g. a
//! Quickshell desktop shell):
//!   * `text` always meets the requested contrast ratio on `background`.
//!   * `red`/`green`/`yellow` always meet WCAG AA 4.5:1 on `background`.
//!   * `dim` always meets 3.0:1 on `background`.
//!   * every role is a well-formed `#RRGGBB` string.

use proptest::prelude::*;
use skein::color::{theme_from_seed_hex, Scheme, WCAG_AA_NORMAL};

fn arb_hex() -> impl Strategy<Value = String> {
    (any::<u8>(), any::<u8>(), any::<u8>())
        .prop_map(|(r, g, b)| format!("#{r:02X}{g:02X}{b:02X}"))
}

fn arb_scheme() -> impl Strategy<Value = Scheme> {
    prop_oneof![
        Just(Scheme::Vibrant),
        Just(Scheme::Tonal),
        Just(Scheme::Muted),
    ]
}

// --- Local WCAG 2.1 math (kept in-test so lib internals stay private). ---

fn lin(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn luminance(hex: &str) -> f32 {
    let h = hex.trim_start_matches('#');
    let r = u8::from_str_radix(&h[0..2], 16).unwrap() as f32 / 255.0;
    let g = u8::from_str_radix(&h[2..4], 16).unwrap() as f32 / 255.0;
    let b = u8::from_str_radix(&h[4..6], 16).unwrap() as f32 / 255.0;
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

fn contrast_ratio(bg_hex: &str, fg_hex: &str) -> f32 {
    let (l1, l2) = (luminance(bg_hex), luminance(fg_hex));
    let (lighter, darker) = if l1 > l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

proptest! {
    #[test]
    fn text_meets_requested_contrast(
        hex in arb_hex(),
        dark in any::<bool>(),
        scheme in arb_scheme(),
        contrast in 1.0f32..7.0f32,
    ) {
        let theme = theme_from_seed_hex(&hex, dark, scheme, contrast).unwrap();
        let cr = contrast_ratio(&theme.background, &theme.text);
        prop_assert!(
            cr + 1e-3 >= contrast,
            "text contrast {cr:.2}:1 below requested {contrast:.2}:1 \
             (seed={hex}, dark={dark}, scheme={scheme:?})"
        );
    }

    #[test]
    fn status_colors_meet_aa(
        hex in arb_hex(),
        dark in any::<bool>(),
        scheme in arb_scheme(),
    ) {
        let theme = theme_from_seed_hex(&hex, dark, scheme, WCAG_AA_NORMAL).unwrap();
        for (name, role) in [
            ("red", &theme.red),
            ("green", &theme.green),
            ("yellow", &theme.yellow),
        ] {
            let cr = contrast_ratio(&theme.background, role);
            prop_assert!(
                cr + 1e-3 >= 4.5,
                "{name} contrast {cr:.2}:1 below 4.5:1 \
                 (seed={hex}, dark={dark}, scheme={scheme:?})"
            );
        }
        let dim_cr = contrast_ratio(&theme.background, &theme.dim);
        prop_assert!(
            dim_cr + 1e-3 >= 3.0,
            "dim contrast {dim_cr:.2}:1 below 3.0:1 \
             (seed={hex}, dark={dark}, scheme={scheme:?})"
        );
    }

    #[test]
    fn all_roles_are_valid_hex(
        hex in arb_hex(),
        dark in any::<bool>(),
        scheme in arb_scheme(),
    ) {
        let theme = theme_from_seed_hex(&hex, dark, scheme, WCAG_AA_NORMAL).unwrap();
        for (name, role) in [
            ("background", &theme.background),
            ("surface", &theme.surface),
            ("surface_variant", &theme.surface_variant),
            ("primary", &theme.primary),
            ("accent", &theme.accent),
            ("text", &theme.text),
            ("subtext", &theme.subtext),
            ("dim", &theme.dim),
            ("border", &theme.border),
            ("red", &theme.red),
            ("green", &theme.green),
            ("yellow", &theme.yellow),
        ] {
            let body = role.strip_prefix('#');
            let ok = body.map(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()));
            prop_assert!(
                ok == Some(true),
                "{name} is not #RRGGBB: {role} (seed={hex})"
            );
        }
    }

    #[test]
    fn dark_mode_orders_background_below_text(
        hex in arb_hex(),
        scheme in arb_scheme(),
    ) {
        let dark = theme_from_seed_hex(&hex, true, scheme, WCAG_AA_NORMAL).unwrap();
        let light = theme_from_seed_hex(&hex, false, scheme, WCAG_AA_NORMAL).unwrap();
        prop_assert!(
            luminance(&dark.background) < luminance(&dark.text),
            "dark theme has lighter background than text (seed={hex})"
        );
        prop_assert!(
            luminance(&light.background) > luminance(&light.text),
            "light theme has darker background than text (seed={hex})"
        );
    }
}
