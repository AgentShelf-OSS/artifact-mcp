//! `cargo xtask release verify-manifest <release-manifest.json>`, ported from
//! `scripts/release/verify-manifest.mjs`.
//!
//! Accepts exactly the manifests the Node verifier accepted, with the same messages and exit
//! codes (0 verified, 1 rejected), except in four places where this port is stricter:
//!
//! - `release.commit`, `image.digest` and each attestation `reference` must be JSON strings. The
//!   Node verifier tested them with a regular expression, which converts an array such as
//!   `["sha256:..."]` to its string and accepted it.
//! - The attestation reference must name the repository literally; the Node expression left `.`
//!   in the repository unescaped, so it matched any character.
//! - The historical fixture preflight report is hashed and parsed from one read, so the parsed
//!   report is always the hashed one.
//! - JSON that `serde_json` rejects but `JSON.parse` accepts (unpaired UTF-16 surrogate escapes,
//!   numbers beyond the `f64` range, nesting deeper than 128) is rejected as unreadable.

use std::path::{Path, PathBuf};

use super::js::{self, Js, get, string};
use super::{format, resolve, sha256_hex};
use crate::Result;

/// A failed verification; the message follows `release manifest verification failed: `.
struct Rejected(String);

fn reject<T>(message: impl Into<String>) -> std::result::Result<T, Rejected> {
    Err(Rejected(message.into()))
}

pub fn run(args: &[String]) -> Result<bool> {
    match verify(args) {
        Ok(summary) => {
            println!("{summary}");
            Ok(true)
        }
        Err(Rejected(message)) => {
            eprintln!("release manifest verification failed: {message}");
            Ok(false)
        }
    }
}

/// Check one `{ path, sha256 }` record against the file beside the manifest; returns its bytes.
fn verify_hashed_artifact(
    directory: &Path,
    item: Option<&Js>,
    label: &str,
) -> std::result::Result<Vec<u8>, Rejected> {
    let path = string(get(item, "path")).filter(|path| format::is_safe_artifact_path(path));
    // `/^[a-f0-9]{64}$/.test(item.sha256 ?? "")` converts the value to a string first.
    let sha256 = get(item, "sha256").filter(|value| !matches!(value, Js::Null));
    let sha256_text = sha256.map_or_else(String::new, |value| js::to_js_string(Some(value)));
    let Some(path) = path.filter(|_| format::is_sha256_hex(&sha256_text)) else {
        return reject(format!("invalid {label} digest record"));
    };
    let bytes = match std::fs::read(directory.join(path)) {
        Ok(bytes) => bytes,
        Err(error) => return reject(format!("cannot hash {label}: {error}")),
    };
    // Strict equality: a non-string sha256 never matches.
    if string(sha256) != Some(sha256_hex(&bytes).as_str()) {
        return reject(format!("digest mismatch for {label}"));
    }
    Ok(bytes)
}

fn verify(args: &[String]) -> std::result::Result<String, Rejected> {
    let [manifest_arg] = args else {
        return reject("usage: cargo xtask release verify-manifest <release-manifest.json>");
    };
    let manifest_path = match resolve(manifest_arg) {
        Ok(path) => path,
        Err(error) => return reject(format!("cannot read JSON: {error}")),
    };
    let directory = manifest_path
        .parent()
        .map_or_else(|| PathBuf::from("/"), Path::to_path_buf);
    let manifest = std::fs::read(&manifest_path)
        .map_err(|error| error.to_string())
        .and_then(|bytes| js::parse(&js::decode_utf8(&bytes)));
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(error) => return reject(format!("cannot read JSON: {error}")),
    };
    let manifest = Some(&manifest);

    if get(manifest, "schemaVersion") != Some(&Js::Number(2.0)) {
        return reject("unsupported schemaVersion");
    }

    let release = get(manifest, "release");
    // `RELEASE_TAG.test(release?.tag ?? "")` converts the value to a string first.
    let tag_value = get(release, "tag").filter(|value| !matches!(value, Js::Null));
    if !format::is_tag(&tag_value.map_or_else(String::new, |value| js::to_js_string(Some(value)))) {
        return reject("invalid tag");
    }
    let version_value = get(release, "version").filter(|value| !matches!(value, Js::Null));
    let version_text =
        version_value.map_or_else(String::new, |value| js::to_js_string(Some(value)));
    let (Some(tag), Some(version)) = (string(tag_value), string(version_value)) else {
        // A non-string tag or version can never equal `tag.slice(1)` strictly.
        return reject("tag/version mismatch");
    };
    if !format::is_version(&version_text) || &tag[1..] != version {
        return reject("tag/version mismatch");
    }
    if !string(get(release, "commit")).is_some_and(format::is_commit) {
        return reject("invalid commit");
    }
    if !string(get(release, "created")).is_some_and(format::is_canonical_created) {
        return reject("invalid created timestamp");
    }

    let image = get(manifest, "image");
    let Some((owner, name)) = string(get(image, "reference")).and_then(format::ghcr_reference)
    else {
        return reject("invalid GHCR image reference");
    };
    let reference = string(get(image, "reference")).expect("checked above");
    let repository_part = format!("ghcr.io/{owner}/{name}");
    let github_repository = format!("{owner}/{name}");
    let Some(digest) =
        string(get(image, "digest")).filter(|digest| format::is_image_digest(digest))
    else {
        return reject("invalid image digest");
    };
    if !reference.ends_with(&format!(":{tag}")) {
        return reject("image tag does not match release tag");
    }
    let immutable_reference = format!("{repository_part}@{digest}");
    if string(get(image, "immutableReference")) != Some(immutable_reference.as_str()) {
        return reject("immutable image reference does not match image identity");
    }

    let schema = get(manifest, "schema");
    let range = (
        js::safe_integer(get(schema, "minCompatible")),
        js::safe_integer(get(schema, "current")),
        js::safe_integer(get(schema, "maxCompatible")),
    );
    match range {
        (Some(min), Some(current), Some(max)) if min >= 0.0 && min <= current && current <= max => {
        }
        _ => return reject("invalid schema range"),
    }

    let binary = get(manifest, "binary");
    verify_hashed_artifact(&directory, binary, "binary")?;
    let sboms = match get(manifest, "sboms").and_then(Js::as_array) {
        Some(sboms) if sboms.len() == 2 => sboms,
        _ => return reject("missing SBOMs"),
    };
    for (item, kind) in sboms.iter().zip(["source", "image"]) {
        let item = Some(item);
        if string(get(item, "kind")) != Some(kind)
            || string(get(item, "format")) != Some("SPDX-2.3")
        {
            return reject("invalid SBOM record");
        }
        verify_hashed_artifact(&directory, item, &format!("{kind} SBOM"))?;
    }

    let validation = get(manifest, "validation");
    verify_hashed_artifact(
        &directory,
        get(validation, "migrationNotes"),
        "migration notes",
    )?;
    if string(get(validation, "historicalFixturePreflight"))
        != Some("exact-oci-historical-fixtures/v1")
    {
        return reject("unexpected fixture preflight state");
    }
    let report_bytes = verify_hashed_artifact(
        &directory,
        get(validation, "historicalFixturePreflightReport"),
        "historical fixture preflight report",
    )?;
    let report = match js::parse(&js::decode_utf8(&report_bytes)) {
        Ok(report) => report,
        Err(error) => {
            return reject(format!(
                "cannot parse historical fixture preflight report: {error}"
            ));
        }
    };
    let report = Some(&report);
    if get(report, "schemaVersion") != Some(&Js::Number(1.0)) || js::truthy(get(report, "error")) {
        return reject("historical fixture preflight did not complete cleanly");
    }
    if string(get(report, "image")) != Some(immutable_reference.as_str()) {
        return reject("historical fixture preflight image does not match immutable release image");
    }
    match get(report, "fixtures").and_then(Js::as_array) {
        Some(fixtures)
            if !fixtures.is_empty()
                && fixtures
                    .iter()
                    .all(|fixture| string(fixture.get("status")) == Some("passed")) => {}
        _ => return reject("historical fixture preflight has missing or failed cases"),
    }

    let attestations = match get(manifest, "attestations").and_then(Js::as_array) {
        Some(attestations) if attestations.len() == 2 => attestations,
        _ => return reject("missing provenance attestations"),
    };
    let (binary_attestation, image_attestation) = (Some(&attestations[0]), Some(&attestations[1]));
    let Some(repository) = string(get(binary_attestation, "repository"))
        .filter(|repository| format::is_repository(repository))
    else {
        return reject("invalid attestation repository");
    };
    if string(get(image_attestation, "repository")) != Some(repository) {
        return reject("invalid attestation repository");
    }
    if repository.to_ascii_lowercase() != github_repository {
        return reject("attestation repository does not match image repository");
    }
    let binary_path = string(get(binary, "path")).expect("verified as a safe artifact path");
    let expected = [
        (
            binary_attestation,
            "github-build-provenance",
            binary_path.to_owned(),
            format!("gh attestation verify {binary_path} --repo {repository}"),
        ),
        (
            image_attestation,
            "oci-build-provenance",
            immutable_reference.clone(),
            format!("gh attestation verify oci://{immutable_reference} --repo {repository}"),
        ),
    ];
    for (value, kind, subject, verification) in &expected {
        if string(get(*value, "kind")) != Some(*kind)
            || string(get(*value, "subject")) != Some(subject.as_str())
            || string(get(*value, "verification")) != Some(verification.as_str())
        {
            return reject("attestation does not bind to release identity");
        }
        if !string(get(*value, "reference"))
            .is_some_and(|reference| format::is_attestation_reference(reference, repository))
        {
            return reject("invalid attestation reference");
        }
    }

    Ok(format!("verified {tag} ({digest})"))
}
