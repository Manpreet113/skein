# skein

Wallpaper-driven color scheme generator and theme daemon — a matugen/pywal-style tool
built in Rust. Watches your wallpaper (or takes an explicit seed color), extracts a
perceptually-uniform Oklch palette that always meets WCAG AA contrast targets, and
fans the theme out to IPC subscribers and rendered template files for all your apps.

Components:

- **`skeind`** — the daemon. Watches a wallpaper directory, re-extracts on change,
  publishes themes over a unix socket, and renders templates to `~/.cache/skein/`.
  Directory events are debounced and access-time reads are ignored, so
  thumbnailers and pickers merely previewing files can't spam re-extracts;
  for an explicit choice, send `skeinc set <path>` instead.
- **`skeinc`** — the client. Interactive TUI to preview/tweak the live theme, or a
  one-shot generator: `skeinc generate <image>`.

## Install

```sh
nix build
nix run .#            # skeind
cargo install --path .         # or via nix profile install
cargo build --release
```

## Usage

```sh
# Daemon — watch ~/.config/skein/wallpaper, push themes to subscribers
skeind

# One-shot — extract from a specific image, render templates, print JSON
skeind --image ~/wallpaper.png --json

# Pin a seed color and enforce AAA text contrast
skeind --seed '#1e88e5' --contrast 7 --mode light

# skeinc one-shot
skeinc generate ~/wallpaper.png --scheme vibrant --mode auto

# Tell the running daemon to switch to an explicit image (the handoff for
# wallpaper pickers — prints the resulting theme once applied)
skeinc set ~/Pictures/Wallpapers/chosen.png

# skeinc TUI (connects to the running daemon)
skeinc
```

Options: `--mode dark|light|auto`, `--scheme vibrant|tonal|muted`, `--seed '#RRGGBB'`,
`--contrast <ratio>`, `--wallpaper-dir`, `--template-dir`, `--output-dir`,
`--socket`, `--config`.

## Config

`~/.config/skein/config.toml` (all optional):

```toml
mode  = "auto"      # dark | light | auto
scheme = "tonal"    # vibrant | tonal | muted
seed  = "#1e88e5"   # pin a seed instead of extracting from the wallpaper
contrast = 4.5
wallpaper_dir = "~/Pictures/wallpapers"
template_dir  = "~/.config/skein/templates"
output_dir    = "~/.cache/skein"
```

CLI flags override config values. `--mode auto` detects the system preference
(`$SKEIN_MODE`, GNOME `gsettings color-scheme`, `$GTK_THEME`, default dark).

## Themes & templates

Templates live in `~/.config/skein/templates/` and are seeded from the built-in set
on first run (never overwritten). Add your own `foo.jinja` → rendered to
`~/.cache/skein/foo`. Each template receives a `theme` object:

```
theme.background / surface / surface_variant / primary / accent / text / subtext
theme.dim / border / red / green / yellow
theme.seed_oklch.{lightness,chroma,hue}  theme.dark_mode
```

`text` meets your `--contrast` target on `background`; `red`/`green`/`yellow`
(fixed semantic hues) always meet 4.5:1 and `dim` meets 3.0:1, so shells can
bind them directly instead of deriving colors client-side.

The daemon also writes a `theme.json` snapshot of the current theme to
`~/.cache/skein/theme.json` on every change, for tools that prefer a file over the socket.

Built-in targets: `colors.css` (Waybar/AGS/CSS), `colors.json`, `alacritty.toml`,
`kitty.conf`, `hyprland.conf`, `rofi.rasi`, `mako`, `gtk.css`, `cava.conf`,
`dunstrc`, `foot.ini`, `swaylock.conf`,
`fish-theme.fish`.

## Home manager

```nix
{ inputs, ... }: {
  imports = [ inputs.skein.homeModules.default ];
  services.skein = {
    enable = true;
    mode   = "auto";
    scheme = "tonal";
    wallpaperDir = "~/Pictures/wallpapers";
  };
}
```

Enables the daemon as a systemd user service and writes `config.toml`.

## Color pipeline

`image → downsample ≤128² → Oklab pixels → K-Means (k=8, Hamerly) →
dominant seed → Oklch tonal palette → WCAG contrast nudging → hex`.
Seeds cluster in perceptual Oklab space; text is nudged in lightness until its
contrast ratio against the background meets `--contrast`.
