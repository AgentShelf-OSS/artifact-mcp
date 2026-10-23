//! `cargo xtask` repository maintenance commands for artifact-mcp.
//!
//! ```text
//! cargo xtask historical-fixture [--schema N | --case NAME] [--out DIR] [--replace-frozen-fixtures]
//! cargo xtask historical-fixture --verify-against-committed [--schema N | --case NAME] [--out DIR]
//! cargo xtask release <subcommand> ...
//! cargo xtask cf-access-setup | tunnel | publish-data-artifact | benchmark-durability ...
//! ```

#![forbid(unsafe_code)]

mod fixture;
mod ops;
mod release;
mod verify;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use artifact_mcp::persistence::migrations::LATEST_SCHEMA_VERSION;

use crate::fixture::{CaseSpec, Outcome};

/// Error type for the maintenance commands; every message reaches the operator verbatim.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const TOP_USAGE: &str = "\
usage: cargo xtask <command> [args]

commands:
  historical-fixture     add or verify frozen historical database fixtures
  release                release evidence: manifest, source SBOM, verification
  cf-access-setup        bootstrap the Cloudflare Access application
  tunnel                 manage the Anthropic MCP tunnel profile
  publish-data-artifact  publish an HTML artifact with live-data bindings
  benchmark-durability   measure publish/update latency against an OCI image or local binary

Run `cargo xtask <command> --help` for the options of one command.";

const USAGE: &str = "\
usage: cargo xtask historical-fixture [--schema N | --case NAME] [--out DIR] [--replace-frozen-fixtures]
       cargo xtask historical-fixture --verify-against-committed [--schema N | --case NAME] [--out DIR]

Adds every missing conformance/fixtures/historical/boundary-vNN (NN = 0..=LATEST_SCHEMA_VERSION)
by running the current migration ledger through NN and seeding the synthetic rows and bodies.
An existing fixture is frozen evidence and is never rewritten without --replace-frozen-fixtures.

  --schema N                  only boundary-vNN
  --case NAME                 only NAME (a boundary or release-v24-durability-recovery)
  --out DIR                   write under DIR instead of conformance/fixtures/historical
  --replace-frozen-fixtures   rewrite selected fixtures that already exist
  --verify-against-committed  regenerate every boundary (and the current-ledger recovery case)
                              into a scratch directory (or --out DIR) and compare it with the
                              committed fixture; exits 1 on any difference except the
                              known frozen-draft objects of boundary-v31 and boundary-v36";

struct Options {
    selection: Option<CaseSpec>,
    out: Option<PathBuf>,
    replace: bool,
    verify: bool,
}

fn parse(args: &[String]) -> Result<Options> {
    let mut options = Options {
        selection: None,
        out: None,
        replace: false,
        verify: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String> {
            inline
                .clone()
                .or_else(|| iter.next().cloned())
                .ok_or_else(|| format!("{name} needs a value").into())
        };
        match flag {
            "--schema" => {
                let raw = value("--schema")?;
                let schema: i64 = raw
                    .parse()
                    .map_err(|_| format!("--schema expects an integer, got {raw}"))?;
                if !(0..=LATEST_SCHEMA_VERSION).contains(&schema) {
                    return Err(format!(
                        "--schema {schema} is outside 0..={LATEST_SCHEMA_VERSION} \
                         (LATEST_SCHEMA_VERSION in src/persistence/migrations.rs)"
                    )
                    .into());
                }
                set_selection(&mut options, CaseSpec::boundary(schema))?;
            }
            "--case" => {
                let name = value("--case")?;
                set_selection(&mut options, fixture::case_by_name(&name)?)?;
            }
            "--out" => options.out = Some(PathBuf::from(value("--out")?)),
            "--replace-frozen-fixtures" if inline.is_none() => options.replace = true,
            "--verify-against-committed" if inline.is_none() => options.verify = true,
            _ => return Err(format!("unknown argument {arg}\n\n{USAGE}").into()),
        }
    }
    if options.verify && options.replace {
        return Err(
            "--verify-against-committed never writes the corpus; drop --replace-frozen-fixtures"
                .into(),
        );
    }
    Ok(options)
}

fn set_selection(options: &mut Options, spec: CaseSpec) -> Result<()> {
    if options.selection.is_some() {
        return Err("pass at most one --schema or --case".into());
    }
    options.selection = Some(spec);
    Ok(())
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one directory below the repository root")
        .to_path_buf()
}

fn display(repo: &Path, path: &Path) -> String {
    path.strip_prefix(repo)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn same_directory(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn generate(repo: &Path, committed_root: &Path, options: Options) -> Result<()> {
    let out_root = options.out.unwrap_or_else(|| committed_root.to_path_buf());
    // The default run adds missing boundaries only. The current-ledger recovery case is built
    // only when named with --case.
    let cases = match options.selection {
        Some(spec) => vec![spec],
        None => (0..=LATEST_SCHEMA_VERSION)
            .map(CaseSpec::boundary)
            .collect(),
    };
    let mut kept = Vec::new();
    let mut wrote = 0_usize;
    for spec in &cases {
        match fixture::write_case(repo, &out_root, spec, options.replace)? {
            Outcome::KeptExisting(path) => kept.push(display(repo, &path)),
            Outcome::Wrote(generated) => {
                wrote += 1;
                println!(
                    "wrote {} (schema {}, artifacts.db sha256 {}, {} bod{})",
                    display(repo, &generated.dir),
                    spec.origin_schema,
                    generated.manifest.database.sha256,
                    generated.manifest.bodies.len(),
                    if generated.manifest.bodies.len() == 1 {
                        "y"
                    } else {
                        "ies"
                    }
                );
                for body in &generated.manifest.bodies {
                    println!("  artifacts/{} ({} bytes)", body.path, body.bytes);
                }
            }
        }
    }
    if kept.len() == 1 && cases.len() == 1 {
        println!(
            "kept {} (it exists; pass --replace-frozen-fixtures to rewrite it)",
            kept[0]
        );
    } else if !kept.is_empty() {
        println!(
            "kept {} existing fixture(s) under {} unchanged",
            kept.len(),
            display(repo, &out_root)
        );
    }
    println!(
        "historical-fixture: wrote {wrote}, kept {}, LATEST_SCHEMA_VERSION {LATEST_SCHEMA_VERSION}",
        kept.len()
    );
    Ok(())
}

fn verify_all(repo: &Path, committed_root: &Path, options: Options) -> Result<bool> {
    if let Some(out) = &options.out
        && (same_directory(out, committed_root)
            || std::env::current_dir()?
                .join(out)
                .starts_with(committed_root))
    {
        return Err("--out must not point into the committed corpus when verifying".into());
    }
    let cases = match options.selection {
        Some(spec) => vec![spec],
        None => fixture::generatable_cases(),
    };
    let scratch = verify::Scratch::new(options.out)?;
    let mut failed = 0_usize;
    let mut with_known = 0_usize;
    let mut masked = BTreeSet::new();
    let mut substituted = BTreeSet::new();
    let mut whitespace_only = BTreeSet::new();
    let mut known_divergences = BTreeSet::new();
    for spec in &cases {
        let report = verify::verify_case(repo, committed_root, &scratch.path, spec)?;
        let (known, unexpected) = report.classify();
        if unexpected.is_empty() && !known.is_empty() {
            with_known += 1;
        }
        if unexpected.is_empty() {
            println!(
                "{}: {} (masked {} clock column(s); provenance fields differ: {})",
                report.name,
                if known.is_empty() {
                    "match"
                } else {
                    "match except known frozen-draft objects"
                },
                report.masked_columns.len(),
                if report.provenance_substitutions.is_empty() {
                    "none".to_owned()
                } else {
                    report
                        .provenance_substitutions
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                }
            );
        } else {
            failed += 1;
            println!("{}: MISMATCH", report.name);
            for difference in &unexpected {
                println!("  - {}", difference.message);
            }
        }
        for (difference, reason) in known {
            known_divergences.insert(format!(
                "{} {}: {reason}",
                report.name,
                difference.object.as_deref().unwrap_or("-")
            ));
        }
        masked.extend(report.masked_columns);
        substituted.extend(report.provenance_substitutions);
        whitespace_only.extend(report.whitespace_only_sql);
    }
    println!();
    println!(
        "masked clock-default columns (DEFAULT reads the clock; the generator never writes them):"
    );
    for column in &masked {
        println!("  {column}");
    }
    println!("sqlite_master SQL equal only after whitespace normalization:");
    for object in &whitespace_only {
        println!("  {object}");
    }
    if !known_divergences.is_empty() {
        println!(
            "known frozen-draft divergences (committed boundary predates the final migration DDL):"
        );
        for divergence in &known_divergences {
            println!("  {divergence}");
        }
    }
    println!("fixture.json fields excluded as provenance:");
    for (field, reason) in verify::PROVENANCE_FIELDS {
        let seen = if substituted.contains(*field) {
            "differed"
        } else {
            "identical"
        };
        println!("  {field} [{seen}]: {reason}");
    }
    if scratch.keep {
        println!("generated cases kept under {}", scratch.path.display());
    }
    println!(
        "historical-fixture verify: {}/{} case(s) match the committed corpus ({} exactly, {} \
         except known frozen-draft objects, {failed} mismatched)",
        cases.len() - failed,
        cases.len(),
        cases.len() - failed - with_known,
        with_known
    );
    Ok(failed == 0)
}

fn run() -> Result<bool> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some((command, rest)) = args.split_first() else {
        return Err(TOP_USAGE.into());
    };
    match command.as_str() {
        "-h" | "--help" | "help" => {
            println!("{TOP_USAGE}");
            Ok(true)
        }
        "historical-fixture" => historical_fixture(rest),
        "release" => release::run(rest),
        "cf-access-setup" | "tunnel" | "publish-data-artifact" | "benchmark-durability" => {
            ops::run(command, rest)
        }
        other => Err(format!("unknown command {other}\n\n{TOP_USAGE}").into()),
    }
}

fn historical_fixture(rest: &[String]) -> Result<bool> {
    if rest.iter().any(|arg| arg == "-h" || arg == "--help") {
        println!("{USAGE}");
        return Ok(true);
    }
    let options = parse(rest)?;
    let repo = repo_root();
    let committed_root = repo.join("conformance/fixtures/historical");
    if options.verify {
        verify_all(&repo, &committed_root, options)
    } else {
        generate(&repo, &committed_root, options)?;
        Ok(true)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::from(2)
        }
    }
}
