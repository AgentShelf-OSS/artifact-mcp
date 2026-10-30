//! `cargo xtask release verify-fixtures-in-image`, ported from
//! `scripts/release/verify-historical-fixtures-in-image.mjs`.
//!
//! Candidate-image historical-fixture preflight. In the release mode every frozen fixture is copied
//! and booted by the immutable OCI image, never by a host binary. Each boot performs authenticated
//! MCP reads; the richest recovery fixtures also prove a write, update and restore, and the
//! durability recovery. The JSON report is a release artifact, so the manifest's passed marker has
//! a machine-checkable execution record rather than a source-only assertion.
//!
//! `--binary PATH` runs the same copies, requests and checks against a native `artifact-mcp`
//! binary instead of a container, for hosts without a Docker daemon. Its report names the binary
//! (`local-binary:<path>@sha256:<digest>`), so `verify-manifest` never accepts it as release
//! evidence. `--fixtures DIR` (local mode only) selects another corpus, for tests.

use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::net::TcpListener;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use super::http::{self, Origin};
use super::js::{self, Js, get, string};
use super::{node_join, resolve, sha256_hex};
use crate::Result;

const USAGE: &str = "\
usage: cargo xtask release verify-fixtures-in-image IMAGE@sha256:DIGEST OUT.json
       cargo xtask release verify-fixtures-in-image --binary PATH [--fixtures DIR] OUT.json";

/// The fixed, public synthetic key material the frozen fixtures were written with.
const AUDIT_KEY_ENV: &str = "AUDIT_LEDGER_HMAC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
const WEBHOOK_KEY_ENV: &str = "WEBHOOK_ENC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

/// The environment the release image sets (`ENV` in the Dockerfile) that the server reads.
const IMAGE_RUST_LOG: &str = "artifact_mcp=info,tower_http=info";
const IMAGE_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const HEALTH_ATTEMPTS: usize = 45;
const HEALTH_INTERVAL: Duration = Duration::from_millis(500);

/// What boots each fixture copy.
enum Target {
    /// The exact OCI image, through the Docker CLI.
    Image(String),
    /// A native binary, for hosts without Docker.
    Binary(PathBuf),
}

struct Invocation {
    target: Target,
    /// The `image` field of the report.
    identity: String,
    fixtures: PathBuf,
    out: String,
}

pub fn run(args: &[String]) -> Result<bool> {
    if matches!(args, [only] if only == "-h" || only == "--help") {
        println!("{USAGE}");
        return Ok(true);
    }
    let outcome = parse(args).and_then(execute);
    match outcome {
        Ok(count) => {
            println!("candidate fixture preflight passed: {count} cases");
            Ok(true)
        }
        Err(message) => {
            eprintln!("{message}");
            Ok(false)
        }
    }
}

fn parse(args: &[String]) -> std::result::Result<Invocation, String> {
    let mut binary = None;
    let mut fixtures = None;
    let mut positional = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--binary" => binary = Some(iter.next().ok_or(USAGE)?.clone()),
            "--fixtures" => fixtures = Some(iter.next().ok_or(USAGE)?.clone()),
            _ => positional.push(arg.clone()),
        }
    }
    let committed = crate::repo_root().join("conformance/fixtures/historical");
    match (binary, positional.as_slice()) {
        (None, [image, out]) if image.contains("@sha256:") && fixtures.is_none() => {
            Ok(Invocation {
                target: Target::Image(image.clone()),
                identity: image.clone(),
                fixtures: committed,
                out: out.clone(),
            })
        }
        (Some(binary), [out]) => {
            let binary =
                fs::canonicalize(&binary).map_err(|error| format!("--binary {binary}: {error}"))?;
            let digest = fs::read(&binary)
                .map(sha256_hex)
                .map_err(|error| format!("--binary {}: {error}", binary.display()))?;
            let fixtures = match fixtures {
                Some(dir) => resolve(&dir).map_err(|error| error.to_string())?,
                None => committed,
            };
            Ok(Invocation {
                identity: format!("local-binary:{}@sha256:{digest}", binary.display()),
                target: Target::Binary(binary),
                fixtures,
                out: out.clone(),
            })
        }
        _ => Err(USAGE.to_owned()),
    }
}

fn execute(invocation: Invocation) -> std::result::Result<usize, String> {
    let names = fixture_names(&invocation.fixtures)?;
    let runtime_user = match &invocation.target {
        Target::Image(_) => Some(runtime_user()?),
        Target::Binary(_) => None,
    };
    let mut report = Js::object([
        ("schemaVersion", Js::Number(1.0)),
        ("image", Js::str(&invocation.identity)),
        (
            "command",
            Js::Array(std::env::args().map(Js::String).collect()),
        ),
        ("fixtures", Js::Array(Vec::new())),
    ]);
    let mut passed = Vec::new();
    for name in &names {
        match verify_fixture(&invocation, runtime_user.as_deref(), name) {
            Ok(evidence) => passed.push(evidence),
            Err(message) => {
                report.set("fixtures", Js::Array(passed));
                report.set("error", Js::str(error_string(&message)));
                write_report(&invocation.out, &report)?;
                return Err(message);
            }
        }
    }
    let count = passed.len();
    report.set("fixtures", Js::Array(passed));
    report.set("completedAt", Js::str(js::iso_now()));
    write_report(&invocation.out, &report)?;
    Ok(count)
}

/// `String(error)` for the errors the Node script threw.
fn error_string(message: &str) -> String {
    if message.starts_with("TypeError: ") || message.starts_with("SyntaxError: ") {
        message.to_owned()
    } else {
        format!("Error: {message}")
    }
}

fn write_report(out: &str, report: &Js) -> std::result::Result<(), String> {
    fs::write(out, format!("{}\n", js::stringify_pretty(report)))
        .map_err(|error| format!("cannot write {out}: {error}"))
}

/// `process.getuid():process.getgid()` (real IDs) for `docker run --user`.
fn runtime_user() -> std::result::Result<String, String> {
    let id = |flag: &str| -> std::result::Result<String, String> {
        let output = Command::new("id")
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("cannot run id {flag}: {error}"))?;
        let value = js::trim(&String::from_utf8_lossy(&output.stdout)).to_owned();
        if output.status.success() && !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit())
        {
            Ok(value)
        } else {
            Err(format!("id {flag} did not print a numeric id"))
        }
    };
    Ok(format!("{}:{}", id("-ru")?, id("-rg")?))
}

/// `readdirSync` order: libuv sorts directory entries by bytes.
fn sorted_entries(dir: &Path) -> std::result::Result<Vec<fs::DirEntry>, String> {
    let mut entries = fs::read_dir(dir)
        .and_then(|entries| entries.collect::<std::io::Result<Vec<_>>>())
        .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

/// Every `boundary-vNN` (contiguous from 00) plus exactly five `release-v*` fixtures.
fn fixture_names(root: &Path) -> std::result::Result<Vec<String>, String> {
    let mut names = Vec::new();
    for entry in sorted_entries(root)? {
        let is_dir = entry
            .file_type()
            .map_err(|error| format!("cannot stat {}: {error}", entry.path().display()))?
            .is_dir();
        if is_dir {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort_by(|left, right| js::utf16_cmp(left, right));
    let is_boundary = |name: &str| {
        name.strip_prefix("boundary-v")
            .is_some_and(|digits| digits.len() == 2 && digits.bytes().all(|b| b.is_ascii_digit()))
    };
    let boundaries = names
        .iter()
        .filter(|name| is_boundary(name))
        .collect::<Vec<_>>();
    let released = names
        .iter()
        .filter(|name| name.starts_with("release-v"))
        .count();
    if released != 5 || boundaries.is_empty() || names.len() != boundaries.len() + released {
        return Err(format!(
            "expected every boundary plus five released rich fixtures, found {}",
            names.len()
        ));
    }
    for (index, name) in boundaries.iter().enumerate() {
        if **name != format!("boundary-v{index:02}") {
            return Err(format!(
                "historical fixture boundaries are not contiguous at {name}"
            ));
        }
    }
    Ok(names)
}

/// The committed body list: every regular file under `artifacts/`, in `localeCompare` order.
fn body_manifest(source: &Path) -> std::result::Result<Js, String> {
    fn visit(
        base: &Path,
        dir: &Path,
        entries: &mut Vec<(String, usize, String)>,
    ) -> std::result::Result<(), String> {
        for entry in sorted_entries(dir)? {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
            if file_type.is_dir() {
                visit(base, &path, entries)?;
            } else if file_type.is_file() {
                let bytes = fs::read(&path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                let relative = path
                    .strip_prefix(base)
                    .expect("visited below the base")
                    .components()
                    .map(|part| part.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                entries.push((relative, bytes.len(), sha256_hex(&bytes)));
            }
        }
        Ok(())
    }
    let base = source.join("artifacts");
    let mut entries = Vec::new();
    visit(&base, &base, &mut entries)?;
    entries.sort_by(|left, right| js::locale_cmp(&left.0, &right.0));
    Ok(Js::Array(
        entries
            .into_iter()
            .map(|(path, bytes, sha256)| {
                Js::object([
                    ("path", Js::String(path)),
                    ("bytes", Js::Number(bytes as f64)),
                    ("sha256", Js::String(sha256)),
                ])
            })
            .collect(),
    ))
}

/// The database digest and body list the fixture commits to, checked against its files.
fn immutable_identity(source: &Path, manifest: &Js) -> std::result::Result<Js, String> {
    let manifest = Some(manifest);
    let label = || js::to_js_string(get(get(manifest, "origin"), "schemaVersion"));
    let database = get(manifest, "database");
    let Some(database_path) = string(get(database, "path")) else {
        return Err("TypeError: fixture.json database.path must be a string".to_owned());
    };
    let database_file = node_join(source, database_path);
    let database_bytes = fs::read(&database_file)
        .map_err(|error| format!("cannot read {}: {error}", database_file.display()))?;
    let database_sha256 = sha256_hex(&database_bytes);
    if string(get(database, "sha256")) != Some(database_sha256.as_str()) {
        return Err(format!("{}: source database digest changed", label()));
    }
    let bodies = js::stringify(&body_manifest(source)?);
    let committed = get(manifest, "bodies").map(js::stringify);
    let Some(committed) = committed.filter(|committed| *committed == bodies) else {
        return Err(format!("{}: source body manifest changed", label()));
    };
    Ok(Js::object([
        ("databaseSha256", Js::String(database_sha256)),
        ("bodyManifestSha256", Js::str(sha256_hex(committed))),
    ]))
}

/// A `mkdtemp` directory, removed (recursively, ignoring errors) on drop.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> std::result::Result<Self, String> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let base = std::env::temp_dir();
        for attempt in 0_u32..100 {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_u32(attempt);
            hasher.write_u32(std::process::id());
            hasher.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_nanos())
                    .unwrap_or_default(),
            );
            let mut seed = hasher.finish();
            let suffix = (0..6)
                .map(|_| {
                    let c = ALPHABET[(seed % ALPHABET.len() as u64) as usize];
                    seed /= ALPHABET.len() as u64;
                    char::from(c)
                })
                .collect::<String>();
            let path = base.join(format!("{prefix}{suffix}"));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(format!("cannot create {}: {error}", path.display())),
            }
        }
        Err(format!("cannot create a unique {prefix}* directory"))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// `cpSync(source, destination, { recursive: true })` into an existing directory.
fn copy_tree(source: &Path, destination: &Path) -> std::result::Result<(), String> {
    for entry in sorted_entries(source)? {
        let from = entry.path();
        let to = destination.join(entry.file_name());
        let failed = |error: std::io::Error| format!("cannot copy {}: {error}", from.display());
        let file_type = entry.file_type().map_err(failed)?;
        if file_type.is_dir() {
            fs::create_dir(&to).map_err(failed)?;
            copy_tree(&from, &to)?;
        } else if file_type.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(&from).map_err(failed)?, &to)
                .map_err(failed)?;
        } else {
            fs::copy(&from, &to).map_err(failed)?;
        }
    }
    Ok(())
}

/// `execFileSync(program, args)`: stdout trimmed, stderr passed through, non-zero exit fails.
fn run_checked(program: &str, args: &[String]) -> std::result::Result<String, String> {
    let command = format!("{program} {}", args.join(" "));
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("Command failed: {command}: {error}"))?;
    if !output.status.success() {
        return Err(format!("Command failed: {command} ({})", output.status));
    }
    Ok(js::trim(&String::from_utf8_lossy(&output.stdout)).to_owned())
}

/// `spawnSync(program, args).stdout`, or `None` when the command could not run or printed nothing.
fn capture(program: &str, args: &[&str]) -> Option<(bool, String)> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    Some((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

fn docker_run_args(
    container: &str,
    user: &str,
    data: &Path,
    token: &str,
    image: &str,
) -> Vec<String> {
    // The distroless image runs as nonroot. This temporary, synthetic copy is deliberately
    // writable while the committed source fixture remains untouched.
    [
        "run",
        "--detach",
        "--name",
        container,
        "--user",
        user,
        "--publish",
        "127.0.0.1::3480",
        "--volume",
        &format!("{}:/data-rust", data.display()),
        "--env",
        &format!("ARTIFACT_API_KEYS=fixture-key:fixture:{token}"),
        "--env",
        AUDIT_KEY_ENV,
        "--env",
        WEBHOOK_KEY_ENV,
        image,
    ]
    .map(str::to_owned)
    .to_vec()
}

fn docker_remove_args(container: &str) -> [&str; 3] {
    ["rm", "--force", container]
}

/// The environment of a native boot: the image `ENV` plus what `docker run` adds, on loopback.
fn process_env(port: u16, data: &Path, token: &str) -> Vec<(String, String)> {
    let pair = |assignment: &str| {
        let (name, value) = assignment.split_once('=').expect("NAME=value");
        (name.to_owned(), value.to_owned())
    };
    vec![
        ("PATH".to_owned(), IMAGE_PATH.to_owned()),
        ("PORT".to_owned(), port.to_string()),
        ("LISTEN_HOST".to_owned(), "127.0.0.1".to_owned()),
        ("DATA_DIR".to_owned(), data.display().to_string()),
        ("RUST_LOG".to_owned(), IMAGE_RUST_LOG.to_owned()),
        (
            "ARTIFACT_API_KEYS".to_owned(),
            format!("fixture-key:fixture:{token}"),
        ),
        pair(AUDIT_KEY_ENV),
        pair(WEBHOOK_KEY_ENV),
    ]
}

/// `/127\.0\.0\.1:(\d+)/` over `docker port` output.
fn published_port(output: &str) -> Option<u16> {
    const LOOPBACK: &str = "127.0.0.1:";
    output.match_indices(LOOPBACK).find_map(|(index, _)| {
        let rest = &output[index + LOOPBACK.len()..];
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (digits > 0)
            .then(|| rest[..digits].parse::<u16>().ok())
            .flatten()
    })
}

/// One booted candidate: a container, or a native process.
enum Server<'a> {
    Container {
        image: &'a str,
        name: String,
        user: &'a str,
    },
    Process {
        binary: &'a Path,
        name: String,
        log: PathBuf,
        child: Option<Child>,
    },
}

impl Server<'_> {
    fn name(&self) -> &str {
        match self {
            Self::Container { name, .. } | Self::Process { name, .. } => name,
        }
    }

    fn start(
        &mut self,
        fixture: &str,
        data: &Path,
        token: &str,
    ) -> std::result::Result<Origin, String> {
        match self {
            Self::Container { image, name, user } => {
                run_checked("docker", &docker_run_args(name, user, data, token, image))?;
                let port = capture("docker", &["port", name, "3480/tcp"])
                    .filter(|(success, _)| *success)
                    .and_then(|(_, stdout)| published_port(&stdout));
                match port {
                    Some(port) => Ok(Origin { port }),
                    None => Err(format!(
                        "{fixture}: unable to discover candidate port\n{}",
                        self.diagnostics()
                    )),
                }
            }
            Self::Process {
                binary, log, child, ..
            } => {
                let port = TcpListener::bind("127.0.0.1:0")
                    .and_then(|listener| listener.local_addr())
                    .map_err(|error| format!("cannot reserve a loopback port: {error}"))?
                    .port();
                let log_file = File::create(&*log)
                    .map_err(|error| format!("cannot create {}: {error}", log.display()))?;
                let log_copy = log_file
                    .try_clone()
                    .map_err(|error| format!("cannot share {}: {error}", log.display()))?;
                let spawned = Command::new(*binary)
                    .env_clear()
                    .envs(process_env(port, data, token))
                    .current_dir("/")
                    .stdin(Stdio::null())
                    .stdout(log_file)
                    .stderr(log_copy)
                    .spawn()
                    .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
                *child = Some(spawned);
                Ok(Origin { port })
            }
        }
    }

    /// The exit status of a native process that has stopped.
    fn exited(&mut self) -> Option<String> {
        match self {
            Self::Process {
                child: Some(child), ..
            } => child
                .try_wait()
                .ok()
                .flatten()
                .map(|status| status.to_string()),
            _ => None,
        }
    }

    /// `containerDiagnostics(container)`, or the native process status and log.
    fn diagnostics(&mut self) -> String {
        let status = self.exited();
        match self {
            Self::Container { name, .. } => {
                let output = |args: &[&str], fallback: &str| {
                    capture("docker", args)
                        .map(|(_, stdout)| stdout)
                        .filter(|stdout| !stdout.is_empty())
                        .unwrap_or_else(|| fallback.to_owned())
                };
                let inspect = output(&["inspect", name], "container no longer exists");
                let logs = output(&["logs", name], "no container logs");
                format!("inspect: {inspect}\nlogs: {logs}")
            }
            Self::Process { log, .. } => {
                let status = status.unwrap_or_else(|| "running or never started".to_owned());
                let logs = fs::read(&*log)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .ok()
                    .filter(|logs| !logs.is_empty())
                    .unwrap_or_else(|| "no process logs".to_owned());
                format!("process: {status}\nlogs: {logs}")
            }
        }
    }

    /// The `finally` block: remove the container, or stop the process; errors are ignored.
    fn stop(&mut self) {
        match self {
            Self::Container { name, .. } => {
                let _ = Command::new("docker")
                    .args(docker_remove_args(name))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            Self::Process { child, log, .. } => {
                if let Some(mut child) = child.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                let _ = fs::remove_file(&*log);
            }
        }
    }
}

fn verify_fixture(
    invocation: &Invocation,
    runtime_user: Option<&str>,
    name: &str,
) -> std::result::Result<Js, String> {
    let source = invocation.fixtures.join(name);
    let manifest_path = source.join("fixture.json");
    let manifest = fs::read(&manifest_path)
        .map_err(|error| format!("cannot read {}: {error}", manifest_path.display()))?;
    let manifest =
        js::parse(&js::decode_utf8(&manifest)).map_err(|error| format!("SyntaxError: {error}"))?;
    let source_before = immutable_identity(&source, &manifest)?;
    let data = TempDir::new(&format!("artifact-mcp-candidate-{name}-"))?;
    let container = format!("artifact-mcp-fixture-{}-{name}", std::process::id());
    let mut server = match &invocation.target {
        Target::Image(image) => Server::Container {
            image,
            name: container,
            user: runtime_user.expect("Docker mode resolves the runtime user"),
        },
        Target::Binary(binary) => Server::Process {
            binary,
            log: data.path.with_extension("log"),
            name: container,
            child: None,
        },
    };
    let result = exercise(
        &mut server,
        name,
        &source,
        &manifest,
        &data.path,
        source_before,
    );
    server.stop();
    drop(data);
    result
}

fn exercise(
    server: &mut Server<'_>,
    name: &str,
    source: &Path,
    manifest: &Js,
    data: &Path,
    source_before: Js,
) -> std::result::Result<Js, String> {
    copy_tree(source, data)?;
    run_checked(
        "chmod",
        &[
            "-R".to_owned(),
            "a+rwX".to_owned(),
            data.display().to_string(),
        ],
    )?;
    let Some(token) = string(get(get(Some(manifest), "authentication"), "token")) else {
        return Err(format!("{name}: fixture.json has no authentication.token"));
    };
    let origin = server.start(name, data, token)?;
    wait_for_health(origin, server)?;

    let fixture_id = match name.strip_prefix("boundary-v") {
        Some(_) => format!("singleb{}", &name[name.len() - 2..]),
        None => "single16".to_owned(),
    };
    let read = rpc(
        origin,
        token,
        "read_artifact",
        Js::object([("id", Js::str(&fixture_id))]),
        1,
    )?;
    let authenticated_read = check_authenticated_read(name, &fixture_id, read.as_ref())?;
    let mut evidence = Js::object([("name", Js::str(name))]);
    if let Some(schema) = get(get(Some(manifest), "origin"), "schemaVersion") {
        evidence.set("originSchema", schema.clone());
    }
    evidence.set("source", source_before.clone());
    evidence.set("authenticatedRead", authenticated_read);

    if name == "release-v23-recovery" {
        let published = rpc(
            origin,
            token,
            "publish_artifact",
            Js::object([
                ("html", Js::str("<main>candidate write</main>")),
                ("title", Js::str("Candidate fixture write")),
            ]),
            2,
        )?;
        let Some(id) = string(get(published.as_ref(), "id")).map(str::to_owned) else {
            return Err("candidate publish did not return artifact id".to_owned());
        };
        rpc(
            origin,
            token,
            "update_artifact",
            Js::object([
                ("id", Js::str(&id)),
                ("html", Js::str("<main>candidate update</main>")),
            ]),
            3,
        )?;
        rpc(
            origin,
            token,
            "restore_artifact",
            Js::object([("id", Js::str(&id)), ("revision", Js::Number(1.0))]),
            4,
        )?;
        evidence.set(
            "recoveryMutation",
            Js::object([
                ("publishedId", Js::String(id)),
                ("update", Js::str("passed")),
                ("restoreRevision", Js::Number(1.0)),
            ]),
        );
    }

    if name == "release-v24-durability-recovery" {
        let mut recovered = Vec::new();
        for artifact_id in ["intentup24", "prepared24", "intentpub24", "intentdel24"] {
            let value = rpc(
                origin,
                token,
                "read_artifact",
                Js::object([("id", Js::str(artifact_id))]),
                10 + recovered.len(),
            )?;
            if string(get(value.as_ref(), "id")) != Some(artifact_id) {
                return Err(format!(
                    "{name}: {artifact_id} was not recovered and readable"
                ));
            }
            recovered.push(Js::str(artifact_id));
        }
        for artifact_id in ["ambigup24", "norow24", "gone24"] {
            expect_concealed_read(origin, token, artifact_id, 20 + recovered.len())?;
        }
        let prepared_history = rpc(
            origin,
            token,
            "read_artifact",
            Js::object([("id", Js::str("prepared24")), ("revision", Js::Number(1.0))]),
            30,
        )?;
        if string(get(prepared_history.as_ref(), "id")) != Some("prepared24") {
            return Err(format!("{name}: prepared24 history was not reconstructed"));
        }
        evidence.set(
            "durabilityRecovery",
            Js::object([
                ("recovered", Js::Array(recovered)),
                (
                    "preparedMetadataOnly",
                    Js::object([
                        ("revision", Js::Number(2.0)),
                        ("historyRevision", Js::Number(1.0)),
                    ]),
                ),
                ("ambiguousStaging", Js::str("concealed")),
                ("uncommittedPublish", Js::str("not_found")),
                ("completedDelete", Js::str("not_found")),
            ]),
        );
    }

    if js::stringify(&immutable_identity(source, manifest)?) != js::stringify(&source_before) {
        return Err(format!(
            "{name}: candidate preflight mutated the frozen source fixture"
        ));
    }
    evidence.set("status", Js::str("passed"));
    Ok(evidence)
}

/// The authenticated read must return the fixture's artifact with a valid `bytes_total`.
fn check_authenticated_read(
    name: &str,
    fixture_id: &str,
    read: Option<&Js>,
) -> std::result::Result<Js, String> {
    let returned = get(read, "id");
    if string(returned) != Some(fixture_id) {
        return Err(format!(
            "{name}: authenticated read returned {} instead of {fixture_id}",
            js::to_js_string(returned)
        ));
    }
    let Some(bytes_total) =
        js::safe_integer(get(read, "bytes_total")).filter(|bytes| *bytes >= 0.0)
    else {
        return Err(format!(
            "{name}: authenticated read has invalid bytes_total"
        ));
    };
    Ok(Js::object([
        ("artifactId", Js::str(fixture_id)),
        ("returnedId", Js::str(fixture_id)),
        ("bytesTotal", Js::Number(bytes_total)),
    ]))
}

fn wait_for_health(origin: Origin, server: &mut Server<'_>) -> std::result::Result<(), String> {
    let mut last = "not ready".to_owned();
    for _ in 0..HEALTH_ATTEMPTS {
        match http::get(origin, "/health") {
            Ok(response) if response.ok() => return Ok(()),
            Ok(response) => last = format!("{} {}", response.status, response.text()),
            Err(error) => last = error,
        }
        if let Some(status) = server.exited() {
            last = format!("{last}; the process exited with {status}");
            break;
        }
        std::thread::sleep(HEALTH_INTERVAL);
    }
    let name = server.name().to_owned();
    Err(format!(
        "{name}: health never became ready ({last})\n{}",
        server.diagnostics()
    ))
}

/// POST one `tools/call` and parse the JSON response.
fn call(
    origin: Origin,
    token: &str,
    name: &str,
    arguments: Js,
    id: usize,
) -> std::result::Result<(http::Response, Js), String> {
    let body = Js::object([
        ("jsonrpc", Js::str("2.0")),
        ("id", Js::Number(id as f64)),
        ("method", Js::str("tools/call")),
        (
            "params",
            Js::object([("name", Js::str(name)), ("arguments", arguments)]),
        ),
    ]);
    let authorization = format!("Bearer {token}");
    let response = http::post_json(
        origin,
        "/mcp",
        &[
            ("authorization", &authorization),
            ("content-type", "application/json"),
        ],
        &js::stringify(&body),
    )?;
    let parsed = js::parse(&response.text()).map_err(|error| format!("SyntaxError: {error}"))?;
    if parsed == Js::Null {
        return Err("TypeError: Cannot read properties of null (reading 'error')".to_owned());
    }
    Ok((response, parsed))
}

/// `rpc()`: a successful tool result's `structuredContent` (`None` is undefined).
fn rpc(
    origin: Origin,
    token: &str,
    name: &str,
    arguments: Js,
    id: usize,
) -> std::result::Result<Option<Js>, String> {
    let (response, parsed) = call(origin, token, name, arguments, id)?;
    let body = Some(&parsed);
    if !response.ok()
        || js::truthy(get(body, "error"))
        || js::truthy(get(get(body, "result"), "isError"))
    {
        return Err(format!(
            "{name}: {} {}",
            response.status,
            js::stringify(&parsed)
        ));
    }
    match get(body, "result") {
        None | Some(Js::Null) => Err(
            "TypeError: Cannot read properties of undefined (reading 'structuredContent')"
                .to_owned(),
        ),
        result => Ok(get(result, "structuredContent").cloned()),
    }
}

/// A missing or uncommitted artifact must answer exactly like one that never existed.
fn expect_concealed_read(
    origin: Origin,
    token: &str,
    artifact_id: &str,
    id: usize,
) -> std::result::Result<(), String> {
    let (response, body) = call(
        origin,
        token,
        "read_artifact",
        Js::object([("id", Js::str(artifact_id))]),
        id,
    )?;
    let result = get(Some(&body), "result");
    let text = get(result, "content")
        .and_then(Js::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| string(item.get("type")) == Some("text"))
        })
        .and_then(|item| string(item.get("text")));
    if response.ok()
        && !js::truthy(get(Some(&body), "error"))
        && get(result, "isError") == Some(&Js::Bool(true))
        && text == Some(format!("Unknown artifact: {artifact_id}").as_str())
    {
        return Ok(());
    }
    Err(format!(
        "{artifact_id}: expected concealed tool error, got {} {}",
        response.status,
        js::stringify(&body)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidate_uses_api_key_auth_without_trusted_identity_headers() {
        let args = docker_run_args(
            "artifact-mcp-fixture-1-boundary-v00",
            "1000:1000",
            Path::new("/tmp/data"),
            "fixture-public-token-v1",
            "ghcr.io/o/r@sha256:abc",
        );
        assert!(
            args.iter()
                .any(|arg| arg == "ARTIFACT_API_KEYS=fixture-key:fixture:fixture-public-token-v1")
        );
        assert!(args.iter().all(|arg| !arg.contains("TRUST_ACCESS_HEADERS")));
        assert!(args.windows(2).any(|pair| pair == ["--user", "1000:1000"]));
        assert!(
            args.windows(2)
                .any(|pair| pair[0] == "--env" && pair[1] == AUDIT_KEY_ENV)
        );
        assert_eq!(
            AUDIT_KEY_ENV,
            "AUDIT_LEDGER_HMAC_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--publish", "127.0.0.1::3480"])
        );
        assert!(
            args.iter().all(|arg| arg != "--rm"),
            "the container must outlive a crash for diagnostics"
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some("ghcr.io/o/r@sha256:abc")
        );
        assert_eq!(
            docker_remove_args("artifact-mcp-fixture-1-boundary-v00"),
            ["rm", "--force", "artifact-mcp-fixture-1-boundary-v00"]
        );
        let env = process_env(4000, Path::new("/tmp/data"), "fixture-public-token-v1");
        assert!(env.iter().all(|(name, _)| name != "TRUST_ACCESS_HEADERS"));
        assert!(env.contains(&("LISTEN_HOST".to_owned(), "127.0.0.1".to_owned())));
        assert!(env.contains(&(
            "ARTIFACT_API_KEYS".to_owned(),
            "fixture-key:fixture:fixture-public-token-v1".to_owned()
        )));
    }

    #[test]
    fn authenticated_read_must_return_the_fixture_artifact() {
        let read = js::parse(r#"{"id":"singleb07","bytes_total":30}"#).unwrap();
        assert_eq!(
            js::stringify(
                &check_authenticated_read("boundary-v07", "singleb07", Some(&read)).unwrap()
            ),
            r#"{"artifactId":"singleb07","returnedId":"singleb07","bytesTotal":30}"#
        );
        let wrong = js::parse(r#"{"id":"other","bytes_total":30}"#).unwrap();
        assert_eq!(
            check_authenticated_read("boundary-v07", "singleb07", Some(&wrong)).unwrap_err(),
            "boundary-v07: authenticated read returned other instead of singleb07"
        );
        assert_eq!(
            check_authenticated_read("boundary-v07", "singleb07", None).unwrap_err(),
            "boundary-v07: authenticated read returned undefined instead of singleb07"
        );
        for bytes in ["-1", "1.5", "\"30\"", "9007199254740992", "null"] {
            let read =
                js::parse(&format!(r#"{{"id":"singleb07","bytes_total":{bytes}}}"#)).unwrap();
            assert_eq!(
                check_authenticated_read("boundary-v07", "singleb07", Some(&read)).unwrap_err(),
                "boundary-v07: authenticated read has invalid bytes_total",
                "{bytes}"
            );
        }
    }

    #[test]
    fn docker_port_output_is_parsed_like_the_node_expression() {
        assert_eq!(published_port("127.0.0.1:49153\n"), Some(49153));
        assert_eq!(published_port("0.0.0.0:1\n127.0.0.1:2"), Some(2));
        assert_eq!(published_port("127.0.0.1:x 127.0.0.1:7"), Some(7));
        assert_eq!(published_port("[::1]:5"), None);
    }

    #[test]
    fn errors_are_recorded_as_javascript_error_strings() {
        assert_eq!(error_string("x: failed"), "Error: x: failed");
        assert_eq!(
            error_string("TypeError: fetch failed"),
            "TypeError: fetch failed"
        );
    }

    #[test]
    fn fixture_listing_requires_contiguous_boundaries_and_five_releases() {
        let root = TempDir::new("artifact-mcp-xtask-fixture-names-").unwrap();
        let make = |name: &str| fs::create_dir(root.path.join(name)).unwrap();
        for name in [
            "boundary-v00",
            "boundary-v01",
            "release-v1",
            "release-v2",
            "release-v3",
        ] {
            make(name);
        }
        fs::write(root.path.join("README"), "not a fixture").unwrap();
        assert_eq!(
            fixture_names(&root.path).unwrap_err(),
            "expected every boundary plus five released rich fixtures, found 5"
        );
        make("release-v4");
        make("release-v5");
        assert_eq!(fixture_names(&root.path).unwrap().len(), 7);
        make("boundary-v03");
        assert_eq!(
            fixture_names(&root.path).unwrap_err(),
            "historical fixture boundaries are not contiguous at boundary-v03"
        );
        fs::remove_dir(root.path.join("boundary-v03")).unwrap();
        make("other");
        assert!(fixture_names(&root.path).is_err());
    }
}
