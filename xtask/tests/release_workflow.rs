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
        "cargo --locked xtask release verify-fixtures-in-image \\\n            \"$IMAGE@$DIGEST\" \
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
    for name in ["prepare", "verify-ci"] {
        assert!(!job(&workflow, name).contains("xtask"), "{name}");
    }
    let candidate = job(&workflow, "candidate");
    let publish = job(&workflow, "publish");
    for job in [&candidate, &publish] {
        // Check out the tagged source, then build the tool before the first use.
        let checkout = position(
            job,
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1\n        with:\n          \
             ref: ${{ needs.prepare.outputs.tag }}",
        );
        assert!(checkout < position(job, BUILD));
        assert!(position(job, BUILD) < position(job, "xtask release"));
        assert!(position(job, BUILD) < position(job, "docker/login-action"));
    }
    // Candidate: the source SBOM describes the checkout before the image build, and the exact
    // digest passes the fixture preflight before the SBOM scan, checksums, and attestations.
    let order = [
        BUILD,
        "cargo --locked xtask release source-sbom",
        "docker/build-push-action",
        "docker rm --force artifact-mcp-release-candidate",
        "cargo --locked xtask release verify-fixtures-in-image",
        "docker cp \"$container:/artifact-mcp\" release/artifact-mcp",
        "anchore/sbom-action",
        "anchore/scan-action",
        "sha256sum --check SHA256SUMS",
        "actions/attest-build-provenance",
        "actions/upload-artifact",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&candidate, pair[0]) < position(&candidate, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }
    // Publish: promotion only after verification, the manifest only after promotion, and the
    // manifest verified before checksums and the draft release.
    let order = [
        BUILD,
        "Verify inputs and candidate artifacts",
        "gh attestation verify release/artifact-mcp",
        "Promote without rebuilding",
        "cargo --locked xtask release manifest",
        "cargo --locked xtask release verify-manifest release-manifest.json",
        "> SHA256SUMS",
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

/// The pinned attestation action; every `uses:` of it must carry exactly this reference.
const ATTEST: &str =
    "actions/attest-build-provenance@e8998f949152b193b063cb0ec769d69d929409be # v2";

#[test]
fn release_workflow_builds_checksums_and_attests_the_static_renderer() {
    let workflow = workflow();
    let candidate = job(&workflow, "candidate");
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
    const ATTEST_IMAGE: &str = "subject-digest: ${{ steps.build.outputs.digest }}";
    for needle in [
        RENDERER_BUILD,
        STATIC_CHECK,
        RENDERER_COPY,
        CANDIDATE_SUMS,
        ATTEST_RENDERER,
        "file --brief \"$renderer\"",
        "echo \"artifact-preview is not statically linked\" >&2; exit 1;",
    ] {
        assert_eq!(candidate.matches(needle).count(), 1, "{needle}");
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
    assert_eq!(candidate.matches(&renderer_step).count(), 1);

    // Candidate order: the image is exercised and its binary copied out, then the renderer is
    // built and checked; both binaries are checksummed before any attestation, and uploaded last.
    let order = [
        "docker cp \"$container:/artifact-mcp\" release/artifact-mcp",
        RENDERER_BUILD,
        STATIC_CHECK,
        RENDERER_COPY,
        "anchore/sbom-action",
        "anchore/scan-action",
        CANDIDATE_SUMS,
        "sha256sum --check SHA256SUMS",
        ATTEST_SERVER,
        ATTEST_RENDERER,
        ATTEST_IMAGE,
        "actions/upload-artifact",
    ];
    for pair in order.windows(2) {
        assert!(
            position(&candidate, pair[0]) < position(&candidate, pair[1]),
            "{} before {}",
            pair[0],
            pair[1]
        );
    }

    // Publish: both binaries' attestations are verified before promotion; the final checksums,
    // the published-asset check and the draft release all include the renderer.
    let verify_renderer =
        "gh attestation verify release/artifact-preview --repo \"$GITHUB_REPOSITORY\"";
    let order = [
        "(cd release && sha256sum --check SHA256SUMS)",
        "gh attestation verify release/artifact-mcp --repo \"$GITHUB_REPOSITORY\"",
        verify_renderer,
        "gh attestation verify \"oci://$IMAGE@$DIGEST\"",
        "Promote without rebuilding",
        "cargo --locked xtask release verify-manifest release-manifest.json",
        "sha256sum artifact-mcp artifact-preview source.spdx.json image.spdx.json \
         historical-fixture-preflight.json migration-notes.md release-manifest.json > SHA256SUMS",
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
    let manifest = &publish[position(&publish, "cargo --locked xtask release manifest")
        ..position(&publish, "cargo --locked xtask release verify-manifest")];
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
