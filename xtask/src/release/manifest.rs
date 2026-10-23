//! `cargo xtask release manifest`, ported from `scripts/release/generate-manifest.mjs`.
//!
//! Builds `release-manifest.json` from arguments only, so ambient CI variables cannot become
//! unrecorded release inputs. The output is byte-identical to the Node script for the same
//! arguments and files.

use super::js::{self, Js};
use super::{Failure, finish, format, parse_pairs, resolve, sha256_hex, wants_help, write_output};
use crate::Result;

const REQUIRED: [&str; 18] = [
    "--out",
    "--tag",
    "--version",
    "--commit",
    "--created",
    "--binary",
    "--image-ref",
    "--image-digest",
    "--schema-min",
    "--schema-current",
    "--schema-max",
    "--source-sbom",
    "--image-sbom",
    "--migration-notes",
    "--fixture-preflight",
    "--repository",
    "--binary-attestation",
    "--image-attestation",
];

fn usage() -> String {
    format!(
        "usage: cargo xtask release manifest {}",
        REQUIRED
            .iter()
            .map(|key| format!("{key} <value>"))
            .collect::<Vec<_>>()
            .join(" ")
    )
}

pub fn run(args: &[String]) -> Result<bool> {
    if wants_help(args) {
        println!("{}", usage());
        return Ok(true);
    }
    finish(generate(args), &usage())
}

fn valid<'a>(
    value: &'a str,
    check: fn(&str) -> bool,
    label: &str,
) -> std::result::Result<&'a str, Failure> {
    if check(value) {
        Ok(value)
    } else {
        Err(Failure::Usage(format!("{label} has an invalid format")))
    }
}

/// `sha256(readFile(resolve(path)))`.
fn digest(path: &str) -> std::result::Result<Js, Failure> {
    let resolved = resolve(path)?;
    let bytes = std::fs::read(&resolved).map_err(|error| {
        Failure::Runtime(format!("cannot read {}: {error}", resolved.display()))
    })?;
    Ok(Js::str(sha256_hex(bytes)))
}

fn generate(args: &[String]) -> std::result::Result<(), Failure> {
    let input = parse_pairs(args, &REQUIRED)?;
    let arg = |key: &str| -> &str {
        let index = REQUIRED
            .iter()
            .position(|known| *known == key)
            .expect("a required option");
        &input[index]
    };
    let tag = valid(arg("--tag"), format::is_tag, "tag")?;
    let version = valid(arg("--version"), format::is_version, "version")?;
    if &tag[1..] != version {
        return Err(Failure::Usage(
            "tag must be v followed by version".to_owned(),
        ));
    }
    let commit = valid(arg("--commit"), format::is_commit, "commit")?;
    let created = valid(arg("--created"), format::is_created_shape, "created")?;
    let image_digest = valid(
        arg("--image-digest"),
        format::is_image_digest,
        "image digest",
    )?;
    let mut schema = [0.0_f64; 3];
    for (slot, key) in schema
        .iter_mut()
        .zip(["--schema-min", "--schema-current", "--schema-max"])
    {
        let parsed = js::string_to_number(arg(key));
        // `parsed < 0` is false for -0, which Node accepted and wrote as 0.
        if !js::is_safe_integer_number(parsed) || parsed < 0.0 {
            return Err(Failure::Usage(format!(
                "{key} must be a non-negative integer"
            )));
        }
        *slot = parsed;
    }
    if schema[0] > schema[1] || schema[1] > schema[2] {
        return Err(Failure::Usage(
            "schema range must satisfy min <= current <= max".to_owned(),
        ));
    }
    let repository = valid(arg("--repository"), format::is_repository, "repository")?;
    let binary_attestation = valid(
        arg("--binary-attestation"),
        format::is_github_url,
        "binary attestation",
    )?;
    let image_attestation = valid(
        arg("--image-attestation"),
        format::is_github_url,
        "image attestation",
    )?;

    let image_ref = arg("--image-ref");
    let binary = arg("--binary");
    let immutable_reference = format!("{}@{image_digest}", format::strip_image_tag(image_ref));
    let manifest = Js::object([
        ("schemaVersion", Js::Number(2.0)),
        (
            "release",
            Js::object([
                ("tag", Js::str(tag)),
                ("version", Js::str(version)),
                ("commit", Js::str(commit)),
                ("created", Js::str(created)),
            ]),
        ),
        (
            "image",
            Js::object([
                ("reference", Js::str(image_ref)),
                ("digest", Js::str(image_digest)),
                ("immutableReference", Js::str(&immutable_reference)),
            ]),
        ),
        (
            "binary",
            Js::object([("path", Js::str(binary)), ("sha256", digest(binary)?)]),
        ),
        (
            "schema",
            Js::object([
                ("minCompatible", Js::Number(schema[0])),
                ("current", Js::Number(schema[1])),
                ("maxCompatible", Js::Number(schema[2])),
            ]),
        ),
        (
            "sboms",
            Js::Array(vec![
                Js::object([
                    ("kind", Js::str("source")),
                    ("path", Js::str(arg("--source-sbom"))),
                    ("sha256", digest(arg("--source-sbom"))?),
                    ("format", Js::str("SPDX-2.3")),
                ]),
                Js::object([
                    ("kind", Js::str("image")),
                    ("path", Js::str(arg("--image-sbom"))),
                    ("sha256", digest(arg("--image-sbom"))?),
                    ("format", Js::str("SPDX-2.3")),
                ]),
            ]),
        ),
        (
            "attestations",
            Js::Array(vec![
                Js::object([
                    ("kind", Js::str("github-build-provenance")),
                    ("repository", Js::str(repository)),
                    ("subject", Js::str(binary)),
                    ("reference", Js::str(binary_attestation)),
                    (
                        "verification",
                        Js::str(format!(
                            "gh attestation verify {binary} --repo {repository}"
                        )),
                    ),
                ]),
                Js::object([
                    ("kind", Js::str("oci-build-provenance")),
                    ("repository", Js::str(repository)),
                    ("subject", Js::str(&immutable_reference)),
                    ("reference", Js::str(image_attestation)),
                    (
                        "verification",
                        Js::str(format!(
                            "gh attestation verify oci://{immutable_reference} --repo {repository}"
                        )),
                    ),
                ]),
            ]),
        ),
        (
            "validation",
            Js::object([
                (
                    "migrationNotes",
                    Js::object([
                        ("path", Js::str(arg("--migration-notes"))),
                        ("sha256", digest(arg("--migration-notes"))?),
                    ]),
                ),
                // `cargo xtask release verify-fixtures-in-image` boots every frozen fixture with
                // the exact OCI digest before this manifest is generated; its JSON report is
                // checksummed and attached beside this manifest.
                (
                    "historicalFixturePreflight",
                    Js::str("exact-oci-historical-fixtures/v1"),
                ),
                (
                    "historicalFixturePreflightReport",
                    Js::object([
                        ("path", Js::str(arg("--fixture-preflight"))),
                        ("sha256", digest(arg("--fixture-preflight"))?),
                    ]),
                ),
            ]),
        ),
    ]);
    write_output(
        &resolve(arg("--out"))?,
        &format!("{}\n", js::stringify_pretty(&manifest)),
    )?;
    Ok(())
}
