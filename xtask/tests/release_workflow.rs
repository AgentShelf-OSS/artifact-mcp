//! Properties of `.github/workflows/release-provenance.yml` and `docs/release-provenance.md`
//! (ported from `test/release-workflow.test.js`).

mod release_support;

use release_support::repo_root;

fn read(relative: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|error| panic!("{relative}: {error}"))
}

fn workflow() -> String {
    read(".github/workflows/release-provenance.yml")
}

/// The text of one job: its `  <name>:` line and every following blank or deeper-indented line.
fn job(workflow: &str, name: &str) -> String {
    let header = format!("  {name}:");
    let mut lines = workflow.lines().skip_while(|line| *line != header);
    let first = lines.next().unwrap_or_else(|| panic!("job {name}"));
    let body = lines
        .take_while(|line| line.is_empty() || line.starts_with("    "))
        .collect::<Vec<_>>();
    format!("{first}\n{}", body.join("\n"))
}

#[track_caller]
fn position(text: &str, needle: &str) -> usize {
    text.find(needle)
        .unwrap_or_else(|| panic!("missing {needle:?}"))
}

#[test]
fn public_release_workflow_stays_independent_of_homelab_deployment() {
    let workflow = workflow();
    for deployment_gate in [
        "trusted-release-signers",
        "git verify-tag",
        "backup_identity",
        "rollback_image",
        "RELEASE_BACKUP_IDENTITY",
        "RELEASE_ROLLBACK_IMAGE",
    ] {
        assert!(
            !workflow.contains(deployment_gate),
            "{deployment_gate} must not block an OSS release"
        );
    }
    assert!(
        !workflow
            .split("environment:")
            .skip(1)
            .any(|rest| rest.trim_start().starts_with("production"))
    );

    assert!(workflow.contains("Release tags must be annotated"));
    // Cargo.toml is the single release version source; package.json carries no version.
    assert!(workflow.contains("Tag $tag does not match Cargo.toml version $version"));
    assert!(!workflow.contains("package.json"));
    assert!(workflow.contains("Require successful CI for tagged commit"));
    assert!(workflow.contains("actions/workflows/ci.yml/runs?head_sha="));
    assert!(workflow.contains("status=completed"));
    assert!(!workflow.contains("status=success"));
    assert!(workflow.contains("git merge-base --is-ancestor"));
    assert!(workflow.contains("docker/build-push-action"));
    assert!(
        workflow
            .contains("--env AUDIT_LEDGER_HMAC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
    );
    assert!(workflow.contains(
        "--tmpfs /data-rust:rw,nosuid,nodev,noexec,size=64m,mode=1777,uid=65532,gid=65532"
    ));
    assert!(!workflow.contains("docker run --detach --rm --name artifact-mcp-release-candidate"));
    assert!(workflow.contains("docker rm --force artifact-mcp-release-candidate"));
    assert!(workflow.contains(
        "cargo --locked xtask release verify-fixtures-in-image \\\n            \
         --record-as \"$RELEASE_IMAGE@$DIGEST\" \\\n            \"$IMAGE@$DIGEST\" \
         release/historical-fixture-preflight.json"
    ));
    assert!(workflow.contains("anchore/sbom-action"));
    assert!(workflow.contains("anchore/scan-action"));
    // The server binary, the preview renderer binary, and the OCI image digest.
    assert_eq!(
        workflow.matches("actions/attest-build-provenance").count(),
        3
    );
    assert!(workflow.contains("sha256sum --check SHA256SUMS"));
    assert!(workflow.contains("tag_name: ${{ needs.prepare.outputs.tag }}"));
    assert!(workflow.contains("prerelease: ${{ contains(needs.prepare.outputs.tag, '-') }}"));
    assert!(workflow.contains("draft: true"));
    assert!(workflow.contains("gh release edit \"$TAG\" --draft=false"));
    assert!(workflow.contains("if: steps.release_state.outputs.published != 'true'"));
    assert!(!workflow.contains("make_latest: ${{ !contains(needs.prepare.outputs.tag, '-') }}"));
    assert!(workflow.lines().any(|line| {
        line.contains("Existing release image digest ")
            && line.contains(" does not match candidate")
    }));
    for asset in [
        "artifact-mcp",
        "artifact-preview",
        "SHA256SUMS",
        "release-manifest.json",
        "source.spdx.json",
        "image.spdx.json",
        "historical-fixture-preflight.json",
        "migration-notes.md",
    ] {
        assert!(workflow.contains(&format!("release/{asset}")), "{asset}");
    }
}

#[test]
fn release_workflow_runs_the_locked_rust_release_tooling() {
    let workflow = workflow();
    for retired in ["node ", "scripts/release/", ".mjs", "actions/setup-node"] {
        assert!(!workflow.contains(retired), "{retired}");
    }
    for command in [
        "cargo --locked xtask release source-sbom \\",
        "cargo --locked xtask release verify-fixtures-in-image \\",
        "cargo --locked xtask release manifest \\",
        "cargo --locked xtask release verify-manifest release-manifest.json",
    ] {
        assert_eq!(workflow.matches(command).count(), 1, "{command}");
    }
    assert_eq!(
        workflow.matches("xtask release").count(),
        4,
        "every xtask call is one of the commands above"
    );

    const BUILD: &str = "cargo build --locked --package xtask";
    for name in ["prepare", "verify-ci", "attest", "publish"] {
        assert!(!job(&workflow, name).contains("xtask"), "{name}");
    }
    let build = job(&workflow, "build");
    let attest = job(&workflow, "attest");
    let manifest = job(&workflow, "manifest");
    let publish = job(&workflow, "publish");
    for job in [&build, &manifest] {
        // Check out the tagged source without a persisted token, then build the tool before the
        // first use.
        let checkout = position(
            job,
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        with:\n          \
             ref: ${{ needs.prepare.outputs.tag }}",
        );
        assert!(checkout < position(job, "persist-credentials: false"));
        assert!(checkout < position(job, BUILD));
        assert!(position(job, BUILD) < position(job, "xtask release"));
    }
    // Build: the source SBOM describes the checkout before the image build, and the exact digest
    // passes the fixture preflight before the SBOM scan, the export, checksums, and upload.
    let order = [
        BUILD,
        "cargo --locked xtask release source-sbom",
        "docker/build-push-action",
        "docker rm --force artifact-mcp-release-candidate",
        "cargo --locked xtask release verify-fixtures-in-image",
        "docker cp \"$container:/artifact-mcp\" release/artifact-mcp",
        "anchore/sbom-action",
        "anchore/scan-action",
        "oci-archive:candidate-image/candidate.oci.tar",
        "sha256sum --check SHA256SUMS",
        "actions/upload-artifact",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&build, pair[0]) < position(&build, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }
    // Attest: the build job's checksums and the archive digest are checked before the push, and
    // the pushed index digest is checked before any attestation.
    let order = [
        "actions/download-artifact",
        "(cd release && sha256sum --check SHA256SUMS)",
        "Candidate archive $archived does not match tested digest $DIGEST",
        "docker/login-action",
        "skopeo copy --all --preserve-digests",
        "Pushed index $pushed does not match tested digest $DIGEST",
        "actions/attest-build-provenance",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&attest, pair[0]) < position(&attest, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }
    // Manifest: written from the attest job's outputs and verified before the final checksums.
    let order = [
        BUILD,
        "actions/download-artifact",
        "cargo --locked xtask release manifest",
        "${{ needs.attest.outputs.image_attestation }}",
        "cargo --locked xtask release verify-manifest release-manifest.json",
        "> SHA256SUMS",
        "actions/upload-artifact",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&manifest, pair[0]) < position(&manifest, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }
    // Publish: promotion only after verification, and the draft release after promotion.
    let order = [
        "Verify inputs and candidate artifacts",
        "gh attestation verify release/artifact-mcp",
        "Promote without rebuilding",
        "softprops/action-gh-release",
        "gh release edit",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&publish, pair[0]) < position(&publish, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }
}

/// The permissions block of one job, up to the next key at the same indentation.
fn permissions(job: &str) -> Vec<&str> {
    job.lines()
        .skip_while(|line| *line != "    permissions:")
        .skip(1)
        .take_while(|line| line.starts_with("      "))
        .map(str::trim)
        .collect()
}

/// Every job name, in file order.
fn job_names(workflow: &str) -> Vec<&str> {
    workflow
        .lines()
        .skip_while(|line| *line != "jobs:")
        .filter_map(|line| {
            let name = line.strip_prefix("  ")?.strip_suffix(':')?;
            (!name.starts_with(' ') && !name.is_empty()).then_some(name)
        })
        .collect()
}

/// What a job that holds a write permission must never contain: anything that fetches, builds, or
/// runs repository or dependency code.
const RUNS_CODE: &[&str] = &[
    "actions/checkout",
    "actions/cache",
    "anchore/",
    "cargo",
    "rustc",
    "xtask",
    "docker run ",
    "docker build ",
    "docker create ",
    "clone",
    "curl",
    "wget",
    "uses: ./",
    ".github/actions",
    "./release/",
    "gh release download",
];

/// What marks a job that compiles the tagged source and its dependencies.
const COMPILES: &[&str] = &["cargo", "docker/build-push-action"];

/// What a job that compiles must never reference.
const CREDENTIALS: &[&str] = &[
    "docker/login-action",
    "secrets.",
    "github.token",
    "GH_TOKEN",
    "id-token",
];

#[test]
fn release_jobs_that_hold_credentials_run_no_repository_code() {
    let workflow = workflow();
    let (mut credentialed, mut compiling) = (Vec::new(), Vec::new());
    for name in job_names(&workflow) {
        let job = job(&workflow, name);
        let permissions = permissions(&job);
        if permissions
            .iter()
            .any(|permission| permission.ends_with("write"))
        {
            credentialed.push(name);
            for code in RUNS_CODE {
                assert!(
                    !job.contains(code),
                    "{name} holds a write permission and runs {code}"
                );
            }
        }
        if COMPILES.iter().any(|marker| job.contains(marker)) {
            compiling.push(name);
            assert_eq!(permissions, ["contents: read"], "{name}");
            for credential in CREDENTIALS {
                assert!(
                    !job.contains(credential),
                    "{name} compiles and holds {credential}"
                );
            }
        }
    }
    assert_eq!(credentialed, ["attest", "publish"]);
    assert_eq!(compiling, ["build", "manifest"]);
    assert_eq!(
        permissions(&job(&workflow, "attest")),
        [
            "attestations: write",
            "contents: read",
            "id-token: write",
            "packages: write"
        ]
    );
    assert_eq!(
        permissions(&job(&workflow, "publish")),
        ["attestations: read", "contents: write", "packages: write"]
    );
    // The jobs that compile hold read access only, and their checkouts keep no token.
    let checkout = "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        \
                    with:\n          ref: ${{ needs.prepare.outputs.tag }}\n          \
                    # Dependency code runs in this job; leave no token in .git/config for it to \
                    read.\n          persist-credentials: false\n";
    for name in ["build", "manifest"] {
        let job = job(&workflow, name);
        assert_eq!(permissions(&job), ["contents: read"], "{name}");
        assert_eq!(job.matches("actions/checkout@").count(), 1, "{name}");
        assert_eq!(job.matches(checkout).count(), 1, "{name}");
    }

    // The build job tests the image on a loopback registry; only `attest` pushes to GHCR, and it
    // pushes the archived index with its digest unchanged.
    let build = job(&workflow, "build");
    assert!(build.contains(
        "image: registry:3@sha256:d4032d4cc323c1731c707931ecada07a53076eac38e3b61ae57533f170f2c6c3"
    ));
    assert!(build.contains("IMAGE: localhost:5000/artifact-mcp"));
    assert!(build.contains("driver-opts: network=host"));
    assert!(build.contains("[[ \"$archived\" == \"$DIGEST\" ]]"));
    let attest = job(&workflow, "attest");
    assert!(attest.contains("IMAGE: ${{ needs.prepare.outputs.image }}"));
    assert!(attest.contains("[[ \"$DIGEST\" =~ ^sha256:[0-9a-f]{64}$ ]]"));
    assert!(attest.contains("[[ \"$archived\" == \"$DIGEST\" ]]"));
    assert!(attest.contains(
        "skopeo copy --all --preserve-digests \\\n            \
         oci-archive:candidate-image/candidate.oci.tar \"docker://$IMAGE:candidate-$COMMIT\""
    ));
    assert!(attest.contains("[[ \"$pushed\" == \"$DIGEST\" ]]"));
    // Files downloaded after the attest job are checked against its output, and the manifest
    // an unprivileged job wrote must name the attested digest and binary.
    assert!(attest.contains(
        "echo \"candidate_sums=$(sha256sum < release/SHA256SUMS | cut -d' ' -f1)\" >> \
         \"$GITHUB_OUTPUT\""
    ));
    let publish = job(&workflow, "publish");
    assert!(publish.contains("CANDIDATE_SUMS: ${{ needs.attest.outputs.candidate_sums }}"));
    assert!(publish.contains("[[ \"$candidate_sums\" == \"$CANDIDATE_SUMS\" ]]"));
    assert!(publish.contains("'.image.digest == $digest and .binary.sha256 == $binary'"));
    assert!(
        position(&publish, "[[ \"$candidate_sums\" == \"$CANDIDATE_SUMS\" ]]")
            < position(&publish, "Promote without rebuilding")
    );
    // A rehearsal pushes under a separate image name and never reaches the publish job.
    assert!(workflow.contains("image=\"$image-rehearsal\""));
    assert!(publish.contains("if: ${{ !inputs.rehearse }}"));
}

/// The pinned attestation action; every `uses:` of it must carry exactly this reference.
const ATTEST: &str =
    "actions/attest-build-provenance@e8998f949152b193b063cb0ec769d69d929409be # v2";

#[test]
fn release_workflow_builds_checksums_and_attests_the_static_renderer() {
    let workflow = workflow();
    let build = job(&workflow, "build");
    let attest = job(&workflow, "attest");
    let manifest_job = job(&workflow, "manifest");
    let publish = job(&workflow, "publish");

    const RENDERER_BUILD: &str = "cargo build --release --locked --target x86_64-unknown-linux-musl \\\n            \
                                  --manifest-path preview-renderer/Cargo.toml \\\n            \
                                  --target-dir \"$RUNNER_TEMP/artifact-preview-target\"";
    const STATIC_CHECK: &str = "grep -Eq 'static-pie linked|statically linked' <<<\"$description\"";
    const RENDERER_COPY: &str = "cp \"$renderer\" release/artifact-preview";
    const CANDIDATE_SUMS: &str = "sha256sum artifact-mcp artifact-preview source.spdx.json \
                                  image.spdx.json historical-fixture-preflight.json > SHA256SUMS";
    const ATTEST_SERVER: &str = "subject-path: release/artifact-mcp";
    const ATTEST_RENDERER: &str = "subject-path: release/artifact-preview";
    const ATTEST_IMAGE: &str = "subject-digest: ${{ needs.build.outputs.image_digest }}";
    for needle in [
        RENDERER_BUILD,
        STATIC_CHECK,
        RENDERER_COPY,
        CANDIDATE_SUMS,
        "file --brief \"$renderer\"",
        "echo \"artifact-preview is not statically linked\" >&2; exit 1;",
    ] {
        assert_eq!(build.matches(needle).count(), 1, "{needle}");
    }
    for needle in [ATTEST_SERVER, ATTEST_RENDERER, ATTEST_IMAGE] {
        assert_eq!(attest.matches(needle).count(), 1, "{needle}");
    }

    // Every attestation uses the same pinned action, including the renderer's.
    assert_eq!(
        workflow.matches("actions/attest-build-provenance@").count(),
        3
    );
    assert_eq!(workflow.matches(ATTEST).count(), 3);
    let renderer_step = format!(
        "- id: attest_renderer\n        name: Attest the released preview renderer binary\n        \
         uses: {ATTEST}\n        with:\n          {ATTEST_RENDERER}"
    );
    assert_eq!(attest.matches(&renderer_step).count(), 1);

    // Build order: the image is exercised and its binary copied out, then the renderer is built
    // and checked; both binaries are checksummed and uploaded together.
    let order = [
        "docker cp \"$container:/artifact-mcp\" release/artifact-mcp",
        RENDERER_BUILD,
        STATIC_CHECK,
        RENDERER_COPY,
        "anchore/sbom-action",
        "anchore/scan-action",
        CANDIDATE_SUMS,
        "sha256sum --check SHA256SUMS",
        "actions/upload-artifact",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&build, pair[0]) < position(&build, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }
    // Attest order: the checksums are verified before any attestation.
    let order = [
        "(cd release && sha256sum --check SHA256SUMS)",
        ATTEST_SERVER,
        ATTEST_RENDERER,
        ATTEST_IMAGE,
    ];
    for pair in order.windows(2) {
        assert!(
            position(&attest, pair[0]) < position(&attest, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }

    // The final checksums include the renderer.
    assert_eq!(
        manifest_job
            .matches(
                "sha256sum artifact-mcp artifact-preview source.spdx.json image.spdx.json \
                 historical-fixture-preflight.json migration-notes.md release-manifest.json > \
                 SHA256SUMS"
            )
            .count(),
        1
    );

    // Publish: both binaries' attestations are verified before promotion; the published-asset
    // check and the draft release include the renderer.
    let verify_renderer =
        "gh attestation verify release/artifact-preview --repo \"$GITHUB_REPOSITORY\"";
    let order = [
        "(cd release && sha256sum --check SHA256SUMS)",
        "gh attestation verify release/artifact-mcp --repo \"$GITHUB_REPOSITORY\"",
        verify_renderer,
        "gh attestation verify \"oci://$IMAGE@$DIGEST\"",
        "Promote without rebuilding",
        "for asset in artifact-mcp artifact-preview SHA256SUMS release-manifest.json",
        "release/artifact-mcp\n            release/artifact-preview\n            release/SHA256SUMS",
        "gh release edit",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&publish, pair[0]) < position(&publish, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }
    assert_eq!(publish.matches(verify_renderer).count(), 1);

    // The release manifest format does not change: SHA256SUMS and the renderer's own attestation
    // bind it, and no xtask command names it.
    let manifest = &manifest_job[position(&manifest_job, "cargo --locked xtask release manifest")
        ..position(
            &manifest_job,
            "cargo --locked xtask release verify-manifest",
        )];
    assert!(!manifest.contains("artifact-preview"), "{manifest}");
    assert!(!manifest.contains("attest_renderer"), "{manifest}");
    assert_eq!(workflow.matches("xtask release").count(), 4);
}

#[test]
fn release_workflow_adds_no_third_party_action() {
    let workflow = workflow();
    let mut actions = workflow
        .lines()
        .filter_map(|line| {
            line.trim_start()
                .trim_start_matches("- ")
                .strip_prefix("uses: ")
        })
        .map(|action| action.split('@').next().unwrap())
        .collect::<Vec<_>>();
    actions.sort_unstable();
    actions.dedup();
    assert_eq!(
        actions,
        [
            "actions/attest-build-provenance",
            "actions/checkout",
            "actions/download-artifact",
            "actions/upload-artifact",
            "anchore/sbom-action",
            "anchore/scan-action",
            "docker/build-push-action",
            "docker/login-action",
            "docker/setup-buildx-action",
            "softprops/action-gh-release",
        ]
    );
}

#[test]
fn every_release_workflow_action_is_pinned_to_a_commit() {
    let workflow = workflow();
    let actions = workflow
        .lines()
        .filter_map(|line| {
            line.trim_start()
                .trim_start_matches("- ")
                .strip_prefix("uses: ")
        })
        .collect::<Vec<_>>();
    assert!(actions.len() >= 10);
    for action in actions {
        let (name, rest) = action.split_once('@').expect("uses: owner/action@ref");
        let reference = rest.split(" #").next().unwrap();
        assert!(
            reference.len() == 40
                && reference
                    .bytes()
                    .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{name} is pinned to {reference:?}, not a commit SHA"
        );
    }
}

#[test]
fn every_workflow_action_is_pinned_to_a_commit() {
    let root = repo_root().join(".github/workflows");
    let mut files = std::fs::read_dir(&root)
        .expect("read .github/workflows")
        .map(|entry| entry.expect("workflow entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "yml"))
        .collect::<Vec<_>>();
    files.sort();
    assert!(files.len() >= 4, "{files:?}");
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read workflow");
        for action in text.lines().filter_map(|line| {
            line.trim_start()
                .trim_start_matches("- ")
                .strip_prefix("uses: ")
        }) {
            let (name, rest) = action.split_once('@').expect("uses: owner/action@ref");
            let reference = rest.split(" #").next().unwrap();
            assert!(
                reference.len() == 40
                    && reference
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
                "{}: {name} is pinned to {reference:?}, not a commit SHA",
                file.display()
            );
        }
    }
}

#[test]
fn github_workflows_use_pinned_node_24_action_runtimes() {
    let combined = [
        ".github/workflows/ci.yml",
        ".github/workflows/dco.yml",
        ".github/workflows/release-provenance.yml",
    ]
    .map(read)
    .join("\n");
    assert!(!combined.contains("actions/checkout@v4"));
    assert!(!combined.contains("actions/setup-node@v4"));
    assert!(
        combined.contains("actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1")
    );
    for (index, _) in combined.match_indices("actions/setup-node@") {
        assert!(
            combined[index..].starts_with(
                "actions/setup-node@820762786026740c76f36085b0efc47a31fe5020 # v7.0.0"
            )
        );
    }
}

#[test]
fn release_documentation_keeps_deployment_evidence_local() {
    let documentation = read("docs/release-provenance.md");
    assert!(documentation.contains("Deployment boundary"));
    assert!(documentation.contains("operator's environment"));
    assert!(documentation.contains("Re-run failed jobs"));
    for private in [
        "trusted-release-signers",
        "RELEASE_BACKUP_IDENTITY",
        "RELEASE_ROLLBACK_IMAGE",
    ] {
        assert!(!documentation.contains(private), "{private}");
    }
    assert!(documentation.contains("cargo --locked xtask release verify-manifest"));
    // The renderer asset is documented with its own attestation check.
    assert!(documentation.contains(
        "gh attestation verify <download-dir>/artifact-preview --repo AgentShelf-OSS/artifact-mcp"
    ));
    assert!(documentation.contains("`release-manifest.json` does not name `artifact-preview`"));
    assert!(
        !documentation.contains("node scripts/release/verify-manifest.mjs release-manifest.json")
    );
}
