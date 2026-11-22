//! Repository wiring and documentation paths must keep matching the checkout.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const DOCS: &[&str] = &["AGENTS.md", "CONTEXT.md", "CONTRIBUTING.md"];
const PATH_PREFIXES: &[&str] = &[
    "src/",
    "tests/",
    "xtask/",
    "docs/",
    "conformance/",
    "migrations/",
    "templates/",
    "assets/",
    "browser-tests/",
    "preview-renderer/",
    ".github/",
];

// (document, exact token, reason) for intentional examples that do not exist in the checkout.
// Keep this empty until an example needs an exception; unused exceptions fail the test.
const PATH_ALLOWLIST: &[(&str, &str, &str)] = &[];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn registered_modules(source: &str) -> BTreeSet<&str> {
    source
        .lines()
        .filter_map(|line| line.trim().strip_prefix("mod ")?.strip_suffix(';'))
        .filter(|name| {
            !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
        .collect()
}

#[test]
fn native_test_registry_matches_files() {
    let dir = repo_root().join("tests/native");
    let source = fs::read_to_string(dir.join("main.rs")).expect("read native test registry");
    let modules = registered_modules(&source);
    let mut violations = Vec::new();
    for entry in fs::read_dir(&dir).expect("read native test directory") {
        let path = entry.expect("native test directory entry").path();
        if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("rs") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .expect("UTF-8 native test filename");
        if stem != "main" && !modules.contains(stem) {
            violations.push(format!(
                "tests/native/{stem}.rs has no mod {stem}; declaration"
            ));
        }
    }
    for stem in modules {
        if !dir.join(format!("{stem}.rs")).is_file() && !dir.join(stem).is_dir() {
            violations.push(format!(
                "mod {stem}; has no tests/native/{stem}.rs or directory"
            ));
        }
    }
    violations.sort();
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

fn strip_line_suffix(token: &str) -> &str {
    if let Some((path, suffix)) = token.rsplit_once(':')
        && !suffix.is_empty()
        && suffix.split('-').count() <= 2
        && suffix.split('-').all(|part| {
            !part.is_empty() && part.bytes().all(|character| character.is_ascii_digit())
        })
    {
        return path;
    }
    token
}

fn repo_path(token: &str) -> Option<&str> {
    let path = strip_line_suffix(token);
    let top_level_doc = !path.contains('/') && (path.ends_with(".md") || path.ends_with(".toml"));
    (top_level_doc || PATH_PREFIXES.iter().any(|prefix| path.starts_with(prefix))).then_some(path)
}

fn path_exists(root: &Path, path: &str) -> bool {
    if let Some(index) = path.find(['*', '?', '[', '{']) {
        let prefix = &path[..index];
        let directory = if prefix.ends_with('/') {
            prefix
        } else {
            prefix.rsplit_once('/').map_or("", |(parent, _)| parent)
        };
        return root.join(directory).is_dir();
    }
    root.join(path).exists()
}

#[test]
fn documented_repo_paths_exist() {
    let root = repo_root();
    let mut used = BTreeSet::new();
    let mut violations = Vec::new();
    for doc in DOCS {
        let source = fs::read_to_string(root.join(doc)).expect("read repository instructions");
        for token in source.split('`').skip(1).step_by(2) {
            let Some(path) = repo_path(token) else {
                continue;
            };
            if path_exists(&root, path) {
                continue;
            }
            if let Some(index) =
                PATH_ALLOWLIST
                    .iter()
                    .position(|(allowed_doc, allowed_token, _)| {
                        allowed_doc == doc && *allowed_token == token
                    })
            {
                used.insert(index);
            } else {
                violations.push(format!("{doc}: missing repository path `{token}`"));
            }
        }
    }
    for (index, (doc, token, reason)) in PATH_ALLOWLIST.iter().enumerate() {
        if !used.contains(&index) {
            violations.push(format!(
                "unused path allowlist: {doc}: `{token}` ({reason})"
            ));
        }
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}

#[test]
fn path_tokens_handle_locations_and_globs() {
    assert_eq!(repo_path("src/config/mod.rs:84"), Some("src/config/mod.rs"));
    assert_eq!(
        repo_path("tests/native/main.rs:3-6"),
        Some("tests/native/main.rs")
    );
    assert_eq!(repo_path("Cargo.toml"), Some("Cargo.toml"));
    assert_eq!(repo_path("cargo test --workspace"), None);
    assert_eq!(repo_path("GET /api/library"), None);
    assert_eq!(repo_path("https://example.com/test.md"), None);
    assert!(path_exists(&repo_root(), "tests/native/*.rs"));
    assert!(path_exists(&repo_root(), "tests/native/uNN_*.rs"));
    assert!(!path_exists(&repo_root(), "tests/no-such-directory/*.rs"));
    assert_eq!(strip_line_suffix("src/lib.rs:bad"), "src/lib.rs:bad");
}

fn collect_markdown(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).expect("read documentation directory") {
        let path = entry.expect("documentation directory entry").path();
        if path.is_dir() {
            collect_markdown(&path, files);
        } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
            files.push(path);
        }
    }
}

/// `/mcp` is reachable without Cloudflare Access. A documented `ARTIFACT_API_KEYS` example that
/// the server would seed is therefore a published credential for anyone who copies it unchanged.
#[test]
fn documented_seed_key_examples_never_seed_a_live_key() {
    let root = repo_root();
    let mut files = vec![
        root.join("README.md"),
        root.join("GETTING_STARTED.md"),
        root.join(".env.example"),
    ];
    collect_markdown(&root.join("docs"), &mut files);
    let mut examples = 0_usize;
    let mut violations = Vec::new();
    for file in files {
        let text = fs::read_to_string(&file).expect("read documentation file");
        for (index, line) in text.lines().enumerate() {
            let Some((_, rest)) = line.split_once("ARTIFACT_API_KEYS=") else {
                continue;
            };
            let value = rest
                .split(|character: char| character.is_whitespace() || character == '`')
                .next()
                .unwrap_or_default()
                .trim_matches(['"', '\'']);
            if value.is_empty() {
                continue;
            }
            examples += 1;
            let parsed = artifact_mcp::config::SeedKeys::parse(value);
            if !parsed.entries.is_empty() {
                violations.push(format!(
                    "{}:{} documents a seed entry that the server would accept",
                    file.strip_prefix(&root).unwrap_or(&file).display(),
                    index + 1
                ));
            }
        }
    }
    assert!(examples > 0, "no ARTIFACT_API_KEYS example was found");
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
