use ratatui::style::Color;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Theme {
    pub background: String,
    pub surface: String,
    pub surface_variant: String,
    pub primary: String,
    pub accent: String,
    pub text: String,
    pub subtext: String,
    pub seed_oklch: OklchSeed,
    pub dark_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OklchSeed {
    pub lightness: f32,
    pub chroma: f32,
    pub hue: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRole {
    Background,
    Surface,
    SurfaceVariant,
    Primary,
    Accent,
    Text,
    Subtext,
}

pub const ALL_ROLES: [ColorRole; 7] = [
    ColorRole::Background,
    ColorRole::Surface,
    ColorRole::SurfaceVariant,
    ColorRole::Primary,
    ColorRole::Accent,
    ColorRole::Text,
    ColorRole::Subtext,
];

impl ColorRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Background => "Background",
            Self::Surface => "Surface",
            Self::SurfaceVariant => "Surface+",
            Self::Primary => "Primary",
            Self::Accent => "Accent",
            Self::Text => "Text",
            Self::Subtext => "Subtext",
        }
    }

    pub fn hex<'a>(self, t: &'a Theme) -> &'a str {
        match self {
            Self::Background => &t.background,
            Self::Surface => &t.surface,
            Self::SurfaceVariant => &t.surface_variant,
            Self::Primary => &t.primary,
            Self::Accent => &t.accent,
            Self::Text => &t.text,
            Self::Subtext => &t.subtext,
        }
    }

    pub fn hex_mut<'a>(self, t: &'a mut Theme) -> &'a mut String {
        match self {
            Self::Background => &mut t.background,
            Self::Surface => &mut t.surface,
            Self::SurfaceVariant => &mut t.surface_variant,
            Self::Primary => &mut t.primary,
            Self::Accent => &mut t.accent,
            Self::Text => &mut t.text,
            Self::Subtext => &mut t.subtext,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OklchParam {
    Lightness,
    Chroma,
    Hue,
}

impl OklchParam {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lightness => "Lightness",
            Self::Chroma => "Chroma",
            Self::Hue => "Hue",
        }
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Lightness => Self::Chroma,
            Self::Chroma => Self::Hue,
            Self::Hue => Self::Lightness,
        }
    }
}

pub struct App {
    pub daemon_theme: Theme,
    pub local_theme: Theme,
    pub dirty: bool,
    pub selected_role: usize,
    pub oklch_param: OklchParam,
    pub conn: super::net::ConnState,
    pub should_quit: bool,
}

impl App {
    pub fn new(initial_theme: Theme) -> Self {
        Self {
            daemon_theme: initial_theme.clone(),
            local_theme: initial_theme,
            dirty: false,
            selected_role: 0,
            oklch_param: OklchParam::Lightness,
            conn: super::net::ConnState::Connecting,
            should_quit: false,
        }
    }

    pub fn apply_daemon_theme(&mut self, theme: Theme) {
        self.daemon_theme = theme.clone();
        self.local_theme = theme;
        self.dirty = false;
    }

    pub fn reset_to_daemon(&mut self) {
        self.local_theme = self.daemon_theme.clone();
        self.dirty = false;
    }

    pub fn selected_color_role(&self) -> ColorRole {
        ALL_ROLES[self.selected_role]
    }

    pub fn nudge(&mut self, delta: f32) {
        let role = self.selected_color_role();
        let current_hex = role.hex(&self.local_theme);
        let (mut l, mut c, mut h) = hex_to_oklch(current_hex);

        match self.oklch_param {
            OklchParam::Lightness => {
                l = (l + delta).clamp(0.0, 1.0);
            }
            OklchParam::Chroma => {
                c = (c + delta).clamp(0.0, 0.4);
            }
            OklchParam::Hue => {
                h = (h + delta).rem_euclid(360.0);
            }
        }

        let new_hex = oklch_to_hex(l, c, h);
        *role.hex_mut(&mut self.local_theme) = new_hex;
        self.dirty = self.local_theme != self.daemon_theme;
    }
}

pub fn fallback_theme() -> Theme {
    Theme {
        background: "#1e1e2e".into(),
        surface: "#181825".into(),
        surface_variant: "#313244".into(),
        primary: "#cba6f7".into(),
        accent: "#f38ba8".into(),
        text: "#cdd6f4".into(),
        subtext: "#a6adc8".into(),
        seed_oklch: OklchSeed {
            lightness: 0.72,
            chroma: 0.14,
            hue: 297.0,
        },
        dark_mode: true,
    }
}

pub fn parse_hex_to_ratatui(hex: &str) -> Color {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return Color::Reset;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(128);
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(128);
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(128);
    Color::Rgb(r, g, b)
}

pub fn contrasting_text(bg: Color) -> Color {
    let (r, g, b) = match bg {
        Color::Rgb(r, g, b) => (r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0),
        _ => return Color::White,
    };

    let lin = |c: f32| -> f32 {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };

    let lum = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
    if lum < 0.179 {
        Color::White
    } else {
        Color::Black
    }
}

pub fn oklch_to_hex(l: f32, c: f32, h_deg: f32) -> String {
    let h = h_deg.to_radians();
    let a = c * h.cos();
    let b = c * h.sin();

    let l_ = l + 0.3963377774 * a + 0.2158037573 * b;
    let m_ = l - 0.1055613458 * a - 0.0638541728 * b;
    let s_ = l - 0.0894841775 * a - 1.2914855480 * b;

    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;

    let r_lin = 4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3;
    let g_lin = -1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3;
    let b_lin = -0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3;

    let gamma = |x: f32| -> f32 {
        let x = x.clamp(0.0, 1.0);
        if x <= 0.0031308 {
            12.92 * x
        } else {
            1.055 * x.powf(1.0 / 2.4) - 0.055
        }
    };

    let r = (gamma(r_lin) * 255.0).round() as u8;
    let g = (gamma(g_lin) * 255.0).round() as u8;
    let b_byte = (gamma(b_lin) * 255.0).round() as u8;

    format!("#{r:02X}{g:02X}{b_byte:02X}")
}

pub fn hex_to_oklch(hex: &str) -> (f32, f32, f32) {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return (0.5, 0.0, 0.0);
    }
    let r_u8 = u8::from_str_radix(&hex[0..2], 16).unwrap_or(128);
    let g_u8 = u8::from_str_radix(&hex[2..4], 16).unwrap_or(128);
    let b_u8 = u8::from_str_radix(&hex[4..6], 16).unwrap_or(128);

    let r_srgb = r_u8 as f32 / 255.0;
    let g_srgb = g_u8 as f32 / 255.0;
    let b_srgb = b_u8 as f32 / 255.0;

    let ungamma = |x: f32| -> f32 {
        if x <= 0.04045 {
            x / 12.92
        } else {
            ((x + 0.055) / 1.055).powf(2.4)
        }
    };

    let r_lin = ungamma(r_srgb);
    let g_lin = ungamma(g_srgb);
    let b_lin = ungamma(b_srgb);

    let l_ = (0.4122214708 * r_lin + 0.5363325363 * g_lin + 0.0514459929 * b_lin).cbrt();
    let m_ = (0.2119034982 * r_lin + 0.6806995451 * g_lin + 0.1073969566 * b_lin).cbrt();
    let s_ = (0.0883024619 * r_lin + 0.2817188376 * g_lin + 0.6299787005 * b_lin).cbrt();

    let l = 0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720403 * s_;
    let a = 1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_;
    let b = 0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757993 * s_;

    let c = (a * a + b * b).sqrt();
    let mut h = b.atan2(a).to_degrees();
    if h < 0.0 {
        h += 360.0;
    }

    (l, c, h)
}

