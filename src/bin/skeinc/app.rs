use ratatui::style::Color;

// Single source of truth: the daemon's Theme type. Re-exported so the rest
// of skeinc keeps working unchanged.
pub use skein::color::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRole {
    Background,
    Surface,
    SurfaceVariant,
    Primary,
    Accent,
    Text,
    Subtext,
    Dim,
    Border,
    Error,
    Success,
    Warning,
}

pub const ALL_ROLES: [ColorRole; 12] = [
    ColorRole::Background,
    ColorRole::Surface,
    ColorRole::SurfaceVariant,
    ColorRole::Primary,
    ColorRole::Accent,
    ColorRole::Text,
    ColorRole::Subtext,
    ColorRole::Dim,
    ColorRole::Border,
    ColorRole::Error,
    ColorRole::Success,
    ColorRole::Warning,
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
            Self::Dim => "Dim",
            Self::Border => "Border",
            Self::Error => "Error",
            Self::Success => "Success",
            Self::Warning => "Warning",
        }
    }

    pub fn hex(self, t: &Theme) -> &str {
        match self {
            Self::Background => &t.background,
            Self::Surface => &t.surface,
            Self::SurfaceVariant => &t.surface_variant,
            Self::Primary => &t.primary,
            Self::Accent => &t.accent,
            Self::Text => &t.text,
            Self::Subtext => &t.subtext,
            Self::Dim => &t.dim,
            Self::Border => &t.border,
            Self::Error => &t.error,
            Self::Success => &t.success,
            Self::Warning => &t.warning,
        }
    }

    pub fn hex_mut(self, t: &mut Theme) -> &mut String {
        match self {
            Self::Background => &mut t.background,
            Self::Surface => &mut t.surface,
            Self::SurfaceVariant => &mut t.surface_variant,
            Self::Primary => &mut t.primary,
            Self::Accent => &mut t.accent,
            Self::Text => &mut t.text,
            Self::Subtext => &mut t.subtext,
            Self::Dim => &mut t.dim,
            Self::Border => &mut t.border,
            Self::Error => &mut t.error,
            Self::Success => &mut t.success,
            Self::Warning => &mut t.warning,
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
    Theme::test_fallback()
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

    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;

    let l3 = l_ * l_ * l_;
    let m3 = m_ * m_ * m_;
    let s3 = s_ * s_ * s_;

    let r_lin = 4.076_741_7 * l3 - 3.307_711_6 * m3 + 0.230_969_94 * s3;
    let g_lin = -1.268_438 * l3 + 2.609_757_4 * m3 - 0.341_319_38 * s3;
    let b_lin = -0.0041960863 * l3 - 0.703_418_6 * m3 + 1.707_614_7 * s3;

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

    let l_ = (0.412_221_46 * r_lin + 0.536_332_55 * g_lin + 0.051_445_995 * b_lin).cbrt();
    let m_ = (0.211_903_5 * r_lin + 0.680_699_5 * g_lin + 0.107_396_96 * b_lin).cbrt();
    let s_ = (0.088_302_46 * r_lin + 0.281_718_85 * g_lin + 0.629_978_7 * b_lin).cbrt();

    let l = 0.210_454_26 * l_ + 0.793_617_8 * m_ - 0.0040720403 * s_;
    let a = 1.977_998_5 * l_ - 2.428_592_2 * m_ + 0.450_593_7 * s_;
    let b = 0.025_904_037 * l_ + 0.782_771_77 * m_ - 0.808_675_8 * s_;

    let c = (a * a + b * b).sqrt();
    let mut h = b.atan2(a).to_degrees();
    if h < 0.0 {
        h += 360.0;
    }

    (l, c, h)
}

