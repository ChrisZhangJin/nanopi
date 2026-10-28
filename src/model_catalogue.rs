//! User-editable model catalogue: `<nanopi_home>/models.json`.
//!
//! The compiled-in table (`models::BUILTIN_MODELS`) ships inside the
//! binary, so a model released after the build is invisible — its
//! context window reads `?/128.0k` and auto-compaction falls back to a
//! flat char threshold instead of the token-vs-window check. On a box
//! with no connectivity, which is nanopi's whole premise, there was no
//! way to fix that short of a rebuild.
//!
//! This module materialises the defaults to disk as a readable,
//! editable file and merges any additions back over them.
//!
//! **Why two blocks.** `builtin` is regenerated whenever the binary's
//! version changes; `custom` is never touched. A single flat list would
//! shadow the compiled table forever — the next nanopi would ship newer
//! models and the on-disk copy would quietly win, which is the exact
//! staleness this file exists to remove.
//!
//! **Global only.** Deliberately not `<cwd>/.nanopi/models.json`:
//! creating any file under a project's `.nanopi/` flips that cwd from
//! `NoProjectResources` to `NeedsPrompt` in `crate::trust`, so a
//! project-scoped catalogue would silently change the trust posture of
//! every repo that adopted it. A context window is not worth that.
//!
//! Nothing here aborts startup. Only `config.toml` and `settings.toml`
//! are fatal when malformed; a bad `models.json` degrades to the
//! compiled table and says so.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::models::ModelInfo;

/// One entry as it appears on disk. Mirrors [`ModelInfo`], but owned —
/// the file is read at runtime, so these cannot be `&'static str`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnedModel {
    pub id: String,
    pub vendor: String,
    pub context_window: u32,
}

/// The on-disk shape of `models.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogueFile {
    /// Binary version that last wrote `builtin`. Drives regeneration.
    #[serde(default)]
    pub generated_by: String,
    /// Human-facing note; round-tripped so a hand edit survives.
    #[serde(default, rename = "_comment", skip_serializing_if = "String::is_empty")]
    pub comment: String,
    /// User-owned. Never rewritten by nanopi. Wins over `builtin`.
    #[serde(default)]
    pub custom: Vec<OwnedModel>,
    /// Mirror of the compiled table, regenerated on version change.
    #[serde(default)]
    pub builtin: Vec<OwnedModel>,
}

const COMMENT: &str = "`builtin` is regenerated when nanopi's version changes (the previous file is \
saved alongside as models.json.bak). Put your own entries in `custom` — those are never touched, \
and they win over `builtin` when the id matches.";

/// A problem that did not stop startup. Mirrors `resources::SkillDiagnostic`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogueDiagnostic {
    pub message: String,
    pub path: PathBuf,
}

/// Outcome of [`load`]: the merged catalogue plus anything worth saying.
#[derive(Debug, Default)]
pub struct LoadResult {
    /// `None` when nothing was loaded and the compiled table stands.
    pub merged: Option<Vec<ModelInfo>>,
    pub diagnostics: Vec<CatalogueDiagnostic>,
}

/// Merge `custom` and file-`builtin` over the compiled table.
///
/// Precedence by id, case-insensitively: `custom` > file `builtin` >
/// compiled. The result is re-sorted LONGEST ID FIRST, which is load
/// bearing — `models::lookup` is a linear `starts_with` scan, so
/// first-match-wins equals longest-prefix-wins only while that holds.
/// Without the re-sort, a user adding `gpt-5` would shadow the built-in
/// `gpt-5.4` for every session.
///
/// Pure, so it is testable without touching the process-global
/// `OnceLock` that [`crate::models::install`] owns.
pub fn merge(compiled: &[ModelInfo], file: &CatalogueFile) -> Vec<ModelInfo> {
    // Walk lowest-precedence first and let later inserts overwrite.
    let mut by_id: std::collections::HashMap<String, ModelInfo> = std::collections::HashMap::new();
    for m in compiled {
        by_id.insert(m.id.to_ascii_lowercase(), *m);
    }
    for src in [&file.builtin, &file.custom] {
        for m in src {
            if m.id.trim().is_empty() || m.context_window == 0 {
                continue;
            }
            by_id.insert(
                m.id.to_ascii_lowercase(),
                ModelInfo {
                    // Leaked deliberately: the catalogue lives for the
                    // whole process, and `ModelInfo` is `&'static str`
                    // because two call sites depend on that lifetime.
                    id: Box::leak(m.id.clone().into_boxed_str()),
                    vendor: Box::leak(m.vendor.clone().into_boxed_str()),
                    context_window: m.context_window,
                },
            );
        }
    }
    let mut out: Vec<ModelInfo> = by_id.into_values().collect();
    out.sort_by(|a, b| b.id.len().cmp(&a.id.len()).then_with(|| a.id.cmp(b.id)));
    out
}

fn to_owned(models: &[ModelInfo]) -> Vec<OwnedModel> {
    models
        .iter()
        .map(|m| OwnedModel {
            id: m.id.to_string(),
            vendor: m.vendor.to_string(),
            context_window: m.context_window,
        })
        .collect()
}

fn fresh_file(compiled: &[ModelInfo], version: &str, custom: Vec<OwnedModel>) -> CatalogueFile {
    CatalogueFile {
        generated_by: version.to_string(),
        comment: COMMENT.to_string(),
        custom,
        builtin: to_owned(compiled),
    }
}

/// Serialize to `path` atomically: temp file in the same directory,
/// then rename over the target. Same approach as `wasm::store`, and for
/// the same reason — a torn write must not brick the next startup.
fn write_atomic(path: &Path, file: &CatalogueFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(file)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = path.with_extension("json.tmp");
    match std::fs::write(&tmp, text.as_bytes()).and_then(|_| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Load `path`, writing or refreshing it as needed, and return the
/// merged catalogue.
///
/// - absent → write it, return the compiled table merged with nothing
/// - `generated_by` matches `version` → load as-is, no write
/// - `generated_by` differs → back up to `models.json.bak`, rewrite
///   `builtin`, preserve `custom` verbatim
/// - unparseable → **never overwrite**, diagnose, fall back
///
/// `allow_write` is false for print mode: a scripted run must not
/// create files as a side effect. It still reads an existing file.
pub fn load(path: &Path, compiled: &'static [ModelInfo], version: &str, allow_write: bool)
    -> LoadResult
{
    let mut diags = Vec::new();

    let raw = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            diags.push(CatalogueDiagnostic {
                message: format!("could not read ({e}); using the built-in catalogue"),
                path: path.to_path_buf(),
            });
            return LoadResult { merged: None, diagnostics: diags };
        }
    };

    let Some(text) = raw else {
        if allow_write {
            if let Err(e) = write_atomic(path, &fresh_file(compiled, version, Vec::new())) {
                diags.push(CatalogueDiagnostic {
                    message: format!("could not write ({e}); using the built-in catalogue"),
                    path: path.to_path_buf(),
                });
            }
        }
        return LoadResult { merged: None, diagnostics: diags };
    };

    let parsed: CatalogueFile = match serde_json::from_str(&text) {
        Ok(f) => f,
        Err(e) => {
            // Refusing to clobber a file we cannot read is what keeps a
            // typo from eating someone's additions.
            diags.push(CatalogueDiagnostic {
                message: format!(
                    "invalid JSON at line {} ({e}); leaving it untouched and using the \
                     built-in catalogue",
                    e.line()
                ),
                path: path.to_path_buf(),
            });
            return LoadResult { merged: None, diagnostics: diags };
        }
    };

    if allow_write && parsed.generated_by != version {
        let bak = path.with_extension("json.bak");
        match std::fs::copy(path, &bak) {
            Ok(_) => {
                let refreshed = fresh_file(compiled, version, parsed.custom.clone());
                match write_atomic(path, &refreshed) {
                    Ok(()) => diags.push(CatalogueDiagnostic {
                        message: format!(
                            "refreshed built-in models for v{version} (previous file saved to \
                             {}); your {} custom {} kept",
                            bak.display(),
                            parsed.custom.len(),
                            if parsed.custom.len() == 1 { "entry was" } else { "entries were" }
                        ),
                        path: path.to_path_buf(),
                    }),
                    Err(e) => diags.push(CatalogueDiagnostic {
                        message: format!("could not refresh ({e}); keeping the existing file"),
                        path: path.to_path_buf(),
                    }),
                }
            }
            Err(e) => diags.push(CatalogueDiagnostic {
                message: format!("could not back up before refresh ({e}); left unchanged"),
                path: path.to_path_buf(),
            }),
        }
    }

    LoadResult { merged: Some(merge(compiled, &parsed)), diagnostics: diags }
}

/// Print load diagnostics, mirroring `build::print_skill_diagnostics`.
pub fn print_diagnostics(diags: &[CatalogueDiagnostic]) {
    for d in diags {
        crate::note!("nanopi: models.json: {} ({})", d.message, d.path.display());
    }
}

/// Load the global catalogue and install it for the process.
///
/// The one call startup makes. A `None` path (no resolvable home) is
/// not an error — the compiled table stands.
pub fn init(allow_write: bool) {
    let Some(path) = crate::paths::global_models_path() else {
        return;
    };
    let res = load(&path, crate::models::builtin(), env!("CARGO_PKG_VERSION"), allow_write);
    print_diagnostics(&res.diagnostics);
    if let Some(merged) = res.merged {
        crate::models::install(Box::leak(merged.into_boxed_slice()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compiled() -> &'static [ModelInfo] {
        &[
            ModelInfo { id: "gpt-5.4", vendor: "openai", context_window: 1_050_000 },
            ModelInfo { id: "gpt-5", vendor: "openai", context_window: 400_000 },
        ]
    }

    fn owned(id: &str, vendor: &str, w: u32) -> OwnedModel {
        OwnedModel { id: id.into(), vendor: vendor.into(), context_window: w }
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("nanopi-cat-{}", crate::util::uuid::v7()));
        std::fs::create_dir_all(&d).unwrap();
        d.join("models.json")
    }

    #[test]
    fn custom_overrides_builtin_and_adds_unknown() {
        let file = CatalogueFile {
            custom: vec![owned("gpt-5", "openai", 123), owned("brand-new", "openai", 999)],
            ..Default::default()
        };
        let merged = merge(compiled(), &file);
        let find = |id: &str| merged.iter().find(|m| m.id == id).map(|m| m.context_window);
        assert_eq!(find("gpt-5"), Some(123), "custom must win by id");
        assert_eq!(find("brand-new"), Some(999), "custom must add unknown ids");
        assert_eq!(find("gpt-5.4"), Some(1_050_000), "untouched entry must survive");
    }

    /// `lookup` is a linear `starts_with` scan, so the merged catalogue
    /// MUST stay longest-id-first. Otherwise a user adding `gpt-5`
    /// shadows the built-in `gpt-5.4` for every session.
    #[test]
    fn merge_restores_longest_id_first_ordering() {
        let file = CatalogueFile {
            custom: vec![owned("a", "openai", 1), owned("a-very-long-model-id", "openai", 2)],
            ..Default::default()
        };
        let merged = merge(compiled(), &file);
        for pair in merged.windows(2) {
            assert!(
                pair[0].id.len() >= pair[1].id.len(),
                "ordering broken: {:?} before {:?}",
                pair[0].id,
                pair[1].id
            );
        }
    }

    #[test]
    fn blank_or_zero_entries_are_ignored() {
        let file = CatalogueFile {
            custom: vec![owned("  ", "openai", 5), owned("zero", "openai", 0)],
            ..Default::default()
        };
        let merged = merge(compiled(), &file);
        assert!(!merged.iter().any(|m| m.id.trim().is_empty() || m.context_window == 0));
        assert!(!merged.iter().any(|m| m.id == "zero"));
    }

    #[test]
    fn absent_file_is_written_with_the_builtin_table() {
        let p = tmp();
        let res = load(&p, compiled(), "1.0.0", true);
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
        assert!(res.merged.is_none(), "nothing to merge on a fresh write");
        let f: CatalogueFile = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(f.generated_by, "1.0.0");
        assert_eq!(f.builtin.len(), 2);
        assert!(f.custom.is_empty());
        assert!(f.comment.contains("custom"));
    }

    #[test]
    fn print_mode_never_creates_the_file() {
        let p = tmp();
        let res = load(&p, compiled(), "1.0.0", false);
        assert!(!p.exists(), "a scripted -p run must not create files");
        assert!(res.merged.is_none());
        assert!(res.diagnostics.is_empty());
    }

    #[test]
    fn same_version_leaves_the_file_byte_identical() {
        let p = tmp();
        load(&p, compiled(), "1.0.0", true);
        let before = std::fs::read_to_string(&p).unwrap();
        let res = load(&p, compiled(), "1.0.0", true);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), before, "must not rewrite");
        assert!(res.merged.is_some());
        assert!(res.diagnostics.is_empty(), "{:?}", res.diagnostics);
    }

    /// The upgrade path: new built-ins land, the user's own entries
    /// survive, and the old file is recoverable.
    #[test]
    fn version_bump_backs_up_and_preserves_custom() {
        let p = tmp();
        load(&p, compiled(), "1.0.0", true);
        let mut f: CatalogueFile =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        f.custom.push(owned("my-model", "openai", 7));
        std::fs::write(&p, serde_json::to_string_pretty(&f).unwrap()).unwrap();

        let newer: &'static [ModelInfo] = &[
            ModelInfo { id: "gpt-5.4", vendor: "openai", context_window: 1_050_000 },
            ModelInfo { id: "gpt-5", vendor: "openai", context_window: 400_000 },
            ModelInfo { id: "gpt-6", vendor: "openai", context_window: 2_000_000 },
        ];
        let res = load(&p, newer, "2.0.0", true);

        let after: CatalogueFile =
            serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(after.generated_by, "2.0.0");
        assert_eq!(after.builtin.len(), 3, "new built-ins must land");
        assert_eq!(after.custom, vec![owned("my-model", "openai", 7)], "custom preserved verbatim");

        let bak: CatalogueFile =
            serde_json::from_str(&std::fs::read_to_string(p.with_extension("json.bak")).unwrap())
                .unwrap();
        assert_eq!(bak.generated_by, "1.0.0", "previous file recoverable");

        assert_eq!(res.diagnostics.len(), 1, "the refresh must be announced");
        let merged = res.merged.unwrap();
        assert!(merged.iter().any(|m| m.id == "gpt-6"));
        assert!(merged.iter().any(|m| m.id == "my-model"));
    }

    /// A typo must never cost the user their additions.
    #[test]
    fn malformed_file_is_left_untouched_and_falls_back() {
        let p = tmp();
        std::fs::write(&p, "{ not json at all ").unwrap();
        let res = load(&p, compiled(), "1.0.0", true);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "{ not json at all ");
        assert!(!p.with_extension("json.bak").exists(), "no backup of a file we refused to touch");
        assert!(res.merged.is_none(), "must fall back to the compiled table");
        assert_eq!(res.diagnostics.len(), 1);
        assert!(res.diagnostics[0].message.contains("invalid JSON"), "{:?}", res.diagnostics);
    }
}
