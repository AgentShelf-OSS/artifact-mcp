//! `cargo xtask release ...`: release evidence commands (ported from `scripts/release/*.mjs`).
//!
//! The release workflow attaches the files these commands write to every GitHub Release, and
//! operators verify them before a deployment. The output bytes, accepted inputs and exit codes
//! match the Node scripts they replace:
//!
//! - exit 2 for a usage or input-format error of `source-sbom` and `manifest`;
//! - exit 1 for a runtime failure, a failed manifest verification, or a failed fixture preflight.

mod fixtures;
mod format;
mod http;
mod js;
mod manifest;
mod sbom;
mod verify_manifest;

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::Result;

const USAGE: &str = "\
usage: cargo xtask release <command> [args]

commands:
  source-sbom               write the deterministic SPDX 2.3 source SBOM for a clean HEAD
                            --out PATH --commit SHA --version X.Y.Z --created UTC --repository OWNER/REPO
  manifest                  write release-manifest.json from the release evidence files
                            (run `cargo xtask release manifest --help` for the flags)
  verify-manifest FILE      verify a release manifest and the evidence files beside it
  verify-fixtures-in-image IMAGE@sha256:DIGEST OUT.json
                            boot every frozen historical fixture in the exact OCI image (Docker)
  verify-fixtures-in-image --binary PATH [--fixtures DIR] OUT.json
                            run the same fixture checks against a native artifact-mcp binary";

/// Dispatch `cargo xtask release <subcommand>`.
pub fn run(args: &[String]) -> Result<bool> {
    let Some((command, rest)) = args.split_first() else {
        return Err(USAGE.into());
    };
    match command.as_str() {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(true)
        }
        "source-sbom" => sbom::run(rest),
        "manifest" => manifest::run(rest),
        "verify-manifest" => verify_manifest::run(rest),
        "verify-fixtures-in-image" => fixtures::run(rest),
        other => Err(format!("unknown release command {other}\n\n{USAGE}").into()),
    }
}

/// Why a generator stopped: the Node scripts exited 2 for usage errors and 1 otherwise.
enum Failure {
    Usage(String),
    Runtime(String),
}

impl<E: std::fmt::Display> From<E> for Failure {
    fn from(error: E) -> Self {
        Self::Runtime(error.to_string())
    }
}

/// Map a generator result to the xtask exit status (2 for usage, 1 for runtime).
fn finish(result: std::result::Result<(), Failure>, usage: &str) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(Failure::Usage(message)) => Err(format!("error: {message}\n{usage}").into()),
        Err(Failure::Runtime(message)) => {
            eprintln!("{message}");
            Ok(false)
        }
    }
}

/// The `--key value` parser of the Node generators: pairs only, no repeats, no unknown keys,
/// and every allowed key present. Returns the values in `allowed` order.
fn parse_pairs(args: &[String], allowed: &[&str]) -> std::result::Result<Vec<String>, Failure> {
    let mut values: Vec<(&str, &str)> = Vec::new();
    for pair in args.chunks(2) {
        let key = pair[0].as_str();
        let Some(value) = pair.get(1) else {
            return Err(Failure::Usage("invalid arguments".to_owned()));
        };
        if !key.starts_with("--") || values.iter().any(|(seen, _)| *seen == key) {
            return Err(Failure::Usage("invalid arguments".to_owned()));
        }
        values.push((key, value));
    }
    if let Some((key, _)) = values.iter().find(|(key, _)| !allowed.contains(key)) {
        return Err(Failure::Usage(format!("unknown option {key}")));
    }
    allowed
        .iter()
        .map(|key| {
            values
                .iter()
                .find(|(seen, _)| seen == key)
                .map(|(_, value)| (*value).to_owned())
                .ok_or_else(|| Failure::Usage(format!("missing {key}")))
        })
        .collect()
}

/// `true` when the only argument asks for help.
fn wants_help(args: &[String]) -> bool {
    matches!(args, [only] if only == "-h" || only == "--help")
}

/// Node's `path.resolve(path)`: absolute against the working directory, `.` and `..` removed
/// lexically (symbolic links are not consulted).
fn resolve(path: &str) -> std::io::Result<PathBuf> {
    let joined = std::env::current_dir()?.join(path);
    let mut resolved = PathBuf::from("/");
    for component in joined.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(part) => resolved.push(part),
        }
    }
    Ok(resolved)
}

/// Node's `path.join(base, relative)`: `relative` is appended even when it starts with `/`, then
/// `.` and `..` are removed lexically.
fn node_join(base: &Path, relative: &str) -> PathBuf {
    let mut joined = base.to_path_buf();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                joined.pop();
            }
            part => joined.push(part),
        }
    }
    joined
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(Sha256::digest(bytes.as_ref()))
}

/// `mkdir(dirname(out), { recursive: true })` then `writeFile(out, text)`.
fn write_output(out: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(out, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn usage_message(result: std::result::Result<Vec<String>, Failure>) -> String {
        match result {
            Err(Failure::Usage(message)) => message,
            Err(Failure::Runtime(message)) => panic!("runtime failure {message}"),
            Ok(values) => panic!("accepted {values:?}"),
        }
    }

    #[test]
    fn pair_parser_matches_the_node_generators() {
        let allowed = ["--a", "--b"];
        assert_eq!(
            parse_pairs(&strings(&["--b", "2", "--a", "--1"]), &allowed).ok(),
            Some(strings(&["--1", "2"]))
        );
        for (args, message) in [
            (&["--a"][..], "invalid arguments"),
            (&["a", "1", "--b", "2"][..], "invalid arguments"),
            (&["--a", "1", "--a", "2"][..], "invalid arguments"),
            (&["--a=1", "--b"][..], "unknown option --a=1"),
            (
                &["--c", "1", "--a", "1", "--b", "2"][..],
                "unknown option --c",
            ),
            (&["--b", "2"][..], "missing --a"),
            (&[][..], "missing --a"),
        ] {
            assert_eq!(
                usage_message(parse_pairs(&strings(args), &allowed)),
                message
            );
        }
    }

    #[test]
    fn paths_resolve_like_node() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(resolve("a/../b/./c").unwrap(), cwd.join("b/c"));
        assert_eq!(resolve("/x/../../y").unwrap(), PathBuf::from("/y"));
        assert_eq!(
            node_join(Path::new("/f"), "/etc/x"),
            PathBuf::from("/f/etc/x")
        );
        assert_eq!(node_join(Path::new("/f/g"), "../h"), PathBuf::from("/f/h"));
    }
}
