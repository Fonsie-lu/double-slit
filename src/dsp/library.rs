//! User presets: patch files (SPEC 5.1 JSON, the format `state::save_patch`
//! writes) in one folder, listed after the factory presets. A preset's name is
//! its file stem, so listing the folder never has to read the patches, which
//! can carry ~180 kB of embedded wavetable each.
//!
//! Import merges files into the folder instead of replacing anything: a new
//! name is added, a patch that is already there is skipped, and a different
//! patch under a taken name (a user or factory preset) gets a numbered name.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::state::{load_patch_data, PatchData};

#[derive(Clone, Debug, PartialEq)]
pub struct UserPreset {
    pub name: String,
    pub path: PathBuf,
}

/// The `.json` files in `dir`, sorted by name (case-insensitive). A missing
/// folder is an empty library.
pub fn list(dir: &Path) -> Vec<UserPreset> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<UserPreset> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .filter_map(|path| {
            let name = path.file_stem()?.to_string_lossy().into_owned();
            Some(UserPreset { name, path })
        })
        .collect();
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Reads one patch file. The name is the file stem, which is what the list
/// shows even if the file was renamed after it was saved.
pub fn load(path: &Path) -> Option<PatchData> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut data = load_patch_data(&text)?;
    if let Some(stem) = path.file_stem() {
        data.info.name = stem.to_string_lossy().into_owned();
    }
    Some(data)
}

/// A preset name that is safe as a file stem: no path separators or control
/// characters, no leading dot, at most 64 characters.
pub fn file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c == '/' || c == '\\' || c.is_control() { '-' } else { c })
        .take(64)
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "Preset".to_owned()
    } else {
        cleaned.to_owned()
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct ImportReport {
    /// Names the imported presets were stored under.
    pub added: Vec<String>,
    /// Files identical to a preset already in the library.
    pub already_there: usize,
    /// Files that could not be read or are not patches.
    pub failed: Vec<String>,
}

impl ImportReport {
    /// One line for the editor.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("IMPORTED {}", self.added.len())];
        if self.already_there > 0 {
            parts.push(format!("{} ALREADY THERE", self.already_there));
        }
        if !self.failed.is_empty() {
            parts.push(format!("{} NOT A PATCH", self.failed.len()));
        }
        parts.join(", ")
    }
}

/// Merges patch files into the library in `dir` (created on demand).
/// `reserved` are names that must not be reused, the factory presets.
pub fn import(dir: &Path, files: &[PathBuf], reserved: &[&str]) -> ImportReport {
    let mut report = ImportReport::default();
    if std::fs::create_dir_all(dir).is_err() {
        report.failed = files.iter().map(|f| f.display().to_string()).collect();
        return report;
    }
    for file in files {
        let shown = file.display().to_string();
        let Some(mut root) = std::fs::read_to_string(file)
            .ok()
            .filter(|text| load_patch_data(text).is_some())
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        else {
            report.failed.push(shown);
            continue;
        };
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // The patch's own name, unless it is the default one a never-named
        // patch carries; then the file name says more.
        let own = root["name"].as_str().filter(|n| !n.trim().is_empty() && *n != "Init").map(str::to_owned);
        let base = file_name(&own.unwrap_or(stem));

        let existing = list(dir);
        if existing.iter().any(|p| same_patch(&p.path, &root)) {
            report.already_there += 1;
            continue;
        }
        let taken = |n: &str| {
            reserved.iter().any(|r| r.eq_ignore_ascii_case(n)) || existing.iter().any(|p| p.name.eq_ignore_ascii_case(n))
        };
        let mut name = base.clone();
        let mut k = 2;
        while taken(&name) {
            name = format!("{base} {k}");
            k += 1;
        }

        root["name"] = Value::String(name.clone());
        match write_patch(dir, &name, &serde_json::to_string_pretty(&root).unwrap_or_default()) {
            Ok(_) => report.added.push(name),
            Err(_) => report.failed.push(shown),
        }
    }
    report
}

/// Writes patch JSON text as `<dir>/<name>.json` through a temporary file, so
/// a failed write never leaves a truncated preset behind.
pub fn write_patch(dir: &Path, name: &str, json: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", file_name(name)));
    let tmp = dir.join(format!(".{}.json.tmp", file_name(name)));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Whether the file at `path` holds the same patch as `root`, ignoring the name.
fn same_patch(path: &Path, root: &Value) -> bool {
    let Some(mut other) = std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else {
        return false;
    };
    let mut root = root.clone();
    root["name"] = Value::Null;
    other["name"] = Value::Null;
    root == other
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dsp::params::NUM_PARAMS;
    use crate::dsp::presets::build_factory_preset;
    use crate::dsp::shared::SynthShared;
    use crate::dsp::state::{apply_patch_data, save_patch, PatchInfo};

    /// A fresh folder per test; tests run in parallel.
    fn scratch(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("double-slit-library-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Factory preset `index` as patch JSON named `name`.
    fn patch_json(index: usize, name: &str) -> String {
        let shared = SynthShared::default();
        let params: [f32; NUM_PARAMS] = apply_patch_data(&build_factory_preset(index).unwrap(), &shared);
        let info = PatchInfo { name: name.to_owned(), ..PatchInfo::default() };
        save_patch(&shared, Some(&params), &info, true)
    }

    fn file(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn import_merges_instead_of_replacing() {
        let root = scratch("merge");
        let (lib, src) = (root.join("lib"), root.join("src"));
        std::fs::create_dir_all(&src).unwrap();
        let bass = file(&src, "a.json", &patch_json(3, "My Bass"));
        let lead = file(&src, "b.json", &patch_json(11, "My Lead"));

        let first = import(&lib, &[bass.clone(), lead], &[]);
        assert_eq!(first.added, vec!["My Bass", "My Lead"]);
        let names: Vec<String> = list(&lib).into_iter().map(|p| p.name).collect();
        assert_eq!(names, vec!["My Bass", "My Lead"]);

        // The same patch again is recognised, under any file or patch name.
        let again = import(&lib, &[bass], &[]);
        assert_eq!(again.added.len(), 0);
        assert_eq!(again.already_there, 1);
        let renamed = file(&src, "c.json", &patch_json(3, "Other Name"));
        assert_eq!(import(&lib, &[renamed], &[]).already_there, 1);

        // A different patch under a taken name keeps both.
        let other = file(&src, "d.json", &patch_json(5, "My Bass"));
        assert_eq!(import(&lib, &[other], &[]).added, vec!["My Bass 2"]);
        assert_eq!(list(&lib).len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn import_keeps_factory_names_free_and_reports_failures() {
        let root = scratch("reserved");
        let lib = root.join("lib");
        let saw = file(&root, "saw.json", &patch_json(0, "Init Saw"));
        let junk = file(&root, "junk.json", "not json");
        let report = import(&lib, &[saw, junk.clone(), root.join("missing.json")], &["Init Saw"]);
        assert_eq!(report.added, vec!["Init Saw 2"]);
        assert_eq!(report.failed.len(), 2);
        assert_eq!(report.summary(), "IMPORTED 1, 2 NOT A PATCH");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unnamed_patches_take_the_file_name() {
        let root = scratch("unnamed");
        let lib = root.join("lib");
        let init = file(&root, "Warm Pad.json", &patch_json(2, "Init"));
        assert_eq!(import(&lib, &[init], &[]).added, vec!["Warm Pad"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exported_patch_loads_back_identically() {
        let root = scratch("roundtrip");
        let index = 13; // multi-frame table and matrix rows
        let path = write_patch(&root, "Round/Trip", &patch_json(index, "Round/Trip")).unwrap();
        assert_eq!(path.file_name().unwrap(), "Round-Trip.json");
        let loaded = load(&path).unwrap();
        let built = build_factory_preset(index).unwrap();
        assert_eq!(loaded.info.name, "Round-Trip");
        assert_eq!(loaded.params, built.params);
        assert_eq!(loaded.rows.len(), built.rows.len());
        assert_eq!(loaded.tables[0].as_ref().unwrap().factory_index(), built.tables[0].as_ref().unwrap().factory_index());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("a/b\\c"), "a-b-c");
        assert_eq!(file_name("..hidden"), "hidden");
        assert_eq!(file_name("   "), "Preset");
        assert_eq!(file_name(&"x".repeat(100)).len(), 64);
    }
}
