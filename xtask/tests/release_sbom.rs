//! `cargo xtask release source-sbom` (ported from the source SBOM case of
//! `test/release-manifest.test.js`).

mod release_support;

use std::path::Path;
use std::process::Command;

use release_support::{TempDir, release, release_ok, stderr};

const CREATED: &str = "2026-07-29T18:00:00Z";

/// `scripts/release/generate-source-sbom.mjs` output for [`synthetic_repository`], with the
/// commit replaced by `COMMIT`.
const NODE_SBOM: &str = include_str!("release_support/source-sbom.node.json");

fn git(directory: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A committed tree whose names exercise escaping and the UTF-16 sort of `Array.sort()`.
fn synthetic_repository() -> (TempDir, String) {
    let repository = TempDir::new("source-sbom");
    git(&repository.path, &["init", "-q", "-b", "main", "."]);
    for (name, contents) in [
        ("Cargo.lock", "# lock\n"),
        ("README.md", "hello\n"),
        ("B.txt", "upper\n"),
        ("b.txt", "lower\n"),
        ("a/x.rs", "fn main() {}\n"),
        ("é.txt", "accent\n"),
        ("😀.txt", "astral\n"),
        ("\u{ff5e}.txt", "bmp\n"),
        ("space name.txt", "space\n"),
        ("empty", ""),
        ("quote\"name.txt", "quote\n"),
        ("tab\tname.txt", "tab\n"),
        ("back\\slash.txt", "back\n"),
    ] {
        repository.write(name, contents);
    }
    std::os::unix::fs::symlink("README.md", repository.join("link")).unwrap();
    git(&repository.path, &["add", "-A"]);
    git(&repository.path, &["commit", "-q", "-m", "fixture"]);
    let commit = git(&repository.path, &["rev-parse", "HEAD"]);
    (repository, commit)
}

fn sbom_args<'a>(out: &'a str, commit: &'a str) -> Vec<&'a str> {
    vec![
        "source-sbom",
        "--out",
        out,
        "--commit",
        commit,
        "--version",
        "1.6.0-rc.1",
        "--created",
        CREATED,
        "--repository",
        "AgentShelf-OSS/artifact-mcp",
    ]
}

#[test]
fn source_sbom_is_stable_for_a_fixed_commit_and_matches_the_node_bytes() {
    let (repository, commit) = synthetic_repository();
    let out = TempDir::new("source-sbom-out");
    let output = out.join("nested/source.spdx.json");
    let output = output.to_str().unwrap();
    release_ok(&sbom_args(output, &commit), &repository.path);
    let first = std::fs::read_to_string(output).unwrap();
    release_ok(&sbom_args(output, &commit), &repository.path);
    assert_eq!(std::fs::read_to_string(output).unwrap(), first);
    assert_eq!(first, NODE_SBOM.replace("COMMIT", &commit));

    let sbom: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(sbom["spdxVersion"], "SPDX-2.3");
    assert_eq!(sbom["creationInfo"]["created"], CREATED);
    let names = sbom["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["fileName"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(names.contains(&"./Cargo.lock"));
    // UTF-16 order puts the astral 😀 (a surrogate pair) before U+FF5E; byte order would not.
    assert_eq!(&names[names.len() - 2..], ["./😀.txt", "./\u{ff5e}.txt"]);
}

#[test]
fn source_sbom_describes_only_a_clean_checkout_of_the_named_commit() {
    let (repository, commit) = synthetic_repository();
    let out = repository.join("out/source.spdx.json");
    let out = out.to_str().unwrap();

    // Untracked files (the release directory, build output) are not part of the source.
    repository.write("untracked.txt", "not committed\n");
    release_ok(&sbom_args(out, &commit), &repository.path);
    assert!(
        !std::fs::read_to_string(out)
            .unwrap()
            .contains("untracked.txt")
    );

    let other = "b".repeat(40);
    let result = release(&sbom_args(out, &other), &repository.path);
    assert_eq!(result.status.code(), Some(2));
    assert!(stderr(&result).contains("error: commit does not match checked-out source"));

    repository.write("README.md", "edited after the commit\n");
    let result = release(&sbom_args(out, &commit), &repository.path);
    assert_eq!(result.status.code(), Some(2));
    assert!(
        stderr(&result).contains("tracked files differ from the checked-out commit"),
        "{}",
        stderr(&result)
    );
}

#[test]
fn source_sbom_rejects_invalid_inputs_like_the_node_script() {
    let (repository, commit) = synthetic_repository();
    for (index, value, expected) in [
        (4, "abc", "commit has an invalid format"),
        (6, "v1.6.0", "version has an invalid format"),
        (
            8,
            "2026-07-29T18:00:00.000Z",
            "created has an invalid format",
        ),
        (10, "AgentShelf-OSS", "repository has an invalid format"),
    ] {
        let mut args = sbom_args("out.json", &commit);
        args[index] = value;
        let result = release(&args, &repository.path);
        assert_eq!(result.status.code(), Some(2), "{expected}");
        assert!(stderr(&result).contains(&format!("error: {expected}")));
    }
    let mut args = sbom_args("out.json", &commit);
    args.extend(["--extra", "1"]);
    let result = release(&args, &repository.path);
    assert_eq!(result.status.code(), Some(2));
    assert!(stderr(&result).contains("error: unknown option --extra"));

    let outside = TempDir::new("source-sbom-outside");
    let result = release(&sbom_args("out.json", &commit), &outside.path);
    assert_eq!(
        result.status.code(),
        Some(1),
        "git rev-parse fails outside a repository"
    );
}
