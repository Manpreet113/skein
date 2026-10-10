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

### App selection & placement

`targets` restricts which templates render (matched by output filename, e.g.
`"kitty.conf"`); omit it to render everything. `[deploy]` symlinks rendered
outputs into the live locations where apps actually read them. It defaults
**off** — nothing outside the cache is touched unless you opt in:

```toml
targets = ["kitty.conf", "ghostty.conf", "fish-theme.fish"]

[deploy]
enable = true
on_conflict = "backup"   # backup | overwrite | skip

[deploy.targets]
"fish-theme.fish" = "~/.config/fish/conf.d/skein-theme.fish"
"yazi-theme.toml" = "~/.config/yazi/theme.toml"
"btop.theme"      = "~/.config/btop/themes/skein.theme"
"skein.lua"       = "~/.config/nvim/colors/skein.lua"
```

Safety rules: existing files are backed up once to `<dest>.skein-bak`
(`overwrite` replaces with a warning, `skip` leaves them alone); directories
are never touched; managed destinations are recorded in
`~/.cache/skein/deployed.json`; per-target failures warn and continue.
`include`/`source`-style templates (kitty, foot, starship, …) need no entry —
they are consumed from the cache directly. Run `skeinc deploy` to apply
placement immediately without waiting for a theme change. Do not list paths
managed by home-manager (or anything else) here — pick one owner per path.

## Themes & templates

Templates live in `~/.config/skein/templates/` and are seeded from the built-in set
on first run (never overwritten). Add your own `foo.jinja` → rendered to
`~/.cache/skein/foo`. Each template receives a `theme` object:

```
theme.background / surface / surface_variant / primary / accent / text / subtext
theme.dim / border / error / success / warning
theme.ansi.{black,red,green,yellow,blue,magenta,cyan,white}
theme.ansi.{bright_black,bright_red,…,bright_white}
theme.seed_oklch.{lightness,chroma,hue}  theme.dark_mode
```

Role names are semantic, not hues: `error`/`success`/`warning` live in the
red/green/amber families by spec (destructive actions use `error`) and always
meet 4.5:1 on `background`; `text` meets your `--contrast` target; `dim`
meets 3.0:1; ANSI normals meet 4.5:1 and brights 3.0:1 — so shells bind them
directly instead of deriving colors client-side.

Grayscale in, grayscale out: seeds below ~0.015 chroma keep ~zero chroma in
every tonal role (no invented tints). Status and ANSI roles keep the scheme's
minimum chroma so they stay distinguishable — a deliberate, documented
exception, not a leak.

The daemon also writes a `theme.json` snapshot of the current theme to
`~/.cache/skein/theme.json` on every change, for tools that prefer a file over the socket.

Built-in targets: `colors.css` (plain CSS vars), `colors.json`, `alacritty.toml`,
`kitty.conf`, `hyprland.conf`, `hyprland.lua` (live border colors, sourced via
a guarded `dofile`), `hyprlock.conf` (`source`-able `$skein_*` variables),
`rofi.rasi`, `mako`, `dunst.conf` (drop-in for `dunstrc.d/`), `swaync.css`,
`waybar.css`, `wlogout.css`, `gtk.css`, `cava.conf`, `foot.ini`,
`swaylock.conf`, `ghostty.conf`, `fuzzel.ini`, `starship.toml`, `btop.theme`,
`fish-theme.fish`, `fzf.fish`, `zellij.kdl`, `tmux.conf`, `yazi-theme.toml`,
`tide-theme.fish` (structural prompt colors via universal vars),
`skein.theme.css` (Vencord/Vesktop theme), `skein-color-theme.json` (VS Code
theme + terminal + syntax tokens), `skein.lua` (Neovim colorscheme),
`zen-userChrome.css` (Zen Browser chrome variables).

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
