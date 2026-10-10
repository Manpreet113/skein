// =============================================================================
// template.rs — Minijinja Legacy Fallback Template Renderer
// =============================================================================
//
// Writes theme data to disk by rendering user-provided Jinja2 templates.
// This is the "last mile" compatibility layer for tools that cannot speak
// Unix Domain Sockets (Kitty, Hyprland, Waybar CSS, etc.).
//
// Design:
//   • Templates live in `~/.config/skein/templates/`.
//   • Each template filename is used as its minijinja template name.
//   • The output path is `~/.cache/skein/<template_name_without_.jinja>`.
//   • The `Theme` struct is serialized to a `serde_json::Value` and passed
//     as the top-level context object `theme` inside each template.
//
// Template example (`colors.css.jinja`):
//   :root {
//     --bg:      {{ theme.background }};
//     --surface: {{ theme.surface }};
//     --primary: {{ theme.primary }};
//     --accent:  {{ theme.accent }};
//     --text:    {{ theme.text }};
//   }
//
// PERFORMANCE NOTE:
//   Templates are compiled once at daemon startup and stored in the
//   `Environment`. Subsequent renders hit only the interpreter — no
//   re-parsing or disk reads. This is safe to call from a normal async
//   context because template rendering is fast (<1 ms per file).
// =============================================================================

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use minijinja::Environment;
use tokio::{fs, sync::Mutex};
use tracing::{debug, info, warn};

use crate::color::Theme;

/// Atomically write the current-theme snapshot (`theme.json`) for
/// consumers that prefer a file over the socket.
///
/// Shared by the daemon broadcast path and the `set-wallpaper` IPC command
/// so every theme change — however triggered — lands in the same file.
pub async fn write_snapshot(output_dir: &Path, theme: &Theme) -> Result<()> {
    let path = output_dir.join("theme.json");
    let json =
        serde_json::to_string_pretty(theme).context("Theme JSON serialization failed")?;
    let tmp = path.with_extension("skein.tmp");
    fs::write(&tmp, &json)
        .await
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .await
        .with_context(|| format!("Failed to rename {} → {}", tmp.display(), path.display()))?;
    debug!(output = %path.display(), "Theme snapshot written");
    Ok(())
}

/// Templates embedded into the binary and seeded into the user's template
/// directory on first run. Existing user files are never overwritten.
const BUILTIN_TEMPLATES: &[(&str, &str)] = &[
    ("colors.css.jinja", include_str!("../templates/colors.css.jinja")),
    ("colors.json.jinja", include_str!("../templates/colors.json.jinja")),
    ("alacritty.toml.jinja", include_str!("../templates/alacritty.toml.jinja")),
    ("kitty.conf.jinja", include_str!("../templates/kitty.conf.jinja")),
    ("hyprland.conf.jinja", include_str!("../templates/hyprland.conf.jinja")),
    ("rofi.rasi.jinja", include_str!("../templates/rofi.rasi.jinja")),
    ("mako.jinja", include_str!("../templates/mako.jinja")),
    ("gtk.css.jinja", include_str!("../templates/gtk.css.jinja")),
    ("cava.conf.jinja", include_str!("../templates/cava.conf.jinja")),
    ("fish-theme.fish.jinja", include_str!("../templates/fish-theme.fish.jinja")),
    ("dunst.conf.jinja", include_str!("../templates/dunst.conf.jinja")),
    ("foot.ini.jinja", include_str!("../templates/foot.ini.jinja")),
    ("swaylock.conf.jinja", include_str!("../templates/swaylock.conf.jinja")),
    ("ghostty.conf.jinja", include_str!("../templates/ghostty.conf.jinja")),
    ("starship.toml.jinja", include_str!("../templates/starship.toml.jinja")),
    ("btop.theme.jinja", include_str!("../templates/btop.theme.jinja")),
    ("fzf.fish.jinja", include_str!("../templates/fzf.fish.jinja")),
    ("hyprland.lua.jinja", include_str!("../templates/hyprland.lua.jinja")),
    ("hyprlock.conf.jinja", include_str!("../templates/hyprlock.conf.jinja")),
    ("swaync.css.jinja", include_str!("../templates/swaync.css.jinja")),
    ("wlogout.css.jinja", include_str!("../templates/wlogout.css.jinja")),
    ("waybar.css.jinja", include_str!("../templates/waybar.css.jinja")),
    ("fuzzel.ini.jinja", include_str!("../templates/fuzzel.ini.jinja")),
    ("zellij.kdl.jinja", include_str!("../templates/zellij.kdl.jinja")),
    ("tmux.conf.jinja", include_str!("../templates/tmux.conf.jinja")),
    ("yazi-theme.toml.jinja", include_str!("../templates/yazi-theme.toml.jinja")),
    ("tide-theme.fish.jinja", include_str!("../templates/tide-theme.fish.jinja")),
    ("skein.theme.css.jinja", include_str!("../templates/skein.theme.css.jinja")),
    ("skein-color-theme.json.jinja", include_str!("../templates/skein-color-theme.json.jinja")),
    ("skein.lua.jinja", include_str!("../templates/skein.lua.jinja")),
    ("zen-userChrome.css.jinja", include_str!("../templates/zen-userChrome.css.jinja")),
];

/// Copy any missing builtin templates into `template_dir`.
pub async fn seed_builtin_templates(template_dir: &Path) -> Result<()> {
    fs::create_dir_all(template_dir)
        .await
        .with_context(|| format!("Failed to create template dir {}", template_dir.display()))?;

    for (name, source) in BUILTIN_TEMPLATES {
        let path = template_dir.join(name);
        if path.exists() {
            continue;
        }
        fs::write(&path, *source)
            .await
            .with_context(|| format!("Failed to write builtin template {}", path.display()))?;
        info!(template = %path.display(), "Seeded builtin template");
    }

    Ok(())
}

/// Build the `theme_rgb` template context: every role as an `[r, g, b]`
/// decimal triple mirroring the `theme` hex strings.
fn rgb_context(theme: &Theme) -> serde_json::Value {
    let triple = |hex: &str| {
        let h = hex.trim_start_matches('#');
        let ch = |i: usize| {
            u8::from_str_radix(h.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0)
        };
        serde_json::json!([ch(0), ch(2), ch(4)])
    };
    let a = &theme.ansi;
    serde_json::json!({
        "background": triple(&theme.background),
        "surface": triple(&theme.surface),
        "surface_variant": triple(&theme.surface_variant),
        "primary": triple(&theme.primary),
        "accent": triple(&theme.accent),
        "text": triple(&theme.text),
        "subtext": triple(&theme.subtext),
        "dim": triple(&theme.dim),
        "border": triple(&theme.border),
        "error": triple(&theme.error),
        "success": triple(&theme.success),
        "warning": triple(&theme.warning),
        "ansi": {
            "black": triple(&a.black),
            "red": triple(&a.red),
            "green": triple(&a.green),
            "yellow": triple(&a.yellow),
            "blue": triple(&a.blue),
            "magenta": triple(&a.magenta),
            "cyan": triple(&a.cyan),
            "white": triple(&a.white),
            "bright_black": triple(&a.bright_black),
            "bright_red": triple(&a.bright_red),
            "bright_green": triple(&a.bright_green),
            "bright_yellow": triple(&a.bright_yellow),
            "bright_blue": triple(&a.bright_blue),
            "bright_magenta": triple(&a.bright_magenta),
            "bright_cyan": triple(&a.bright_cyan),
            "bright_white": triple(&a.bright_white),
        },
    })
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// A compiled template environment ready for rendering on each theme change.
///
/// Wrap in `Arc` and share across tasks; internal state is protected by a
/// `Mutex` because `Environment` is not `Sync` when using the `loader` feature
/// with mutable state.
pub struct TemplateRenderer {
    env: Mutex<Environment<'static>>,
    /// `(template_name, output_path)` pairs discovered at load time.
    outputs: Vec<(String, PathBuf)>,
}

impl TemplateRenderer {
    /// Discover templates in `template_dir` and compile them.
    ///
    /// Template files must end in `.jinja`.  The output path is derived by
    /// stripping the `.jinja` suffix and writing to `output_dir`.
    ///
    /// `targets` optionally restricts which templates render, matched by
    /// output filename (`"kitty.conf"`, with or without the `.jinja`
    /// suffix); `"*"` and `None` both mean everything. Unlisted templates
    /// are still seeded, just never rendered.
    ///
    /// Example:
    ///   template_dir  = ~/.config/skein/templates/
    ///   output_dir    = ~/.cache/skein/
    ///   template file = colors.css.jinja  →  output = ~/.cache/skein/colors.css
    pub async fn load(
        template_dir: &Path,
        output_dir: &Path,
        targets: Option<&[String]>,
    ) -> Result<Arc<Self>> {
        // Ensure the output directory exists.
        fs::create_dir_all(output_dir)
            .await
            .with_context(|| format!("Failed to create output dir {}", output_dir.display()))?;

        let wanted: Option<std::collections::HashSet<String>> = targets
            .filter(|list| !list.iter().any(|t| t.trim() == "*"))
            .map(|list| {
                list.iter()
                    .map(|t| t.trim().trim_end_matches(".jinja").to_owned())
                    .collect()
            });

        let mut env = Environment::new();
        // skein templates emit code/markup, never HTML — disable autoescape.
        // (minijinja infers a JSON autoescape for template names like
        // `colors.json.jinja`, which would quote every interpolated value.)
        env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
        let mut outputs: Vec<(String, PathBuf)> = Vec::new();

        // Walk template_dir, load each .jinja file.
        let mut dir = match fs::read_dir(template_dir).await {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    dir = %template_dir.display(),
                    error = %e,
                    "Template directory not found or unreadable; no templates loaded"
                );
                return Ok(Arc::new(Self {
                    env: Mutex::new(env),
                    outputs,
                }));
            }
        };

        while let Some(entry) = dir
            .next_entry()
            .await
            .context("Failed to read template dir entry")?
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jinja") {
                continue;
            }

            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();

            let source = fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to read template {}", path.display()))?;

            env.add_template_owned(name.clone(), source)
                .with_context(|| format!("Failed to compile template '{name}'"))?;

            // Strip `.jinja` suffix for output filename.
            let out_name = name.trim_end_matches(".jinja").to_owned();
            let out_path = output_dir.join(&out_name);

            // Honor the `targets` allowlist: everything compiles (so errors
            // still surface at startup), but unlisted templates never render.
            if wanted.as_ref().is_some_and(|w| !w.contains(&out_name)) {
                debug!(template = %name, "Skipping template (not in targets)");
                continue;
            }

            info!(template = %name, output = %out_path.display(), "Loaded template");
            outputs.push((name, out_path));
        }

        info!(count = outputs.len(), "Templates loaded");

        Ok(Arc::new(Self {
            env: Mutex::new(env),
            outputs,
        }))
    }

    /// Render all templates with the current `theme` and write outputs to disk.
    ///
    /// Errors from individual templates are logged as warnings but do not abort
    /// the render pass — a malformed template should not kill the daemon.
    pub async fn render_all(&self, theme: &Theme) -> Result<()> {
        if self.outputs.is_empty() {
            return Ok(());
        }

        // Convert the Theme to a serde_json::Value so minijinja can traverse it.
        // This is a cheap in-process serialization, not file I/O.
        let theme_value =
            serde_json::to_value(theme).context("Failed to serialize Theme for template context")?;
        // Decimal RGB triples for consumers that cannot parse hex (e.g.
        // hyprlock's `rgb(R, G, B)`). Use with the `join` filter:
        //   rgb({{ theme_rgb.background | join(", ") }})
        let theme_rgb = rgb_context(theme);

        // Phase 1: Render all templates while holding the lock.
        // All rendering is synchronous (CPU-bound, fast), so we do it all in one
        // lock acquisition to avoid repeatedly locking/unlocking.
        let rendered_outputs: Vec<(PathBuf, String)> = {
            let env = self.env.lock().await;
            self.outputs
                .iter()
                .filter_map(|(template_name, out_path)| {
                    match env.get_template(template_name) {
                        Ok(tmpl) => {
                            match tmpl.render(minijinja::context! { theme => &theme_value, theme_rgb => &theme_rgb }) {
                                Ok(rendered) => Some((out_path.clone(), rendered)),
                                Err(e) => {
                                    warn!(template = %template_name, error = %e, "Template render error");
                                    None
                                }
                            }
                        }
                        Err(e) => {
                            warn!(template = %template_name, error = %e, "Template not found in env");
                            None
                        }
                    }
                })
                .collect()
        }; // Lock is released here.

        // Phase 2: Write rendered outputs to disk (async I/O, no lock held).
        for (out_path, rendered) in rendered_outputs {
            // Atomic write: write to a temp file then rename, preventing
            // partial reads by hot-reloading tools (Waybar, Kitty, etc.).
            let tmp_path = out_path.with_extension("skein.tmp");

            if let Err(e) = fs::write(&tmp_path, &rendered).await {
                warn!(path = %tmp_path.display(), error = %e, "Failed to write temp template output");
                continue;
            }

            if let Err(e) = fs::rename(&tmp_path, &out_path).await {
                warn!(
                    from = %tmp_path.display(),
                    to = %out_path.display(),
                    error = %e,
                    "Failed to atomically rename template output"
                );
            } else {
                debug!(output = %out_path.display(), "Template rendered");
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_theme() -> Theme {
        Theme::test_fallback()
    }

    #[tokio::test]
    async fn renders_template_to_file() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpl_dir = tmp.path().join("templates");
        let out_dir = tmp.path().join("output");
        std::fs::create_dir_all(&tmpl_dir).unwrap();

        // Write a simple template.
        std::fs::write(
            tmpl_dir.join("colors.css.jinja"),
            ":root { --bg: {{ theme.background }}; --fg: {{ theme.text }}; }",
        )
        .unwrap();

        let renderer = TemplateRenderer::load(&tmpl_dir, &out_dir, None).await.unwrap();
        renderer.render_all(&dummy_theme()).await.unwrap();

        let output = std::fs::read_to_string(out_dir.join("colors.css")).unwrap();
        assert!(output.contains("#1e1e2e"), "background color missing from output");
        assert!(output.contains("#cdd6f4"), "text color missing from output");
    }

    #[tokio::test]
    async fn handles_missing_template_dir_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpl_dir = tmp.path().join("nonexistent");
        let out_dir = tmp.path().join("output");

        // Should not error — just warns and returns an empty renderer.
        let renderer = TemplateRenderer::load(&tmpl_dir, &out_dir, None).await.unwrap();
        renderer.render_all(&dummy_theme()).await.unwrap();
    }

    #[tokio::test]
    async fn targets_allowlist_skips_unlisted_templates() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpl_dir = tmp.path().join("templates");
        let out_dir = tmp.path().join("output");
        std::fs::create_dir_all(&tmpl_dir).unwrap();

        std::fs::write(tmpl_dir.join("a.css.jinja"), "a {{ theme.background }}").unwrap();
        std::fs::write(tmpl_dir.join("b.css.jinja"), "b {{ theme.background }}").unwrap();

        let allow = vec!["a.css".to_string()];
        let renderer = TemplateRenderer::load(&tmpl_dir, &out_dir, Some(&allow))
            .await
            .unwrap();
        renderer.render_all(&dummy_theme()).await.unwrap();

        assert!(out_dir.join("a.css").exists());
        assert!(!out_dir.join("b.css").exists());
    }

    #[tokio::test]
    async fn targets_star_renders_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let tmpl_dir = tmp.path().join("templates");
        let out_dir = tmp.path().join("output");
        std::fs::create_dir_all(&tmpl_dir).unwrap();

        std::fs::write(tmpl_dir.join("a.css.jinja"), "a").unwrap();
        let allow = vec!["*".to_string(), "whatever".to_string()];
        let renderer = TemplateRenderer::load(&tmpl_dir, &out_dir, Some(&allow))
            .await
            .unwrap();
        renderer.render_all(&dummy_theme()).await.unwrap();

        assert!(out_dir.join("a.css").exists());
    }
}
