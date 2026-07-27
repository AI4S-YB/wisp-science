//! Best-effort artifact provenance: snapshot the workspace around a producing
//! tool call and diff to learn which files it wrote and read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

const MAX_UNDO_TEXT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_UNDO_CAPTURE_BYTES: u64 = 32 * 1024 * 1024;

/// Reported to `Output::provenance` after a producing tool writes ≥1 file.
#[derive(Debug, Clone, Default)]
pub struct ProvenanceRecord {
    pub tool: String,
    pub language: String,
    pub source: String,
    pub output: String,
    pub success: bool,
    pub files_written: Vec<String>,
    pub files_read: Vec<String>,
    pub file_changes: Vec<ProvenanceFileChange>,
}

/// A bounded preimage for undoing one local text-file change.
#[derive(Debug, Clone, Default)]
pub struct ProvenanceFileChange {
    pub path: String,
    pub before_exists: bool,
    pub before_text: Option<String>,
    pub before_checksum: Option<String>,
    pub after_checksum: Option<String>,
    pub reversible: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TextPreimage {
    pub text: String,
    pub checksum: String,
}

const SKIP_DIRS: &[&str] = &[
    ".git",
    ".venv",
    "node_modules",
    ".wisp",
    "uploads",
    "__pycache__",
];
// ponytail: recursive mtime scan, capped + heavy dirs skipped. Swap for an fs-notify
// watcher only if this shows up in a profile.
const MAX_ENTRIES: usize = 20_000;

pub fn is_producing(tool: &str) -> bool {
    matches!(
        tool,
        "python" | "r" | "shell" | "write" | "edit" | "generate_image"
    )
}

pub fn language_of(tool: &str) -> String {
    match tool {
        "python" => "python",
        "r" => "r",
        "shell" => "bash",
        _ => "text",
    }
    .to_string()
}

pub fn source_of(tool: &str, args: &serde_json::Value) -> String {
    let key = match tool {
        "python" | "r" => "code",
        "write" | "edit" | "generate_image" => "path",
        _ => "cmd",
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Recursive path→mtime map of the workspace, skipping heavy dirs, capped.
pub fn snapshot(root: &Path) -> BTreeMap<PathBuf, SystemTime> {
    snapshot_capped(root, MAX_ENTRIES)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn path_identity(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        path.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        path
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_identity(left) == path_identity(right)
}

pub fn is_text_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "md" | "markdown"
            | "rmd"
            | "qmd"
            | "txt"
            | "csv"
            | "tsv"
            | "json"
            | "jsonl"
            | "yaml"
            | "yml"
            | "toml"
            | "xml"
            | "html"
            | "htm"
            | "css"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "jsx"
            | "rs"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "cxx"
            | "hpp"
            | "hxx"
            | "cs"
            | "go"
            | "java"
            | "kt"
            | "kts"
            | "swift"
            | "py"
            | "pyi"
            | "r"
            | "rb"
            | "php"
            | "lua"
            | "pl"
            | "pm"
            | "scala"
            | "clj"
            | "cljs"
            | "ex"
            | "exs"
            | "erl"
            | "hrl"
            | "fs"
            | "fsx"
            | "vb"
            | "vue"
            | "svelte"
            | "astro"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "ps1"
            | "bat"
            | "cmd"
            | "tex"
            | "bib"
            | "log"
            | "ini"
            | "cfg"
            | "conf"
            | "sql"
            | "ipynb"
    )
}

fn source_mentions_path(source: &str, root: &Path, path: &Path) -> bool {
    let source = source.replace('\\', "/");
    let absolute = path.to_string_lossy().replace('\\', "/");
    let relative = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    #[cfg(windows)]
    let source = source.to_ascii_lowercase();
    #[cfg(windows)]
    let absolute = absolute.to_ascii_lowercase();
    #[cfg(windows)]
    let relative = relative.to_ascii_lowercase();
    source.contains(&absolute) || source.contains(&relative)
}

/// Capture only bounded text files explicitly named by a producing tool.
///
/// New files need no preimage. Existing files whose destination is computed
/// dynamically remain visible in the undo preview but are not overwritten.
pub fn capture_text_preimages(
    before: &BTreeMap<PathBuf, SystemTime>,
    root: &Path,
    source: &str,
) -> BTreeMap<PathBuf, TextPreimage> {
    let mut out = BTreeMap::new();
    let mut captured = 0_u64;
    for path in before.keys() {
        if !is_text_path(path) || !source_mentions_path(source, root, path) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        let len = metadata.len();
        if len > MAX_UNDO_TEXT_BYTES
            || captured
                .checked_add(len)
                .is_none_or(|total| total > MAX_UNDO_CAPTURE_BYTES)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        captured += len;
        out.insert(
            path.clone(),
            TextPreimage {
                checksum: sha256_hex(text.as_bytes()),
                text,
            },
        );
    }
    out
}

fn read_current_text(path: &Path) -> Result<(String, String), &'static str> {
    let metadata = std::fs::metadata(path).map_err(|_| "file is no longer readable")?;
    if metadata.len() > MAX_UNDO_TEXT_BYTES {
        return Err("text file exceeds the 10 MiB undo limit");
    }
    let bytes = std::fs::read(path).map_err(|_| "file is no longer readable")?;
    let text = String::from_utf8(bytes).map_err(|_| "binary files are not supported yet")?;
    let checksum = sha256_hex(text.as_bytes());
    Ok((text, checksum))
}

/// Turn the provenance diff into bounded undo entries.
pub fn undo_file_changes(
    before: &BTreeMap<PathBuf, SystemTime>,
    root: &Path,
    written: &[String],
    preimages: &BTreeMap<PathBuf, TextPreimage>,
) -> Vec<ProvenanceFileChange> {
    written
        .iter()
        .map(|relative| {
            let requested_path = root.join(relative);
            let before_path = before
                .keys()
                .find(|path| same_path(path, &requested_path))
                .cloned();
            let before_exists = before_path.is_some();
            let path = before_path.as_deref().unwrap_or(&requested_path);
            if !is_text_path(path) {
                return ProvenanceFileChange {
                    path: relative.clone(),
                    before_exists,
                    reason: Some("binary files are not supported yet".into()),
                    ..Default::default()
                };
            }
            let (_, after_checksum) = match read_current_text(path) {
                Ok(current) => current,
                Err(reason) => {
                    return ProvenanceFileChange {
                        path: relative.clone(),
                        before_exists,
                        reason: Some(reason.into()),
                        ..Default::default()
                    };
                }
            };
            if !before_exists {
                return ProvenanceFileChange {
                    path: relative.clone(),
                    before_exists: false,
                    after_checksum: Some(after_checksum),
                    reversible: true,
                    ..Default::default()
                };
            }
            let Some(preimage) = preimages.get(path) else {
                return ProvenanceFileChange {
                    path: relative.clone(),
                    before_exists: true,
                    after_checksum: Some(after_checksum),
                    reason: Some("pre-change text was not captured safely".into()),
                    ..Default::default()
                };
            };
            ProvenanceFileChange {
                path: relative.clone(),
                before_exists: true,
                before_text: Some(preimage.text.clone()),
                before_checksum: Some(preimage.checksum.clone()),
                after_checksum: Some(after_checksum),
                reversible: true,
                reason: None,
            }
        })
        .collect()
}

/// Add writes that an mtime-only workspace scan can miss.
///
/// Explicit file tools name their destination, while captured text preimages
/// let shell/Python/R calls prove a content change without trusting timestamp
/// granularity.
pub fn augment_written_paths(
    tool: &str,
    root: &Path,
    source: &str,
    success: bool,
    preimages: &BTreeMap<PathBuf, TextPreimage>,
    written: &mut Vec<String>,
) {
    let relative = |path: &Path| {
        path.strip_prefix(root)
            .ok()
            .filter(|path| {
                !path.as_os_str().is_empty()
                    && path
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
            })
            .map(|path| path.to_string_lossy().replace('\\', "/"))
    };
    for (path, preimage) in preimages {
        if read_current_text(path).is_ok_and(|(_, checksum)| checksum != preimage.checksum) {
            if let Some(path) = relative(path) {
                written.push(path);
            }
        }
    }
    if success && matches!(tool, "write" | "edit" | "generate_image") {
        let source_path = Path::new(source);
        let path = if source_path.is_absolute() {
            source_path.to_path_buf()
        } else {
            root.join(source_path)
        };
        if path.is_file() && !preimages.keys().any(|before| same_path(before, &path)) {
            if let Some(path) = relative(&path) {
                written.push(path);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    written.retain(|path| seen.insert(path_identity(&root.join(path))));
    written.sort();
}

fn snapshot_capped(root: &Path, max_entries: usize) -> BTreeMap<PathBuf, SystemTime> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    let mut visited = 0;
    'walk: while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            if visited >= max_entries {
                break 'walk;
            }
            visited += 1;
            let Ok(ft) = entry.file_type() else { continue };
            let p = entry.path();
            if ft.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if !SKIP_DIRS.contains(&name) {
                    stack.push(p);
                }
            } else if ft.is_file() {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                out.insert(p, mtime);
            }
        }
    }
    out
}

/// Diff two snapshots → (files_written, files_read), both workspace-relative.
/// written = new or mtime-advanced files; read = pre-existing files (not also written)
/// whose relative path appears literally in `source`.
pub fn diff(
    before: &BTreeMap<PathBuf, SystemTime>,
    after: &BTreeMap<PathBuf, SystemTime>,
    root: &Path,
    source: &str,
) -> (Vec<String>, Vec<String>) {
    let rel = |p: &Path| -> String {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .replace('\\', "/")
    };
    let mut written = Vec::new();
    for (p, mt) in after {
        match before.get(p) {
            None => written.push(rel(p)),
            Some(old) if mt > old => written.push(rel(p)),
            _ => {}
        }
    }
    written.sort();
    let wset: std::collections::HashSet<&String> = written.iter().collect();
    let mut read = Vec::new();
    for p in before.keys() {
        let r = rel(p);
        if !r.is_empty() && !wset.contains(&r) && source.contains(&r) {
            read.push(r);
        }
    }
    read.sort();
    (written, read)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_written_and_read_and_skips_git() {
        let tmp = std::env::temp_dir().join("wisp_prov_snap_test");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::create_dir_all(tmp.join(".git")).unwrap();
        std::fs::write(tmp.join("data.csv"), b"x").unwrap();
        std::fs::write(tmp.join(".git/HEAD"), b"x").unwrap();
        let before = snapshot(&tmp);
        assert!(
            !before.keys().any(|p| p.ends_with("HEAD")),
            ".git must be skipped"
        );
        std::fs::write(tmp.join("out.png"), b"y").unwrap();
        let after = snapshot(&tmp);
        let (w, r) = diff(
            &before,
            &after,
            &tmp,
            "df=read_csv('data.csv'); savefig('out.png')",
        );
        assert!(w.contains(&"out.png".to_string()));
        assert!(r.contains(&"data.csv".to_string()));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn r_is_a_producing_language_with_code_source() {
        assert!(is_producing("r"));
        assert_eq!(language_of("r"), "r");
        assert_eq!(
            source_of("r", &serde_json::json!({"code": "png('plot.png')"})),
            "png('plot.png')"
        );
    }

    #[test]
    fn file_mutation_tools_are_producing_with_path_source() {
        for tool in ["write", "edit", "generate_image"] {
            assert!(is_producing(tool));
            assert_eq!(
                source_of(tool, &serde_json::json!({"path": "results/table.csv"})),
                "results/table.csv"
            );
        }
    }

    #[test]
    fn snapshot_caps_entries_inside_a_wide_directory() {
        let tmp = std::env::temp_dir().join("wisp_prov_wide_test");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::create_dir_all(&tmp).unwrap();
        for name in ["a", "b", "c"] {
            std::fs::write(tmp.join(name), b"x").unwrap();
        }

        assert_eq!(snapshot_capped(&tmp, 2).len(), 2);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn undo_captures_existing_and_new_markdown_but_not_word() {
        let tmp = std::env::temp_dir().join("wisp_prov_undo_text_test");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("notes.md"), "before\n").unwrap();
        let before = snapshot(&tmp);
        let preimages = capture_text_preimages(&before, &tmp, "edit notes.md");

        std::fs::write(tmp.join("notes.md"), "after\n").unwrap();
        std::fs::write(tmp.join("summary.md"), "new\n").unwrap();
        std::fs::write(tmp.join("paper.docx"), b"PK\x03\x04").unwrap();
        let written = vec!["notes.md".into(), "summary.md".into(), "paper.docx".into()];
        let changes = undo_file_changes(&before, &tmp, &written, &preimages);

        assert!(changes[0].reversible);
        assert_eq!(changes[0].before_text.as_deref(), Some("before\n"));
        assert!(changes[1].reversible);
        assert!(!changes[1].before_exists);
        assert!(!changes[2].reversible);
        assert!(changes[2].reason.as_deref().unwrap().contains("binary"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn existing_text_with_a_dynamic_destination_is_not_guessed() {
        let tmp = std::env::temp_dir().join("wisp_prov_undo_dynamic_test");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("notes.md"), "before\n").unwrap();
        let before = snapshot(&tmp);
        let preimages = capture_text_preimages(&before, &tmp, "run generated destination");
        std::fs::write(tmp.join("notes.md"), "after\n").unwrap();
        let changes = undo_file_changes(&before, &tmp, &["notes.md".into()], &preimages);

        assert!(!changes[0].reversible);
        assert!(changes[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("not captured"));
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn changed_preimages_do_not_depend_on_mtime_resolution() {
        let tmp = std::env::temp_dir().join("wisp_prov_undo_timestamp_test");
        std::fs::remove_dir_all(&tmp).ok();
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("notes.md"), "before\n").unwrap();
        let before = snapshot(&tmp);
        let preimages = capture_text_preimages(&before, &tmp, "notes.md");
        std::fs::write(tmp.join("notes.md"), "after\n").unwrap();

        let mut written = Vec::new();
        augment_written_paths("shell", &tmp, "notes.md", true, &preimages, &mut written);
        assert_eq!(written, vec!["notes.md"]);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
