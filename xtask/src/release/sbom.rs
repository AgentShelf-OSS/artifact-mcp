//! `cargo xtask release source-sbom`, ported from `scripts/release/generate-source-sbom.mjs`.
//!
//! Writes a deterministic SPDX 2.3 source SBOM for the files committed at HEAD. The output is
//! byte-identical to the Node script for the same arguments and tree. One check is new: the
//! tracked files must match HEAD (`git status`), because the SBOM claims to describe that commit
//! and the workflow now compiles this tool before it hashes the tree.

use std::process::{Command, Stdio};

use super::js::{self, Js};
use super::{Failure, finish, format, parse_pairs, resolve, sha256_hex, wants_help, write_output};
use crate::Result;

const USAGE: &str = "usage: cargo xtask release source-sbom --out <path> --commit <40-hex> \
--version <version> --created <UTC ISO-8601> --repository <owner/repo>";

const OPTIONS: [&str; 5] = [
    "--out",
    "--commit",
    "--version",
    "--created",
    "--repository",
];

const PACKAGE_ID: &str = "SPDXRef-Package-artifact-mcp";

pub fn run(args: &[String]) -> Result<bool> {
    if wants_help(args) {
        println!("{USAGE}");
        return Ok(true);
    }
    finish(generate(args), USAGE)
}

fn require(value: &str, valid: fn(&str) -> bool, label: &str) -> std::result::Result<(), Failure> {
    if valid(value) {
        Ok(())
    } else {
        Err(Failure::Usage(format!("{label} has an invalid format")))
    }
}

/// `execFileSync("git", args)`: stdout, with stderr passed through; a non-zero exit fails.
fn git(args: &[&str]) -> std::result::Result<Vec<u8>, Failure> {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| Failure::Runtime(format!("cannot run git {}: {error}", args.join(" "))))?;
    if !output.status.success() {
        return Err(Failure::Runtime(format!(
            "Command failed: git {} ({})",
            args.join(" "),
            output.status
        )));
    }
    Ok(output.stdout)
}

fn generate(args: &[String]) -> std::result::Result<(), Failure> {
    let [out, commit, version, created, repository] =
        <[String; 5]>::try_from(parse_pairs(args, &OPTIONS)?).expect("five options");
    require(&commit, format::is_commit, "commit")?;
    require(&version, format::is_version, "version")?;
    require(&created, format::is_created_shape, "created")?;
    require(&repository, format::is_repository, "repository")?;
    let out = resolve(&out)?;
    let head = git(&["rev-parse", "HEAD"])?;
    if js::trim(&js::decode_utf8(&head)) != commit {
        return Err(Failure::Usage(
            "commit does not match checked-out source".to_owned(),
        ));
    }
    let changed = git(&[
        "--no-optional-locks",
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=no",
    ])?;
    if !changed.is_empty() {
        let paths = js::decode_utf8(&changed)
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .take(5)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Failure::Usage(format!(
            "tracked files differ from the checked-out commit ({paths})"
        )));
    }

    let listing = git(&["ls-files", "-z"])?;
    let mut files = js::decode_utf8(&listing)
        .split('\0')
        .filter(|file| !file.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    files.sort_by(|left, right| js::utf16_cmp(left, right));

    let mut file_entries = Vec::with_capacity(files.len());
    let mut relationships = vec![relationship("SPDXRef-DOCUMENT", "DESCRIBES", PACKAGE_ID)];
    for (index, file) in files.iter().enumerate() {
        let path = resolve(file)?;
        let contents = std::fs::read(&path).map_err(|error| {
            Failure::Runtime(format!("cannot read {}: {error}", path.display()))
        })?;
        let spdx_id = format!("SPDXRef-File-{}", index + 1);
        file_entries.push(Js::object([
            ("SPDXID", Js::str(&spdx_id)),
            ("fileName", Js::str(format!("./{file}"))),
            (
                "checksums",
                Js::Array(vec![Js::object([
                    ("algorithm", Js::str("SHA256")),
                    ("checksumValue", Js::str(sha256_hex(&contents))),
                ])]),
            ),
            ("licenseConcluded", Js::str("NOASSERTION")),
            ("copyrightText", Js::str("NOASSERTION")),
        ]));
        relationships.push(relationship(PACKAGE_ID, "CONTAINS", &spdx_id));
    }

    let document = Js::object([
        ("SPDXID", Js::str("SPDXRef-DOCUMENT")),
        ("spdxVersion", Js::str("SPDX-2.3")),
        ("name", Js::str(format!("artifact-mcp-source-{version}"))),
        ("dataLicense", Js::str("CC0-1.0")),
        (
            "documentNamespace",
            Js::str(format!(
                "https://github.com/{repository}/releases/source/{commit}"
            )),
        ),
        (
            "creationInfo",
            Js::object([
                (
                    "creators",
                    Js::Array(vec![Js::str("Tool: artifact-mcp-release-source-sbom")]),
                ),
                ("created", Js::str(&created)),
            ]),
        ),
        (
            "packages",
            Js::Array(vec![Js::object([
                ("SPDXID", Js::str(PACKAGE_ID)),
                ("name", Js::str("artifact-mcp")),
                ("versionInfo", Js::str(&version)),
                ("downloadLocation", Js::str("NOASSERTION")),
                ("filesAnalyzed", Js::Bool(true)),
                ("licenseConcluded", Js::str("Apache-2.0")),
                ("licenseDeclared", Js::str("Apache-2.0")),
                ("copyrightText", Js::str("NOASSERTION")),
                (
                    "externalRefs",
                    Js::Array(vec![Js::object([
                        ("referenceCategory", Js::str("OTHER")),
                        ("referenceType", Js::str("git")),
                        (
                            "referenceLocator",
                            Js::str(format!("git+https://github.com/{repository}@{commit}")),
                        ),
                    ])]),
                ),
            ])]),
        ),
        ("files", Js::Array(file_entries)),
        ("relationships", Js::Array(relationships)),
    ]);
    write_output(&out, &format!("{}\n", js::stringify_pretty(&document)))?;
    Ok(())
}

fn relationship(element: &str, kind: &str, related: &str) -> Js {
    Js::object([
        ("spdxElementId", Js::str(element)),
        ("relationshipType", Js::str(kind)),
        ("relatedSpdxElement", Js::str(related)),
    ])
}
