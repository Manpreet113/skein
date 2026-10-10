// =============================================================================
// deploy.rs — Opt-in placement of rendered templates into live locations
// =============================================================================
//
// Renders always land in the cache output dir first. When the user opts in
// via `[deploy]` in config.toml, this module additionally symlinks selected
// outputs to where their apps actually read them (e.g. fish `conf.d`,
// yazi `theme.toml`, btop `themes/`).
//
// Safety rules (the whole point of this module):
//   • Nothing happens unless `deploy.enable = true`.
//   • Symlinks only — one source of truth (the cache file), atomic with the
//     existing tmp+rename writes, trivially detectable and reversible.
//   • An existing non-skein file is never silently destroyed: depending on
//     `on_conflict` it is backed up once (`<dest>.skein-bak`), overwritten
//     with a warning, or skipped with a warning. Directories are never
//     touched.
//   • Every pass is per-target isolated: one bad entry warns and the rest
//     still apply. Deploy never aborts a render pass and never kills the
//     daemon.
// =============================================================================

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use tracing::{debug, info, warn};

/// What to do when the destination already exists and is not a symlink we
/// manage. Parsed leniently from config (unknown values warn and fall back
/// to `Backup` rather than killing the daemon).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConflictPolicy {
    /// Rename the existing file to `<dest>.skein-bak` once, then link.
    #[default]
    Backup,
    /// Remove the existing file (with a warning), then link.
    Overwrite,
    /// Leave the existing file alone (with a warning).
    Skip,
}

impl ConflictPolicy {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "overwrite" => Self::Overwrite,
            "skip" => Self::Skip,
            "backup" => Self::Backup,
            other => {
                warn!(value = %other, "Unknown deploy.on_conflict; falling back to \"backup\"");
                Self::Backup
            }
        }
    }
}

/// The `[deploy]` section of config.toml. Everything optional; absent
/// section (or `enable = false`) means cache-only operation.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeployFileConfig {
    /// Master switch. Default false: nothing is placed outside the cache
    /// unless the user explicitly opts in.
    #[serde(default)]
    pub enable: bool,
    /// `backup` | `overwrite` | `skip`. Default `backup`.
    #[serde(default)]
    pub on_conflict: Option<String>,
    /// Map of rendered output filename → live destination, e.g.
    /// `"yazi-theme.toml" = "~/.config/yazi/theme.toml"`.
    /// Keys are output filenames as seen in the cache dir (no `.jinja`).
    #[serde(default)]
    pub targets: HashMap<String, PathBuf>,
}

/// A resolved, ready-to-apply deploy plan (`~` already expanded).
#[derive(Debug, Clone, Default)]
pub struct DeployPlan {
    pub enable: bool,
    pub on_conflict: ConflictPolicy,
    /// `(output filename, live destination)` pairs.
    pub targets: Vec<(String, PathBuf)>,
}

impl DeployPlan {
    /// Resolve user config into a plan. Never fails: unknown policy values
    /// warn and fall back inside [`ConflictPolicy::parse`].
    pub fn resolve(cfg: &DeployFileConfig) -> Self {
        let on_conflict = cfg
            .on_conflict
            .as_deref()
            .map(ConflictPolicy::parse)
            .unwrap_or_default();
        let targets = cfg
            .targets
            .iter()
            .map(|(name, dest)| (normalize_name(name), expand_tilde(dest.clone())))
            .collect();
        Self {
            enable: cfg.enable,
            on_conflict,
            targets,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enable && !self.targets.is_empty()
    }
}

/// Outcome summary of a deploy pass, for CLI feedback and tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeployReport {
    pub linked: usize,
    pub skipped: usize,
    pub backed_up: usize,
}

/// Suffix appended for conflict backups (`<dest>.skein-bak`).
pub const BACKUP_SUFFIX: &str = ".skein-bak";

/// State file recording which destinations skein manages (reserved for a
/// future `undeploy`; written on every pass that links anything).
pub const STATE_FILE: &str = "deployed.json";

/// Normalize a user-supplied template key to an output filename:
/// trim whitespace and tolerate a trailing `.jinja`.
fn normalize_name(name: &str) -> String {
    name.trim().trim_end_matches(".jinja").to_owned()
}

/// Expand a leading `~` or `~/` to the user's home directory.
pub fn expand_tilde(p: PathBuf) -> PathBuf {
    match p.to_str() {
        Some("~") => dirs_next::home_dir().unwrap_or(p),
        Some(s) if s.starts_with("~/") => match dirs_next::home_dir() {
            Some(home) => home.join(&s[2..]),
            None => p,
        },
        _ => p,
    }
}

/// Ensure every target in `plan` is symlinked from the rendered output in
/// `output_dir` to its live destination. Never fails hard: per-target
/// problems are warnings; the returned report tells the caller what
/// happened.
pub async fn ensure_deploy(output_dir: &Path, plan: &DeployPlan) -> DeployReport {
    let mut report = DeployReport::default();

    if !plan.enabled() {
        return report;
    }

    let mut managed: HashMap<String, ManagedEntry> = HashMap::new();

    for (name, dest) in &plan.targets {
        let src = output_dir.join(name);
        if !src.is_file() {
            warn!(template = %name, "Deploy target has no rendered output; skipping (is it in `targets`?)");
            report.skipped += 1;
            continue;
        }
        if dest == &src {
            warn!(path = %dest.display(), "Deploy destination is the rendered file itself; skipping");
            report.skipped += 1;
            continue;
        }

        match ensure_link(&src, dest, plan.on_conflict).await {
            LinkOutcome::Linked { backed_up } => {
                info!(from = %src.display(), to = %dest.display(), "Deployed template output");
                report.linked += 1;
                if backed_up {
                    report.backed_up += 1;
                }
                managed.insert(
                    dest.display().to_string(),
                    ManagedEntry {
                        template: name.clone(),
                        backup: backed_up.then(|| backup_path(dest)),
                    },
                );
            }
            LinkOutcome::Skipped => {
                report.skipped += 1;
            }
        }
    }

    if report.linked > 0 {
        write_state(output_dir, &managed).await;
    }

    report
}

#[derive(Debug, Clone, serde::Serialize)]
struct ManagedEntry {
    template: String,
    backup: Option<PathBuf>,
}

async fn write_state(output_dir: &Path, managed: &HashMap<String, ManagedEntry>) {
    let path = output_dir.join(STATE_FILE);
    match serde_json::to_string_pretty(managed) {
        Ok(json) => {
            let tmp = path.with_extension("skein.tmp");
            if tokio::fs::write(&tmp, &json).await.is_ok() {
                let _ = tokio::fs::rename(&tmp, &path).await;
            }
        }
        Err(e) => warn!(error = %e, "Failed to serialize deploy state"),
    }
    debug!(entries = managed.len(), "Deploy state written");
}

fn backup_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(BACKUP_SUFFIX);
    dest.with_file_name(name)
}

enum LinkOutcome {
    Linked { backed_up: bool },
    Skipped,
}

/// Bring a single destination into the desired state. Directories are never
/// removed or replaced.
async fn ensure_link(src: &Path, dest: &Path, policy: ConflictPolicy) -> LinkOutcome {
    // Already managed: symlink pointing exactly at our rendered file.
    if let Ok(current) = tokio::fs::read_link(dest).await {
        if current == src {
            return LinkOutcome::Linked { backed_up: false };
        }
        // Same output filename, different directory: almost certainly our
        // own link from when another output dir was configured. Refresh it
        // without a backup — the target was skein-managed either way.
        if current.file_name().is_some_and(|n| Some(n) == src.file_name()) {
            if tokio::fs::remove_file(dest).await.is_ok() {
                return link_or_skip(src, dest, false);
            }
            warn!(path = %dest.display(), "Cannot refresh deploy symlink; skipping");
            return LinkOutcome::Skipped;
        }
        // A symlink pointing elsewhere is user-owned: resolve via policy.
        return match replace_path(dest, policy).await {
            Some(backed_up) => link_or_skip(src, dest, backed_up),
            None => LinkOutcome::Skipped,
        };
    }

    match tokio::fs::symlink_metadata(dest).await {
        // Destination missing: ensure parents, then link.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = dest.parent() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    warn!(path = %dest.display(), error = %e, "Cannot create deploy parent dir; skipping");
                    return LinkOutcome::Skipped;
                }
            }
            link_or_skip(src, dest, false)
        }
        Err(e) => {
            warn!(path = %dest.display(), error = %e, "Cannot stat deploy destination; skipping");
            LinkOutcome::Skipped
        }
        Ok(meta) => {
            if meta.is_dir() {
                warn!(path = %dest.display(), "Deploy destination is a directory; refusing to touch it");
                return LinkOutcome::Skipped;
            }
            match replace_path(dest, policy).await {
                Some(backed_up) => link_or_skip(src, dest, backed_up),
                None => LinkOutcome::Skipped,
            }
        }
    }
}

fn link_or_skip(src: &Path, dest: &Path, backed_up: bool) -> LinkOutcome {
    match link(src, dest) {
        Ok(()) => LinkOutcome::Linked { backed_up },
        Err(_) => LinkOutcome::Skipped,
    }
}

/// Apply the conflict policy to an existing non-managed path. Returns
/// whether a backup was taken (`None` = skip).
async fn replace_path(dest: &Path, policy: ConflictPolicy) -> Option<bool> {
    match policy {
        ConflictPolicy::Skip => {
            warn!(path = %dest.display(), "Deploy destination exists and is not skein-managed; skipping (on_conflict = skip)");
            None
        }
        ConflictPolicy::Overwrite => {
            if let Err(e) = tokio::fs::remove_file(dest).await {
                warn!(path = %dest.display(), error = %e, "Cannot remove deploy destination; skipping");
                return None;
            }
            warn!(path = %dest.display(), "Removed existing file for deploy (on_conflict = overwrite)");
            Some(false)
        }
        ConflictPolicy::Backup => {
            let bak = backup_path(dest);
            if bak.exists() {
                // A backup already exists: keep the old backup, replace.
                warn!(path = %dest.display(), backup = %bak.display(), "Backup already exists; replacing destination without a new backup");
                if tokio::fs::remove_file(dest).await.is_err() {
                    warn!(path = %dest.display(), "Cannot remove deploy destination; skipping");
                    return None;
                }
                return Some(true);
            }
            if let Err(e) = tokio::fs::rename(dest, &bak).await {
                warn!(path = %dest.display(), error = %e, "Cannot back up deploy destination; skipping");
                return None;
            }
            info!(from = %dest.display(), to = %bak.display(), "Backed up existing file before deploy");
            Some(true)
        }
    }
}

fn link(src: &Path, dest: &Path) -> std::io::Result<()> {
    let res = std::os::unix::fs::symlink(src, dest);
    if let Err(e) = &res {
        warn!(from = %src.display(), to = %dest.display(), error = %e, "Failed to create deploy symlink");
    }
    res
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn render_src(out: &Path) {
        std::fs::write(out.join("theme.conf"), "color volunteered").unwrap();
    }

    #[tokio::test]
    async fn links_missing_destination_and_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        render_src(&out);
        let dest = tmp.path().join("deep/nested/theme.conf");
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Backup,
            targets: vec![("theme.conf".into(), dest.clone())],
        };

        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report, DeployReport { linked: 1, skipped: 0, backed_up: 0 });
        assert_eq!(std::fs::read_link(&dest).unwrap(), out.join("theme.conf"));
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "color volunteered");
    }

    #[tokio::test]
    async fn backs_up_existing_foreign_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        render_src(&out);
        let dest = dir.path().join("nested").join("theme.conf");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "user config").unwrap();
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Backup,
            targets: vec![("theme.conf".into(), dest.clone())],
        };

        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report.backed_up, 1);
        assert_eq!(report.linked, 1);
        let bak = backup_path(&dest);
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "user config");
        assert_eq!(std::fs::read_link(&dest).unwrap(), out.join("theme.conf"));

        // Second pass: already managed, idempotent with no new backup.
        let report2 = ensure_deploy(&out, &plan).await;
        assert_eq!(report2, DeployReport { linked: 1, skipped: 0, backed_up: 0 });
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "user config");
    }

    #[tokio::test]
    async fn existing_backup_is_kept_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        render_src(&out);
        let dest = dir.path().join("theme.conf");
        std::fs::write(&dest, "newer user config").unwrap();
        std::fs::write(backup_path(&dest), "original backup").unwrap();
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Backup,
            targets: vec![("theme.conf".into(), dest.clone())],
        };

        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report, DeployReport { linked: 1, skipped: 0, backed_up: 1 });
        // Old backup preserved; destination now managed.
        assert_eq!(
            std::fs::read_to_string(backup_path(&dest)).unwrap(),
            "original backup"
        );
        assert_eq!(std::fs::read_link(&dest).unwrap(), out.join("theme.conf"));
    }

    #[tokio::test]
    async fn refreshes_stale_managed_symlink_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let old_out = dir.path().join("old-out");
        std::fs::create_dir_all(&out).unwrap();
        std::fs::create_dir_all(&old_out).unwrap();
        render_src(&out);
        render_src(&old_out);
        // Link left behind by a previous output_dir configuration.
        let dest = dir.path().join("theme.conf");
        std::os::unix::fs::symlink(old_out.join("theme.conf"), &dest).unwrap();
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Backup,
            targets: vec![("theme.conf".into(), dest.clone())],
        };

        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report, DeployReport { linked: 1, skipped: 0, backed_up: 0 });
        assert_eq!(std::fs::read_link(&dest).unwrap(), out.join("theme.conf"));
        assert!(!backup_path(&dest).exists());
    }

    #[tokio::test]
    async fn skip_leaves_foreign_file_alone() {        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        render_src(&out);
        let dest = dir.path().join("theme.conf");
        std::fs::write(&dest, "user config").unwrap();
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Skip,
            targets: vec![("theme.conf".into(), dest.clone())],
        };

        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report, DeployReport { linked: 0, skipped: 1, backed_up: 0 });
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "user config");
    }

    #[tokio::test]
    async fn overwrite_replaces_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        render_src(&out);
        let dest = dir.path().join("theme.conf");
        std::fs::write(&dest, "user config").unwrap();
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Overwrite,
            targets: vec![("theme.conf".into(), dest.clone())],
        };

        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report, DeployReport { linked: 1, skipped: 0, backed_up: 0 });
        assert_eq!(std::fs::read_link(&dest).unwrap(), out.join("theme.conf"));
        assert!(!backup_path(&dest).exists());
    }

    #[tokio::test]
    async fn idempotent_when_already_linked() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        render_src(&out);
        let dest = dir.path().join("theme.conf");
        std::os::unix::fs::symlink(out.join("theme.conf"), &dest).unwrap();
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Backup,
            targets: vec![("theme.conf".into(), dest.clone())],
        };

        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report, DeployReport { linked: 1, skipped: 0, backed_up: 0 });
        assert!(!backup_path(&dest).exists());
    }

    #[tokio::test]
    async fn refuses_directories_and_missing_sources() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir_all(&out).unwrap();
        // Note: no rendered source file.
        let dest_dir = dir.path().join("somedir");
        std::fs::create_dir_all(&dest_dir).unwrap();
        let plan = DeployPlan {
            enable: true,
            on_conflict: ConflictPolicy::Overwrite,
            targets: vec![
                ("missing.conf".into(), dir.path().join("missing.conf")),
                ("theme.conf".into(), dest_dir.clone()),
            ],
        };

        // Render only theme.conf so the directory case is exercised.
        render_src(&out);
        let report = ensure_deploy(&out, &plan).await;

        assert_eq!(report.linked, 0);
        assert_eq!(report.skipped, 2);
        assert!(dest_dir.is_dir());
    }

    #[tokio::test]
    async fn disabled_plan_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let plan = DeployPlan {
            enable: false,
            on_conflict: ConflictPolicy::Backup,
            targets: vec![("theme.conf".into(), dir.path().join("theme.conf"))],
        };
        let report = ensure_deploy(dir.path(), &plan).await;
        assert_eq!(report, DeployReport::default());
        assert!(!dir.path().join("theme.conf").exists());
    }

    #[test]
    fn unknown_policy_falls_back_to_backup() {
        assert_eq!(ConflictPolicy::parse("OVERWRITE"), ConflictPolicy::Overwrite);
        assert_eq!(ConflictPolicy::parse("bogus"), ConflictPolicy::Backup);
    }
}
