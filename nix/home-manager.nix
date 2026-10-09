{ config, lib, pkgs, ... }:

with lib;

let
  cfg = config.services.skein;
  tomlFormat = pkgs.formats.toml { };
in {
  options.services.skein = {
    enable = mkEnableOption "skein wallpaper theme daemon";

    package = mkOption {
      type = types.package;
      description = "skein package to use (defaulted to this flake's build by the module wrapper).";
    };

    mode = mkOption {
      type = types.enum [ "dark" "light" "auto" ];
      default = "auto";
      description = "Color mode.";
    };

    scheme = mkOption {
      type = types.enum [ "vibrant" "tonal" "muted" ];
      default = "tonal";
      description = "Palette scheme.";
    };

    seed = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Pin the seed color (\"#RRGGBB\") instead of extracting from the wallpaper.";
    };

    contrast = mkOption {
      type = types.nullOr types.float;
      default = null;
      description = "Minimum text contrast ratio (default 4.5).";
    };

    wallpaperDir = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Directory of wallpapers to watch. A string so the daemon can expand ~ itself (a Nix path would be copied to the store).";
    };

    templateDir = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Directory of *.jinja templates. String; ~ is expanded by the daemon.";
    };

    outputDir = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Directory rendered templates are written to. String; ~ is expanded by the daemon.";
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."skein/config.toml".source = tomlFormat.generate "config.toml" (
      filterAttrs (_: v: v != null) {
        mode = cfg.mode;
        scheme = cfg.scheme;
        seed = cfg.seed;
        contrast = cfg.contrast;
        wallpaper_dir = cfg.wallpaperDir;
        template_dir = cfg.templateDir;
        output_dir = cfg.outputDir;
      }
    );

    systemd.user.services.skeind = {
      Unit = {
        Description = "skein wallpaper theme daemon";
        After = [ "graphical-session.target" ];
        Requires = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/skeind";
        Restart = "on-failure";
      };
      Install = {
        WantedBy = [ "graphical-session.target" ];
      };
    };
  };
}
