//! `cargo xtask benchmark-durability IMAGE_OR_LOCAL_TAG OUT.json`: bench the production Rust OCI
//! image over its real MCP HTTP transport, and `--binary PATH OUT.json` to bench a local
//! `artifact-mcp` binary the same way without Docker.
//!
//! Port of the former `scripts/benchmark-durability-oci.mjs`. `BENCH_DATA_DIR` must be an empty,
//! dedicated directory on the intended persistent volume; set `BENCH_REQUIRE_PERSISTENT=1` to
//! fail unless that directory has a discoverable mount. Use an OCI digest for release evidence;
//! a local tag supports before/after comparison. `BENCH_WARMUPS` (default 20) and
//! `BENCH_MEASURED` (default 200) set the iteration counts. The machine-readable report never
//! contains the ephemeral benchmark credential.
//!
//! Unlike the script, the server also receives an ephemeral `AUDIT_LEDGER_HMAC_KEY` and raised
//! ingress budgets (`serverOverrides` in the report): a current server refuses to start without
//! the key and rate-limits a benchmark-sized run with its default budgets.

use std::fs;
use std::io::Read as _;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::env_var;
use super::http::Http;
use super::js::{self, Js, Object, object};

const USAGE: &str = "usage: cargo xtask benchmark-durability IMAGE_OR_LOCAL_TAG OUT.json
       cargo xtask benchmark-durability --binary PATH OUT.json";
const TOKEN: &str = "artifact-mcp-benchmark-token";
const WEBHOOK_ENC_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
/// An ephemeral audit-ledger key. The server refuses to start without one; the former script
/// predates that rule and never passed it, so it could no longer start a current image.
const AUDIT_LEDGER_HMAC_KEY: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
/// Ingress budgets raised for the ephemeral benchmark server. The production defaults admit
/// about ten uploads a minute per publisher, so a run of hundreds of publishes would end in
/// HTTP 429; the benchmark measures the durability path, not the rate limiter. The report
/// records these values under `serverOverrides`.
const SERVER_OVERRIDES: [(&str, &str); 3] = [
    ("INGRESS_MCP_PER_WINDOW", "1000000"),
    ("INGRESS_MUTATIONS_PER_WINDOW", "1000000"),
    ("INGRESS_UPLOADS_PER_WINDOW", "1000000"),
];

/// Every variable the benchmark passes to the server: `(name, value, is_credential)`.
fn server_environment() -> Vec<(&'static str, String, bool)> {
    let mut environment = vec![
        ("ARTIFACT_API_KEYS", format!("bench:default:{TOKEN}"), true),
        ("WEBHOOK_ENC_KEY", WEBHOOK_ENC_KEY.to_owned(), true),
        (
            "AUDIT_LEDGER_HMAC_KEY",
            AUDIT_LEDGER_HMAC_KEY.to_owned(),
            true,
        ),
    ];
    environment.extend(
        SERVER_OVERRIDES
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned(), false)),
    );
    environment
}
const PAYLOAD_BYTES: usize = 64 * 1024;
const HEALTH_ATTEMPTS: u32 = 60;
const HEALTH_INTERVAL: Duration = Duration::from_millis(500);
/// The environment the production image sets for the server (Dockerfile `ENV`).
const IMAGE_RUST_LOG: &str = "artifact_mcp=info,tower_http=info";

enum Target {
    Image(String),
    Binary(PathBuf),
}

/// Entry point for `cargo xtask benchmark-durability`.
pub fn main(args: &[String]) -> bool {
    match run(args) {
        Ok(passed) => passed,
        Err(message) => {
            eprintln!("Error: {message}");
            false
        }
    }
}

fn parse_target(args: &[String]) -> Option<(Target, PathBuf)> {
    let first = args.first()?;
    if first == "--binary" {
        let binary = args.get(1).filter(|path| !path.is_empty())?;
        let out = args.get(2).filter(|path| !path.is_empty())?;
        return Some((Target::Binary(PathBuf::from(binary)), PathBuf::from(out)));
    }
    if let Some(binary) = first.strip_prefix("--binary=") {
        let out = args.get(1).filter(|path| !path.is_empty())?;
        return (!binary.is_empty())
            .then(|| (Target::Binary(PathBuf::from(binary)), PathBuf::from(out)));
    }
    let out = args.get(1).filter(|path| !path.is_empty())?;
    (!first.is_empty()).then(|| (Target::Image(first.clone()), PathBuf::from(out)))
}

/// `Number(process.env[name] ?? fallback)`, required to be a non-negative integer.
fn integer(name: &str, fallback: u64) -> Result<u64, String> {
    #[allow(clippy::cast_precision_loss)]
    let value = env_var(name).map_or(fallback as f64, |raw| js::string_to_number(&raw));
    if !value.is_finite() || value.fract() != 0.0 || value < 0.0 {
        return Err(format!("{name} must be a non-negative integer"));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(value as u64)
}

/// Removes a directory this command created, on every exit path.
struct TempDir(Option<PathBuf>);

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn make_temp_dir() -> Result<PathBuf, String> {
    use std::hash::{BuildHasher, RandomState};
    let base = std::env::temp_dir();
    for attempt in 0..16_u32 {
        let seed = RandomState::new().hash_one((std::process::id(), attempt, Instant::now()));
        let path = base.join(format!("artifact-mcp-oci-bench-{:06x}", seed & 0x00ff_ffff));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("mkdtemp {}: {error}", path.display())),
        }
    }
    Err("mkdtemp: could not create a unique benchmark directory".to_owned())
}

/// `execFileSync(program, args, { encoding: "utf8" }).trim()`, or `None` when it fails.
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| js::js_trim(&String::from_utf8_lossy(&output.stdout)).to_owned())
}

/// `statfs(2)` type and optimal transfer block size, from coreutils `stat --file-system`.
fn statfs(dir: &Path) -> Result<Js, String> {
    let output = Command::new("stat")
        .args(["--file-system", "--format=%t %s", "--"])
        .arg(dir)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("statfs {}: {error}", dir.display()))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split_whitespace();
    let kind = fields
        .next()
        .and_then(|hex| u64::from_str_radix(hex, 16).ok());
    let block_size = fields.next().and_then(|size| size.parse::<u64>().ok());
    match (output.status.success(), kind, block_size) {
        (true, Some(kind), Some(block_size)) => Ok(object([
            ("type", Js::from(kind)),
            ("blockSize", Js::from(block_size)),
        ])),
        _ => Err(format!("statfs {} failed", dir.display())),
    }
}

/// `os.cpus()` model of the first CPU and the CPU count, as libuv reads them on Linux.
fn cpus() -> (String, u64) {
    let model = fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, model)| model.trim().to_owned())
        })
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());
    let count = fs::read_to_string("/proc/stat")
        .map(|stat| {
            stat.lines()
                .filter(|line| {
                    line.strip_prefix("cpu")
                        .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
                })
                .count()
        })
        .unwrap_or(0);
    (model, count as u64)
}

fn environment(data_dir: &Path, persistent: bool, mount: Option<&str>) -> Result<Js, String> {
    let statfs = statfs(data_dir)?;
    let (cpu, cpu_count) = cpus();
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.trim().to_owned())
        .unwrap_or_default();
    let optional = |value: Option<String>| value.map_or(Js::Null, Js::String);
    Ok(object([
        ("host", optional(run_capture("hostname", &[]))),
        ("kernel", Js::from(kernel)),
        ("cpu", Js::from(cpu)),
        ("cpus", Js::from(cpu_count)),
        ("dataDir", Js::from(data_dir.display().to_string())),
        ("persistentEvidence", Js::Bool(persistent)),
        ("mount", optional(mount.map(str::to_owned))),
        ("statfs", statfs),
    ]))
}

/// `new Date().toISOString()`.
fn iso_now() -> String {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let millis = i64::try_from(since.as_millis()).unwrap_or(i64::MAX);
    iso_from_millis(millis)
}

fn iso_from_millis(millis: i64) -> String {
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        of_day / 3600,
        of_day % 3600 / 60,
        of_day % 60,
        millis.rem_euclid(1000)
    )
}

/// `values[Math.min(values.length - 1, Math.ceil(values.length * fraction) - 1)]`.
fn percentile(sorted: &[f64], fraction: f64) -> Option<f64> {
    #[allow(clippy::cast_precision_loss)]
    let length = sorted.len() as f64;
    let index = ((length * fraction).ceil() - 1.0).min(length - 1.0);
    if index < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    sorted.get(index as usize).copied()
}

fn summarize(mut values: Vec<f64>) -> Js {
    values.sort_by(f64::total_cmp);
    let mut summary = Object::new();
    summary.insert("count", Js::from(values.len() as u64));
    for (key, value) in [
        ("p50Ms", percentile(&values, 0.5)),
        ("p95Ms", percentile(&values, 0.95)),
        ("maxMs", values.last().copied()),
    ] {
        if let Some(value) = value {
            summary.insert(key, Js::Number(value));
        }
    }
    Js::Object(summary)
}

fn elapsed_ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

#[derive(Clone, Copy)]
enum Scenario {
    Single,
    Bundle,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::Single => "single-64KiB",
            Scenario::Bundle => "bundle-10x64KiB",
        }
    }
}

struct Bench {
    http: Http,
    base: String,
    request_id: u64,
    warmups: u64,
    measured: u64,
    payload: String,
}

impl Bench {
    fn rpc(&mut self, name: &str, args: Js) -> Result<Option<Js>, String> {
        self.request_id += 1;
        let body = object([
            ("jsonrpc", Js::from("2.0")),
            ("id", Js::from(self.request_id)),
            ("method", Js::from("tools/call")),
            (
                "params",
                object([("name", Js::from(name)), ("arguments", args)]),
            ),
        ]);
        let authorization = format!("Bearer {TOKEN}");
        let response = self
            .http
            .send(
                "POST",
                &format!("{}/mcp", self.base),
                &[
                    ("authorization", &authorization),
                    ("content-type", "application/json"),
                ],
                Some(js::stringify(&body).into_bytes()),
            )
            .map_err(|_| "fetch failed".to_owned())?;
        let text = response.text();
        let parsed = js::parse(&text).map_err(|error| js::parse_error_message(&text, &error))?;
        let failed = !response.ok()
            || js::truthy(js::get(Some(&parsed), "error")?)
            || js::truthy(js::get_opt(js::get(Some(&parsed), "result")?, "isError"));
        if failed {
            return Err(format!(
                "{name}: {} {}",
                response.status,
                js::stringify(&parsed)
            ));
        }
        Ok(js::get(js::get(Some(&parsed), "result")?, "structuredContent")?.cloned())
    }

    fn files(&self, iteration: u64, phase: &str) -> Js {
        let mut files = Object::new();
        for index in 0..10 {
            files.insert(
                format!("{index}.html"),
                Js::from(format!(
                    "<main>{}{phase}-{iteration}-{index}</main>",
                    self.payload
                )),
            );
        }
        Js::Object(files)
    }

    fn publish(
        &mut self,
        scenario: Scenario,
        index: u64,
        phase: &str,
    ) -> Result<Option<Js>, String> {
        match scenario {
            Scenario::Single => {
                let html = format!("<main>{}{phase}-{index}</main>", self.payload);
                self.rpc("publish_artifact", object([("html", Js::from(html))]))
            }
            Scenario::Bundle => {
                let files = self.files(index, phase);
                self.rpc("publish_bundle", object([("files", files)]))
            }
        }
    }

    fn update(
        &mut self,
        scenario: Scenario,
        artifact: Option<&Js>,
        index: u64,
        phase: &str,
    ) -> Result<(), String> {
        let mut args = Object::new();
        if let Some(id) = js::get(artifact, "id")? {
            args.insert("id", id.clone());
        }
        match scenario {
            Scenario::Single => args.insert(
                "html",
                Js::from(format!(
                    "<main>{}updated-{phase}-{index}</main>",
                    self.payload
                )),
            ),
            Scenario::Bundle => {
                args.insert("files", self.files(index, &format!("updated-{phase}")))
            }
        }
        self.rpc("update_artifact", Js::Object(args)).map(drop)
    }

    fn measure(&mut self, scenario: Scenario) -> Result<Js, String> {
        for index in 0..self.warmups {
            let artifact = self.publish(scenario, index, "warmup")?;
            self.update(scenario, artifact.as_ref(), index, "warmup")?;
        }
        let mut publish_times = Vec::new();
        let mut update_times = Vec::new();
        for index in 0..self.measured {
            let start = Instant::now();
            let artifact = self.publish(scenario, index, "measured")?;
            publish_times.push(elapsed_ms(start));
            let start = Instant::now();
            self.update(scenario, artifact.as_ref(), index, "measured")?;
            update_times.push(elapsed_ms(start));
        }
        Ok(object([
            ("scenario", Js::from(scenario.name())),
            ("warmups", Js::from(self.warmups)),
            ("measured", Js::from(self.measured)),
            ("publish", summarize(publish_times)),
            ("update", summarize(update_times)),
        ]))
    }

    fn run_scenarios(&mut self, completed: &mut Vec<Js>) -> Result<(), String> {
        for scenario in [Scenario::Single, Scenario::Bundle] {
            completed.push(self.measure(scenario)?);
        }
        Ok(())
    }
}

/// Poll `GET {base}/health` every 500 ms, 60 times.
fn wait_for_health(
    http: &Http,
    base: &str,
    label: &str,
    mut child: Option<&mut Child>,
) -> Result<(), String> {
    let mut last = "not ready".to_owned();
    for _ in 0..HEALTH_ATTEMPTS {
        if let Some(child) = child.as_deref_mut()
            && let Ok(Some(status)) = child.try_wait()
        {
            return Err(format!("{label} exited before it was healthy ({status})"));
        }
        match http.send("GET", &format!("{base}/health"), &[], None) {
            Ok(response) if response.ok() => return Ok(()),
            Ok(response) => last = format!("{} {}", response.status, response.text()),
            Err(_) => last = "TypeError: fetch failed".to_owned(),
        }
        std::thread::sleep(HEALTH_INTERVAL);
    }
    Err(format!("{label} was not healthy: {last}"))
}

/// `execFileSync("docker", args)` failure text, with the credential-bearing values withheld.
fn command_failed(command: &str, stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    if stderr.is_empty() {
        format!("Command failed: {command}")
    } else {
        format!("Command failed: {command}\n{stderr}")
    }
}

fn spawn_failed(program: &str, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("spawnSync {program} ENOENT")
    } else {
        format!("spawnSync {program} {error}")
    }
}

fn image_identity(image: &str) -> Result<Js, String> {
    let output = Command::new("docker")
        .args(["image", "inspect", image])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| spawn_failed("docker", &error))?;
    if !output.status.success() {
        // execFileSync without `stdio` also echoes the child's stderr.
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
        return Err(command_failed(
            &format!("docker image inspect {image}"),
            &output.stderr,
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let parsed = js::parse(&text).map_err(|error| js::parse_error_message(&text, &error))?;
    let inspected = match &parsed {
        Js::Array(items) => items.first(),
        other => other.property("0"),
    };
    let id = js::get_opt(inspected, "Id");
    if !js::truthy(id) {
        return Err("docker image inspect returned no image identity".to_owned());
    }
    let mut labels = Object::new();
    if let Some(Js::Object(all)) = js::get_opt(js::get_opt(inspected, "Config"), "Labels") {
        for (key, value) in all.iter() {
            if key.starts_with("org.opencontainers.image.") {
                labels.insert(key, value.clone());
            }
        }
    }
    Ok(object([
        ("id", id.cloned().unwrap_or(Js::Null)),
        (
            "repoDigests",
            js::or(js::get_opt(inspected, "RepoDigests"), Js::Array(Vec::new())),
        ),
        ("labels", Js::Object(labels)),
    ]))
}

/// The `/127\.0\.0\.1:(\d+)/` capture of `docker port` output.
fn loopback_port(text: &str) -> Option<&str> {
    text.match_indices("127.0.0.1:").find_map(|(at, needle)| {
        let rest = &text[at + needle.len()..];
        let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        (digits > 0).then(|| &rest[..digits])
    })
}

fn run_image(
    image: &str,
    data_dir: &Path,
    report: &mut Object,
    bench_counts: (u64, u64),
    completed: &mut Vec<Js>,
) -> Result<(), String> {
    let container = format!("artifact-mcp-durability-bench-{}", std::process::id());
    let result = (|| {
        report.insert("image", image_identity(image)?);
        let volume = format!("{}:/data-rust", data_dir.display());
        let mut args: Vec<String> = [
            "run",
            "--detach",
            "--rm",
            "--name",
            &container,
            "--publish",
            "127.0.0.1::3480",
            "--volume",
            &volume,
        ]
        .map(str::to_owned)
        .to_vec();
        // The failure message names every variable but withholds the credential values.
        let mut shown = args.clone();
        for (name, value, secret) in server_environment() {
            args.extend(["--env".to_owned(), format!("{name}={value}")]);
            let value = if secret { "<withheld>" } else { value.as_str() };
            shown.extend(["--env".to_owned(), format!("{name}={value}")]);
        }
        args.push(image.to_owned());
        shown.push(image.to_owned());
        let output = Command::new("docker")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| spawn_failed("docker", &error))?;
        if !output.status.success() {
            return Err(command_failed(
                &format!("docker {}", shown.join(" ")),
                &output.stderr,
            ));
        }
        let port = run_capture("docker", &["port", &container, "3480/tcp"])
            .as_deref()
            .and_then(loopback_port)
            .map(str::to_owned)
            .ok_or_else(|| "unable to discover production OCI benchmark port".to_owned())?;
        let base = format!("http://127.0.0.1:{port}");
        let http = Http::new(None, true)?;
        wait_for_health(&http, &base, "production OCI benchmark container", None)?;
        Bench::new(http, base, bench_counts).run_scenarios(completed)
    })();
    let _ = Command::new("docker")
        .args(["rm", "--force", &container])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    result
}

impl Bench {
    fn new(http: Http, base: String, (warmups, measured): (u64, u64)) -> Self {
        Self {
            http,
            base,
            request_id: 0,
            warmups,
            measured,
            payload: "x".repeat(PAYLOAD_BYTES),
        }
    }
}

/// Stops the local server on every exit path.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 16];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if read == 0 {
            return Ok(hex::encode(hasher.finalize()));
        }
        hasher.update(&buffer[..read]);
    }
}

fn run_binary(
    binary: &Path,
    data_dir: &Path,
    report: &mut Object,
    bench_counts: (u64, u64),
    completed: &mut Vec<Js>,
) -> Result<(), String> {
    let binary = binary
        .canonicalize()
        .map_err(|error| format!("--binary {}: {error}", binary.display()))?;
    report.insert(
        "binary",
        object([
            ("path", Js::from(binary.display().to_string())),
            ("sha256", Js::from(file_sha256(&binary)?)),
        ]),
    );
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map_err(|error| format!("could not reserve a loopback port: {error}"))?
        .port();
    let log_path = std::env::temp_dir().join(format!(
        "artifact-mcp-bench-server-{}.log",
        std::process::id()
    ));
    let log = fs::File::create(&log_path)
        .map_err(|error| format!("create {}: {error}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .map_err(|error| format!("create {}: {error}", log_path.display()))?;
    // The image's environment plus exactly what `docker run` passes; nothing else leaks in.
    let child = Command::new(&binary)
        .env_clear()
        .env(
            "PATH",
            env_var("PATH").unwrap_or_else(|| "/usr/bin:/bin".to_owned()),
        )
        .env("PORT", port.to_string())
        .env("LISTEN_HOST", "127.0.0.1")
        .env("DATA_DIR", data_dir)
        .env("RUST_LOG", IMAGE_RUST_LOG)
        .envs(
            server_environment()
                .into_iter()
                .map(|(name, value, _)| (name, value)),
        )
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err)
        .spawn()
        .map_err(|error| spawn_failed(&binary.display().to_string(), &error))?;
    let mut server = Server(child);
    let base = format!("http://127.0.0.1:{port}");
    let result = (|| {
        let http = Http::new(None, true)?;
        wait_for_health(&http, &base, "local benchmark server", Some(&mut server.0))?;
        Bench::new(http, base.clone(), bench_counts).run_scenarios(completed)
    })();
    drop(server);
    match result {
        Ok(()) => {
            let _ = fs::remove_file(&log_path);
            Ok(())
        }
        Err(message) => Err(format!("{message} (server log: {})", log_path.display())),
    }
}

fn write_report(out: &Path, report: &Object) -> Result<(), String> {
    let text = format!("{}\n", js::stringify_pretty(&Js::Object(report.clone())));
    fs::write(out, text)
        .map_err(|error| super::node_fs_error(&error, "open", &out.display().to_string()))
}

fn run(args: &[String]) -> Result<bool, String> {
    if args
        .first()
        .is_some_and(|arg| arg == "--help" || arg == "-h")
    {
        println!("{USAGE}");
        return Ok(true);
    }
    let (target, out) = parse_target(args).ok_or_else(|| USAGE.to_owned())?;
    let warmups = integer("BENCH_WARMUPS", 20)?;
    let measured = integer("BENCH_MEASURED", 200)?;
    let requested_dir = env_var("BENCH_DATA_DIR").filter(|dir| !dir.is_empty());
    let data_dir = match &requested_dir {
        Some(dir) => PathBuf::from(dir),
        None => make_temp_dir()?,
    };
    let _cleanup = TempDir(requested_dir.is_none().then(|| data_dir.clone()));
    let display_dir = data_dir.display().to_string();
    fs::create_dir_all(&data_dir)
        .map_err(|error| super::node_fs_error(&error, "mkdir", &display_dir))?;
    let not_empty = fs::read_dir(&data_dir)
        .map_err(|error| super::node_fs_error(&error, "scandir", &display_dir))?
        .next()
        .is_some();
    if not_empty {
        return Err(format!(
            "BENCH_DATA_DIR must be an empty dedicated directory: {display_dir}"
        ));
    }
    let require_persistent = env_var("BENCH_REQUIRE_PERSISTENT").as_deref() == Some("1");
    if !require_persistent {
        eprintln!(
            "SMOKE ONLY: set BENCH_DATA_DIR and BENCH_REQUIRE_PERSISTENT=1 for persistent-volume evidence."
        );
    }
    let mount = run_capture("findmnt", &["--json", "--target", &display_dir]);
    if require_persistent && (requested_dir.is_none() || mount.as_deref().is_none_or(str::is_empty))
    {
        return Err(
            "persistent evidence requires BENCH_DATA_DIR and a discoverable findmnt target"
                .to_owned(),
        );
    }
    let environment = environment(&data_dir, require_persistent, mount.as_deref())?;

    let mut report = Object::new();
    report.insert("schemaVersion", Js::from(1_u64));
    let (kind, requested_key, requested, label) = match &target {
        Target::Image(image) => (
            "artifact-mcp-production-oci-durability-benchmark",
            "requestedImage",
            image.clone(),
            "production OCI",
        ),
        Target::Binary(binary) => (
            "artifact-mcp-local-binary-durability-benchmark",
            "requestedBinary",
            binary.display().to_string(),
            "local binary",
        ),
    };
    report.insert("kind", Js::from(kind));
    report.insert(requested_key, Js::from(requested));
    report.insert("environment", environment);
    let mut overrides = Object::new();
    for (name, value) in SERVER_OVERRIDES {
        overrides.insert(name, Js::from(value));
    }
    report.insert("serverOverrides", Js::Object(overrides));
    report.insert("scenarios", Js::Array(Vec::new()));

    let mut completed = Vec::new();
    let counts = (warmups, measured);
    let outcome = match &target {
        Target::Image(image) => run_image(image, &data_dir, &mut report, counts, &mut completed),
        Target::Binary(binary) => {
            run_binary(binary, &data_dir, &mut report, counts, &mut completed)
        }
    };
    let scenario_count = completed.len();
    report.insert("scenarios", Js::Array(completed));
    match outcome {
        Ok(()) => {
            report.insert("completedAt", Js::from(iso_now()));
            write_report(&out, &report)?;
            println!("{label} durability benchmark passed: {scenario_count} scenarios");
            Ok(true)
        }
        Err(message) => {
            report.insert("error", Js::from(format!("Error: {message}")));
            write_report(&out, &report)?;
            Err(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_follow_the_script_percentiles() {
        let summary = summarize(vec![5.0, 1.0, 4.0, 2.0, 3.0]);
        assert_eq!(
            js::stringify(&summary),
            r#"{"count":5,"p50Ms":3,"p95Ms":5,"maxMs":5}"#
        );
        assert_eq!(js::stringify(&summarize(Vec::new())), r#"{"count":0}"#);
        let twenty: Vec<f64> = (1..=20).map(f64::from).collect();
        assert_eq!(
            js::stringify(&summarize(twenty)),
            r#"{"count":20,"p50Ms":10,"p95Ms":19,"maxMs":20}"#
        );
    }

    #[test]
    fn iso_timestamps_match_to_iso_string() {
        assert_eq!(iso_from_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_from_millis(951_782_400_123), "2000-02-29T00:00:00.123Z");
        assert_eq!(
            iso_from_millis(1_792_663_199_999),
            "2026-10-22T09:59:59.999Z"
        );
    }

    #[test]
    fn docker_port_output_parses_like_the_script() {
        assert_eq!(loopback_port("127.0.0.1:49153"), Some("49153"));
        assert_eq!(
            loopback_port("0.0.0.0:1\n127.0.0.1:x 127.0.0.1:42"),
            Some("42")
        );
        assert_eq!(loopback_port("[::1]:5"), None);
    }

    #[test]
    fn targets_parse_image_and_binary_modes() {
        let args = |list: &[&str]| list.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        assert!(matches!(
            parse_target(&args(&["img:tag", "out.json", "ignored"])),
            Some((Target::Image(image), _)) if image == "img:tag"
        ));
        assert!(matches!(
            parse_target(&args(&["--binary", "/bin/x", "out.json"])),
            Some((Target::Binary(path), _)) if path == Path::new("/bin/x")
        ));
        assert!(parse_target(&args(&["img"])).is_none());
        assert!(parse_target(&args(&["--binary", "/bin/x"])).is_none());
    }
}
