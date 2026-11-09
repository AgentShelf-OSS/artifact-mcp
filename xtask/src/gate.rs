//! Local CI checks for the main workspace and the two isolated browser workspaces.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::process::Command;

use crate::Result;

const USAGE: &str = "\
usage: cargo xtask gate [--dry-run] [--skip-browser]

Runs the main workspace, preview-renderer, then browser-tests checks, stopping at the first failure.
CHROME_HEADLESS_SHELL must name the pinned browser. ARTIFACT_MCP_BIN must name a built server binary.
The isolated workspaces run with CI=1, including the preview renderer's user-namespace checks.

  --dry-run       print the commands without running them or checking prerequisites
  --skip-browser  skip browser-tests explicitly; preview-renderer still needs Chrome";

#[derive(Debug, PartialEq, Eq)]
struct Step {
    name: &'static str,
    args: Vec<&'static str>,
    env: Vec<(&'static str, &'static str)>,
}

fn commands(skip_browser: bool) -> Vec<Step> {
    let mut steps = vec![
        Step {
            name: "main: fmt",
            args: vec!["fmt", "--all", "--check"],
            env: vec![],
        },
        Step {
            name: "main: clippy with all features",
            args: vec![
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            env: vec![],
        },
        Step {
            name: "main: clippy without features",
            args: vec!["clippy", "--workspace", "--locked", "--", "-D", "warnings"],
            env: vec![],
        },
        Step {
            name: "main: tests",
            args: vec!["test", "--workspace", "--all-targets", "--locked"],
            env: vec![],
        },
        Step {
            name: "main: doctests",
            args: vec!["test", "--doc", "--locked"],
            env: vec![],
        },
        Step {
            name: "main: rustdoc",
            args: vec!["doc", "--workspace", "--no-deps", "--locked"],
            env: vec![("RUSTDOCFLAGS", "-D warnings")],
        },
        Step {
            name: "main: historical fixtures",
            args: vec!["xtask", "historical-fixture", "--verify-against-committed"],
            env: vec![],
        },
    ];
    let workspaces = [
        ("preview-renderer", "preview-renderer/Cargo.toml"),
        ("browser-tests", "browser-tests/Cargo.toml"),
    ];
    for (workspace, manifest) in workspaces {
        if skip_browser && workspace == "browser-tests" {
            continue;
        }
        for (check, args) in [
            ("fmt", vec!["fmt", "--manifest-path", manifest, "--check"]),
            (
                "clippy",
                vec![
                    "clippy",
                    "--manifest-path",
                    manifest,
                    "--all-targets",
                    "--locked",
                    "--",
                    "-D",
                    "warnings",
                ],
            ),
            (
                "tests",
                vec!["test", "--manifest-path", manifest, "--locked"],
            ),
            (
                "rustdoc",
                vec!["doc", "--manifest-path", manifest, "--no-deps", "--locked"],
            ),
        ] {
            let mut env = vec![("CI", "1")];
            if check == "rustdoc" {
                env.push(("RUSTDOCFLAGS", "-D warnings"));
            }
            steps.push(Step {
                name: workspace,
                args,
                env,
            });
        }
    }
    steps
}

fn missing_prerequisites(
    env: &BTreeMap<OsString, OsString>,
    skip_browser: bool,
) -> Vec<&'static str> {
    ["CHROME_HEADLESS_SHELL", "ARTIFACT_MCP_BIN"]
        .into_iter()
        .filter(|name| !skip_browser || *name != "ARTIFACT_MCP_BIN")
        .filter(|name| {
            env.get(OsStr::new(name))
                .is_none_or(|value| value.is_empty())
        })
        .collect()
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_./:=-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

impl Step {
    fn display(&self, cargo: &OsStr) -> String {
        self.env
            .iter()
            .map(|(key, value)| format!("{key}={}", shell_word(value)))
            .chain(std::iter::once(shell_word(&cargo.to_string_lossy())))
            .chain(self.args.iter().map(|arg| shell_word(arg)))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(crate) fn run(args: &[String]) -> Result<bool> {
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return Ok(true);
    }
    let mut dry_run = false;
    let mut skip_browser = false;
    for arg in args {
        match arg.as_str() {
            "--dry-run" => dry_run = true,
            "--skip-browser" => skip_browser = true,
            _ => return Err(format!("unknown gate argument {arg}\n\n{USAGE}").into()),
        }
    }
    if skip_browser {
        println!("SKIPPED: browser-tests (--skip-browser)");
    }
    let env = ["CHROME_HEADLESS_SHELL", "ARTIFACT_MCP_BIN"]
        .into_iter()
        .filter_map(|name| std::env::var_os(name).map(|value| (OsString::from(name), value)))
        .collect::<BTreeMap<_, _>>();
    if !dry_run {
        let missing = missing_prerequisites(&env, skip_browser);
        if !missing.is_empty() {
            return Err(format!(
                "gate: missing prerequisite variable(s): {}",
                missing.join(", ")
            )
            .into());
        }
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    for step in commands(skip_browser) {
        let display = step.display(&cargo);
        println!("{display}");
        if dry_run {
            continue;
        }
        let status = Command::new(&cargo)
            .args(&step.args)
            .envs(step.env.iter().copied())
            .current_dir(crate::repo_root())
            .status()
            .map_err(|error| format!("gate: could not start {}: {display}: {error}", step.name))?;
        if !status.success() {
            eprintln!("gate: FAILED: {}: {display} ({status})", step.name);
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    struct CiCommand {
        args: Vec<String>,
        env: BTreeMap<String, String>,
    }

    // Only the workflow's literal scalars and shell words are needed here. Reject unsupported
    // syntax rather than silently omitting a new check. No YAML or shell dependency is needed.
    fn scalar(value: &str) -> &str {
        let value = value.trim();
        value
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
            .or_else(|| {
                value
                    .strip_prefix('"')
                    .and_then(|value| value.strip_suffix('"'))
            })
            .unwrap_or(value)
    }

    fn indent(line: &str) -> usize {
        line.len() - line.trim_start().len()
    }

    fn environment(lines: &[&str], depth: usize) -> BTreeMap<String, String> {
        let Some(start) = lines
            .iter()
            .position(|line| indent(line) == depth && line.trim() == "env:")
        else {
            return BTreeMap::new();
        };
        lines[start + 1..]
            .iter()
            .take_while(|line| line.trim().is_empty() || indent(line) > depth)
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            .map(|line| {
                assert_eq!(
                    indent(line),
                    depth + 2,
                    "unsupported CI environment: {line}"
                );
                let (key, value) = line.trim().split_once(':').unwrap();
                (key.to_owned(), scalar(value).to_owned())
            })
            .collect()
    }

    fn shell_words(command: &str) -> Vec<String> {
        let mut words = Vec::new();
        let mut word = String::new();
        let mut quote = None;
        for character in command.chars() {
            match character {
                '\'' | '"' if quote == Some(character) => quote = None,
                '\'' | '"' if quote.is_none() => quote = Some(character),
                character if character.is_whitespace() && quote.is_none() => {
                    if !word.is_empty() {
                        words.push(std::mem::take(&mut word));
                    }
                }
                character => word.push(character),
            }
        }
        assert!(quote.is_none(), "unclosed quote in CI command: {command}");
        if !word.is_empty() {
            words.push(word);
        }
        words
    }

    fn normalize_command(command: &str, mut env: BTreeMap<String, String>) -> CiCommand {
        let mut words = shell_words(command).into_iter().peekable();
        if words.peek().is_some_and(|word| word == "env") {
            words.next();
        }
        while let Some((key, value)) = words.peek().and_then(|word| word.split_once('=')) {
            env.insert(key.to_owned(), value.to_owned());
            words.next();
        }
        let cargo = words.next().expect("CI command must invoke cargo");
        assert_eq!(
            std::path::Path::new(&cargo).file_name().unwrap(),
            "cargo",
            "unsupported CI command: {command}"
        );
        CiCommand {
            args: words.collect(),
            env,
        }
    }

    fn ci_commands(workflow: &str) -> Vec<CiCommand> {
        // These run steps prepare prerequisites or report change-base diagnostics. The gate
        // requires a prebuilt server and browser, and has no GitHub change-base context.
        // Each exclusion permits only these exact cargo commands, in order. Other shell lines
        // are ignored. Action-only steps (checkout, toolchain install and cache) have no run commands.
        let mut excluded = [
            (
                "rust",
                "Report MCP surface paths changed since the change base",
                &[
                    "MCP_SURFACE_BASE=\"$base\" cargo test --locked --test mcp_surface -- --nocapture",
                ][..],
                false,
            ),
            (
                "preview-renderer",
                "Download the pinned chrome-headless-shell from preview-renderer/Dockerfile",
                &[][..],
                false,
            ),
            (
                "preview-renderer",
                "Allow the unprivileged user namespaces that the CHROME_SANDBOX tests need",
                &[][..],
                false,
            ),
            (
                "browser",
                "cargo build --release --locked",
                &["cargo build --release --locked"][..],
                false,
            ),
            (
                "browser",
                "Download the pinned chrome-headless-shell from preview-renderer/Dockerfile",
                &[][..],
                false,
            ),
        ];
        let lines: Vec<_> = workflow.lines().collect();
        let mut commands = Vec::new();
        for job in ["rust", "preview-renderer", "browser"] {
            let header = format!("  {job}:");
            let start = lines.iter().position(|line| *line == header).unwrap();
            let end = (start + 1..lines.len())
                .find(|&index| {
                    let line = lines[index];
                    !line.trim().is_empty()
                        && !line.trim_start().starts_with('#')
                        && indent(line) <= 2
                })
                .unwrap_or(lines.len());
            let job_lines = &lines[start + 1..end];
            let job_env = environment(job_lines, 4);
            let mut starts: Vec<_> = job_lines
                .iter()
                .enumerate()
                .filter(|(_, line)| line.starts_with("      - "))
                .map(|(index, _)| index)
                .collect();
            starts.push(job_lines.len());
            for bounds in starts.windows(2) {
                let step = &job_lines[bounds[0]..bounds[1]];
                let run = step.iter().enumerate().find_map(|(index, line)| {
                    line.strip_prefix("      - run:")
                        .or_else(|| line.strip_prefix("        run:"))
                        .map(|value| (index, value.trim()))
                });
                let Some((index, value)) = run else {
                    assert!(
                        step[0].starts_with("      - uses:"),
                        "unsupported CI step: {step:?}"
                    );
                    continue;
                };
                let name = step[0]
                    .strip_prefix("      - name:")
                    .map(scalar)
                    .unwrap_or(value);
                let mut env = job_env.clone();
                env.extend(environment(step, 8));
                let run_lines: Vec<_> = if value == "|" {
                    step[index + 1..]
                        .iter()
                        .take_while(|line| line.trim().is_empty() || indent(line) > 8)
                        .map(|line| line.trim())
                        .filter(|line| !line.is_empty() && !line.starts_with('#'))
                        .collect()
                } else {
                    vec![scalar(value)]
                };
                if let Some((_, _, allowed, used)) =
                    excluded
                        .iter_mut()
                        .find(|(excluded_job, excluded_name, _, _)| {
                            *excluded_job == job && *excluded_name == name
                        })
                {
                    assert!(!*used, "duplicate excluded CI step: {job}/{name}");
                    *used = true;
                    let actual: Vec<_> = run_lines
                        .iter()
                        .filter(|line| {
                            shell_words(line).iter().any(|word| {
                                std::path::Path::new(word)
                                    .file_name()
                                    .is_some_and(|name| name == "cargo")
                            })
                        })
                        .map(|line| {
                            let command = normalize_command(line, env.clone());
                            assert!(
                                allowed.iter().any(|allowed| {
                                    normalize_command(allowed, env.clone()) == command
                                }),
                                "unexpected cargo command in excluded CI step {job}/{name}: {line}"
                            );
                            command
                        })
                        .collect();
                    let expected: Vec<_> = allowed
                        .iter()
                        .map(|line| normalize_command(line, env.clone()))
                        .collect();
                    assert_eq!(
                        actual, expected,
                        "excluded CI commands changed: {job}/{name}"
                    );
                    continue;
                }
                commands.extend(
                    run_lines
                        .iter()
                        .map(|line| normalize_command(line, env.clone())),
                );
            }
        }
        for (job, name, _, used) in excluded {
            assert!(used, "unused CI exclusion: {job}/{name}");
        }
        commands
    }

    #[test]
    fn command_list_matches_workflow() {
        let workflow = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/ci.yml"),
        )
        .unwrap();
        let actual: Vec<_> = commands(false)
            .iter()
            .map(|step| normalize_command(&step.display(OsStr::new("cargo")), BTreeMap::new()))
            .collect();
        assert_eq!(actual, ci_commands(&workflow));
    }

    #[test]
    fn workflow_block_commands_and_inline_environment_are_normalized() {
        let workflow = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.github/workflows/ci.yml"),
        )
        .unwrap();
        let block = workflow.replace(
            "      - run: cargo fmt --all --check",
            "      - run: |\n          cargo   fmt --all --check\n          env RUSTDOCFLAGS=\"-D warnings\" /usr/bin/cargo doc --workspace --no-deps --locked",
        );
        let mut expected = ci_commands(&workflow);
        expected.insert(
            1,
            normalize_command(
                "RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked",
                BTreeMap::new(),
            ),
        );
        assert_eq!(ci_commands(&block), expected);
    }

    #[test]
    #[should_panic(
        expected = "unexpected cargo command in excluded CI step preview-renderer/Download the pinned chrome-headless-shell from preview-renderer/Dockerfile: cargo test --workspace --locked"
    )]
    fn cargo_command_added_to_excluded_step_is_rejected() {
        let workflow = include_str!("../../.github/workflows/ci.yml").replacen(
            "      - name: Allow the unprivileged user namespaces that the CHROME_SANDBOX tests need",
            "          cargo test --workspace --locked\n      - name: Allow the unprivileged user namespaces that the CHROME_SANDBOX tests need",
            1,
        );
        ci_commands(&workflow);
    }

    #[test]
    #[should_panic(expected = "assertion `left == right` failed")]
    fn changed_mirrored_argument_is_rejected() {
        let workflow = include_str!("../../.github/workflows/ci.yml").replacen(
            "cargo test --workspace --all-targets --locked",
            "cargo test --workspace --all-targets --offline",
            1,
        );
        let actual: Vec<_> = commands(false)
            .iter()
            .map(|step| normalize_command(&step.display(OsStr::new("cargo")), BTreeMap::new()))
            .collect();
        assert_eq!(actual, ci_commands(&workflow));
    }

    #[test]
    #[should_panic(expected = "unused CI exclusion: browser/cargo build --release --locked")]
    fn renamed_excluded_step_is_rejected() {
        let workflow = include_str!("../../.github/workflows/ci.yml").replacen(
            "      - run: cargo build --release --locked",
            "      - name: Build the server\n        run: cargo build --release --locked",
            1,
        );
        ci_commands(&workflow);
    }

    #[test]
    fn command_list_matches_ci_order_and_flags() {
        let actual: Vec<_> = commands(false)
            .iter()
            .map(|step| step.display(OsStr::new("cargo")))
            .collect();
        assert_eq!(
            actual,
            [
                "cargo fmt --all --check",
                "cargo clippy --workspace --all-targets --all-features --locked -- -D warnings",
                "cargo clippy --workspace --locked -- -D warnings",
                "cargo test --workspace --all-targets --locked",
                "cargo test --doc --locked",
                "RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked",
                "cargo xtask historical-fixture --verify-against-committed",
                "CI=1 cargo fmt --manifest-path preview-renderer/Cargo.toml --check",
                "CI=1 cargo clippy --manifest-path preview-renderer/Cargo.toml --all-targets --locked -- -D warnings",
                "CI=1 cargo test --manifest-path preview-renderer/Cargo.toml --locked",
                "CI=1 RUSTDOCFLAGS='-D warnings' cargo doc --manifest-path preview-renderer/Cargo.toml --no-deps --locked",
                "CI=1 cargo fmt --manifest-path browser-tests/Cargo.toml --check",
                "CI=1 cargo clippy --manifest-path browser-tests/Cargo.toml --all-targets --locked -- -D warnings",
                "CI=1 cargo test --manifest-path browser-tests/Cargo.toml --locked",
                "CI=1 RUSTDOCFLAGS='-D warnings' cargo doc --manifest-path browser-tests/Cargo.toml --no-deps --locked",
            ]
        );
    }

    #[test]
    fn skip_browser_keeps_all_other_checks() {
        let full = commands(false);
        assert_eq!(commands(true), full[..11]);
    }

    #[test]
    fn prerequisites_name_each_missing_or_empty_variable() {
        let mut env = BTreeMap::new();
        assert_eq!(
            missing_prerequisites(&env, false),
            ["CHROME_HEADLESS_SHELL", "ARTIFACT_MCP_BIN"]
        );
        assert_eq!(missing_prerequisites(&env, true), ["CHROME_HEADLESS_SHELL"]);
        env.insert("CHROME_HEADLESS_SHELL".into(), "/browser".into());
        assert_eq!(missing_prerequisites(&env, false), ["ARTIFACT_MCP_BIN"]);
        assert!(missing_prerequisites(&env, true).is_empty());
        env.insert("ARTIFACT_MCP_BIN".into(), "".into());
        assert_eq!(missing_prerequisites(&env, false), ["ARTIFACT_MCP_BIN"]);
        env.insert("ARTIFACT_MCP_BIN".into(), "/server".into());
        assert!(missing_prerequisites(&env, false).is_empty());
        env.remove(OsStr::new("CHROME_HEADLESS_SHELL"));
        assert_eq!(
            missing_prerequisites(&env, false),
            ["CHROME_HEADLESS_SHELL"]
        );
    }

    #[test]
    fn display_quotes_cargo_paths_without_changing_the_command() {
        assert_eq!(
            commands(false)[0].display(OsStr::new("/some path/cargo")),
            "'/some path/cargo' fmt --all --check"
        );
        assert_eq!(shell_word("a'b"), "'a'\\''b'");
    }
}
