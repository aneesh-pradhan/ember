//! A compact map of the project for the system prompt, so the model can go straight to the right files instead of
//! spending its first round trips on list_files and grep (Nemotron makes one tool call per reply).
//!
//! Small projects get every file with its top-level definitions; large ones get their directories with file
//! counts. The system prompt is the cached prefix of every request, so after the first call the map is billed at
//! the cached rate. EMBER_REPO_MAP=0 turns it off.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

/// Upper bound on the map's size (about 2k tokens).
const MAX_CHARS: usize = 7_000;
/// Projects up to this many files are mapped file by file, with definitions.
const DETAILED_FILES: usize = 150;
const SCAN_LIMIT: usize = 5_000;
const NAMES_PER_FILE: usize = 10;

/// The map, or None when it's turned off or there are no files.
pub fn build(root: &Path) -> Option<String> {
    if matches!(std::env::var("EMBER_REPO_MAP").as_deref(), Ok("0" | "off" | "false")) {
        return None;
    }
    let files: Vec<String> = crate::tools::project_files(root, SCAN_LIMIT).into_iter().filter(|f| !is_noise(f)).collect();
    if files.is_empty() {
        return None;
    }
    let map = if files.len() <= DETAILED_FILES { detailed(root, &files) } else { by_directory(&files) };
    Some(cap(&map))
}

/// Lockfiles, data, logs and binary assets: never the file the model needs to find, and they crowd out the ones it
/// does. list_files still shows them.
fn is_noise(file: &str) -> bool {
    const LOCKFILES: &[&str] = &[
        "Cargo.lock", "package-lock.json", "yarn.lock", "pnpm-lock.yaml", "bun.lockb", "poetry.lock", "uv.lock",
        "Pipfile.lock", "Gemfile.lock", "composer.lock", "go.sum", "flake.lock", "mix.lock", "pubspec.lock",
    ];
    const EXTENSIONS: &[&str] = &[
        "jsonl", "ndjson", "csv", "tsv", "parquet", "arrow", "feather", "pkl", "npy", "npz", "h5", "sqlite", "sqlite3",
        "db", "log", "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "pdf", "zip", "gz", "tgz", "bz2", "xz", "7z",
        "jar", "wasm", "so", "dylib", "dll", "exe", "o", "a", "class", "pyc", "woff", "woff2", "ttf", "otf", "mp3",
        "mp4", "mov", "wav",
    ];
    let name = file.rsplit('/').next().unwrap_or(file);
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    LOCKFILES.contains(&name) || EXTENSIONS.contains(&ext.as_str()) || name.ends_with(".min.js") || name.ends_with(".map")
}

/// Every file, with its top-level definitions where the language is known.
fn detailed(root: &Path, files: &[String]) -> String {
    let mut out = String::new();
    for file in files {
        let names = definitions(root, file);
        out.push_str(file);
        if !names.is_empty() {
            let more = if names.len() > NAMES_PER_FILE { ", …" } else { "" };
            out.push_str(&format!(": {}{more}", names[..names.len().min(NAMES_PER_FILE)].join(", ")));
        }
        out.push('\n');
    }
    out
}

/// Directories (two levels deep) with how many files each holds, plus the files at the top level.
fn by_directory(files: &[String]) -> String {
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    let mut top = vec![];
    for file in files {
        let parts: Vec<&str> = file.split('/').collect();
        if parts.len() == 1 {
            top.push(file.as_str());
        } else {
            *dirs.entry(parts[..parts.len().min(3) - 1].join("/") + "/").or_default() += 1;
        }
    }
    let mut out = format!("{} files; directories with file counts (list_files or grep for more):\n", files.len());
    for (dir, n) in dirs {
        out.push_str(&format!("{dir} ({n})\n"));
    }
    for file in top {
        out.push_str(&format!("{file}\n"));
    }
    out
}

fn cap(map: &str) -> String {
    if map.len() <= MAX_CHARS {
        return map.trim_end().to_string();
    }
    let cut = map[..MAX_CHARS].rfind('\n').unwrap_or(MAX_CHARS);
    format!("{}\n… (map truncated; use list_files)", &map[..cut])
}

/// Top-level definitions in a source file, by language. Lines are matched with simple patterns, not parsed.
fn definitions(root: &Path, file: &str) -> Vec<String> {
    static PATTERNS: OnceLock<Vec<(&'static [&'static str], Regex)>> = OnceLock::new();
    let patterns = PATTERNS.get_or_init(|| {
        let re = |p: &str| Regex::new(p).unwrap();
        vec![
            (&["rs"][..], re(r"^(?:pub(?:\([a-z]+\))? )?(?:async )?(?:fn|struct|enum|trait|mod|type) ([A-Za-z_][A-Za-z0-9_]*)")),
            (&["py"][..], re(r"^(?:async )?(?:def|class) ([A-Za-z_][A-Za-z0-9_]*)")),
            (&["js", "jsx", "ts", "tsx", "mjs", "cjs"][..],
             re(r"^(?:export )?(?:default )?(?:async )?(?:function\*? |class )([A-Za-z_$][\w$]*)|^(?:export )?(?:const|let) ([A-Za-z_$][\w$]*) = (?:async )?(?:\(|function)")),
            (&["go"][..], re(r"^(?:func (?:\([^)]*\) )?|type )([A-Za-z_][A-Za-z0-9_]*)")),
            (&["rb"][..], re(r"^(?:def|class|module) ([A-Za-z_][A-Za-z0-9_:?!]*)")),
            // Types at the start of a line followed by a body or base list (not forward declarations), and function
            // definitions: a return type, then a name and an argument list on a line with no `;` (so declarations
            // and calls don't count).
            (&["c", "h", "cc", "cpp", "cxx", "hh", "hpp", "hxx"][..],
             re(r"^(?:template\s*<[^>]*>\s*)?(?:class|struct|enum(?: class)?|union|namespace) ([A-Za-z_]\w*)\s*(?:$|[:{]|final\b)|^(?:(?:static|inline|extern|constexpr|virtual|friend)\s+)*(?:const\s+)?[A-Za-z_][\w:<>,]*(?:\s*[*&]+\s*|\s+)((?:[A-Za-z_]\w*::)*~?[A-Za-z_]\w*)\s*\([^;]*$")),
            (&["java", "kt", "cs"][..],
             re(r"^(?:(?:public|private|protected|internal|abstract|final|static|sealed|partial|data|open)\s+)*(?:class|interface|enum|record|object|struct) ([A-Za-z_]\w*)")),
        ]
    });
    let ext = file.rsplit('.').next().unwrap_or("");
    let Some((_, re)) = patterns.iter().find(|(exts, _)| exts.contains(&ext)) else { return vec![] };
    let path = root.join(file);
    if std::fs::metadata(&path).map(|m| m.len() > 256 * 1024).unwrap_or(true) {
        return vec![];
    }
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    // Patterns with alternatives capture the name in whichever group matched.
    text.lines().filter_map(|l| re.captures(l)).filter_map(|c| c.iter().skip(1).flatten().next().map(|m| m.as_str().to_string())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ember-map-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (path, text) in files {
            let p = dir.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        dir
    }

    #[test]
    fn small_projects_list_files_with_definitions() {
        let dir = project("small", &[
            ("src/lib.rs", "pub fn parse() {}\nstruct Token;\n    fn nested() {}\npub(crate) enum Kind {}\nimpl Token {}\n"),
            ("app/models.py", "import os\nclass Invoice:\n    def total(self): ...\ndef helper():\n    pass\nasync def fetch(): ...\n"),
            ("web/ui.ts", "export function render() {}\nexport const go = async () => 1;\nconst x = 3;\nexport default class App {}\n"),
            ("README.md", "# hi\n"),
        ]);
        let map = build(&dir).unwrap();
        assert!(map.contains("src/lib.rs: parse, Token, Kind"), "{map}");
        assert!(map.contains("app/models.py: Invoice, helper, fetch"), "{map}");
        assert!(map.contains("web/ui.ts: render, go, App"), "{map}");
        assert!(map.lines().any(|l| l == "README.md"), "{map}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn maps_c_cpp_and_java_definitions() {
        let dir = project("cpp", &[
            ("src/graph.cpp", "#include \"graph.h\"\nint Graph::shortest(int a, int b) {\n  return helper(a);\n}\n\
                               static void helper(int x)\n{\n}\nint main(int argc, char** argv) {\n}\nvoid declared(int);\n"),
            ("src/graph.h", "#pragma once\nclass Graph {\npublic:\n  int shortest(int a, int b);\n};\nstruct Edge;\n\
                             template <typename T>\nstruct Heap {\n};\nenum class Kind { A };\n"),
            ("App.java", "package x;\npublic final class App {\n  public static void main(String[] a) {}\n}\n"),
        ]);
        let map = build(&dir).unwrap();
        assert!(map.contains("src/graph.cpp: Graph::shortest, helper, main"), "{map}");
        assert!(map.contains("src/graph.h: Graph, Heap, Kind"), "{map}");
        assert!(map.contains("App.java: App"), "{map}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn skips_lockfiles_and_data_files() {
        let dir = project("noise", &[
            ("src/main.rs", "fn main() {}\n"),
            ("Cargo.lock", "# lock\n"),
            ("bench/results/run.jsonl", "{}\n"),
            ("docs/logo.png", "png"),
            ("web/app.min.js", "x"),
        ]);
        let map = build(&dir).unwrap();
        assert_eq!(map, "src/main.rs: main");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn large_projects_list_directories_and_stay_capped() {
        let files: Vec<(String, String)> = (0..400).map(|i| (format!("pkg/mod{}/f{i}.py", i % 7), "def f(): ...\n".into())).collect();
        let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
        let dir = project("large", &refs);
        let map = build(&dir).unwrap();
        assert!(map.starts_with("400 files;") && map.contains("pkg/mod0/ (58)"), "{map}");
        assert!(map.len() <= MAX_CHARS + 60);
        assert_eq!(cap(&"x\n".repeat(5_000)).lines().last(), Some("… (map truncated; use list_files)"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
