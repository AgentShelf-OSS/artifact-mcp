// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//
//! Derived MCP surface gate.
//!
//! This deliberately reads the contracts already owned by the server instead of maintaining a
//! second registry. It answers one release question: does every advertised MCP name still have a
//! validation path, an output schema, dispatch, OAuth scope coverage, documentation, and live
//! contract coverage?
//!
//! ```sh
//! cargo test --test mcp_surface
//! MCP_SURFACE_BASE=origin/master cargo test --test mcp_surface -- --nocapture  # also report changed surface paths
//! ```
//!
//! The source files are read at test time, so a declaration, documentation or golden change is
//! checked without rebuilding the server.

// Shared with the conformance oracle: `JSON.stringify` byte compatibility for the golden check.
#[allow(dead_code)]
#[path = "conformance/js.rs"]
mod js;

use std::path::{Path, PathBuf};
use std::process::Command;

use js::{Js, Obj};

/// Errors collected while checking; every mismatch is reported, not only the first.
#[derive(Default)]
struct Errors(Vec<String>);

impl Errors {
    fn check(&mut self, condition: bool, message: impl Into<String>) {
        if !condition {
            self.0.push(message.into());
        }
    }
}

fn unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

fn sorted_unique(items: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut out = unique(items);
    out.sort();
    out
}

fn duplicates(items: &[String]) -> Vec<String> {
    sorted_unique(
        items
            .iter()
            .enumerate()
            .filter(|(index, item)| items.iter().position(|first| first == *item) != Some(*index))
            .map(|(_, item)| item.clone()),
    )
}

/// JavaScript's `\s`.
fn is_space(character: char) -> bool {
    matches!(
        character,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn skip_space(text: &str, from: usize) -> usize {
    text[from..]
        .char_indices()
        .find(|(_, character)| !is_space(*character))
        .map_or(text.len(), |(offset, _)| from + offset)
}

fn is_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Every non-overlapping match of a multiline (`/gm`) pattern anchored with `^`. `matcher`
/// returns the captured text and the match end for a match starting at a line start.
fn line_matches(
    text: &str,
    mut matcher: impl FnMut(&str, usize) -> Option<(String, usize)>,
) -> Vec<String> {
    let mut found = Vec::new();
    let mut position = 0;
    while position <= text.len() {
        let line_start = position == 0
            || text[..position]
                .chars()
                .next_back()
                .is_some_and(is_line_terminator);
        if line_start && let Some((captured, end)) = matcher(text, position) {
            found.push(captured);
            position = end.max(position + 1);
            continue;
        }
        position += text[position..].chars().next().map_or(1, char::len_utf8);
    }
    found
}

/// Length of the run of `[a-z_]` (or `[a-z/]` when `slash`) at `from`.
fn word_length(text: &str, from: usize, slash: bool) -> usize {
    text[from..]
        .bytes()
        .take_while(|byte| byte.is_ascii_lowercase() || *byte == if slash { b'/' } else { b'_' })
        .count()
}

/// Match `"([a-z_]+)"` at `from`; return the name and the index after the closing quote.
fn quoted_word(text: &str, from: usize) -> Option<(String, usize)> {
    if text.as_bytes().get(from) != Some(&b'"') {
        return None;
    }
    let length = word_length(text, from + 1, false);
    let close = from + 1 + length;
    (length > 0 && text.as_bytes().get(close) == Some(&b'"'))
        .then(|| (text[from + 1..close].to_owned(), close + 1))
}

/// The body of `CONSTANT: &str = r#"..."#;`.
fn rust_raw_string<'a>(source: &'a str, constant: &str) -> Result<&'a str, String> {
    let marker = format!("{constant}: &str = r#\"");
    let start = source
        .find(&marker)
        .ok_or_else(|| format!("could not find {constant}"))?
        + marker.len();
    let end = source[start..]
        .find("\"#;")
        .ok_or_else(|| format!("could not find closing delimiter for {constant}"))?;
    Ok(&source[start..start + end])
}

/// Tool names matched by `/^\s*"([a-z_]+)"\s*(?:if [^{]+)?=>/gm` inside the tools/call match.
fn tool_names_from_dispatch(source: &str, errors: &mut Errors) -> Vec<String> {
    let call_start = source.find("let result = match name {");
    let call_end =
        call_start.and_then(|start| source[start..].find("    }?;").map(|end| start + end));
    let (Some(start), Some(end)) = (call_start, call_end) else {
        errors.check(false, "could not locate tools/call dispatch match");
        return Vec::new();
    };
    let arms = line_matches(&source[start..end], |text, at| {
        let (name, after) = quoted_word(text, skip_space(text, at))?;
        let cursor = skip_space(text, after);
        if text[cursor..].starts_with("if ") {
            // `[^{]+` is greedy: take the last `=>` before the next `{`.
            let guard = cursor + 3;
            let limit = text[guard..]
                .find('{')
                .map_or(text.len(), |brace| guard + brace);
            if limit > guard + 1
                && let Some(arrow) = text[guard + 1..limit].rfind("=>")
            {
                return Some((name, guard + 1 + arrow + 2));
            }
        }
        text[cursor..]
            .starts_with("=>")
            .then_some((name, cursor + 2))
    });
    sorted_unique(arms)
}

fn balanced_block(source: &str, opening_brace: usize) -> Result<&str, String> {
    let mut depth = 0_i64;
    for (index, byte) in source.bytes().enumerate().skip(opening_brace) {
        match byte {
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
        if depth == 0 {
            return Ok(&source[opening_brace..=index]);
        }
    }
    Err("could not find closing Rust block".to_owned())
}

/// Names matched by `/^\s*name:\s*"([a-z_]+)"\.to_owned\(\),$/gm`, duplicates kept.
fn named_tool_definition_names(source: &str) -> Vec<String> {
    line_matches(source, |text, at| {
        let cursor = skip_space(text, at);
        if !text[cursor..].starts_with("name:") {
            return None;
        }
        let (name, after) = quoted_word(text, skip_space(text, cursor + "name:".len()))?;
        let tail = ".to_owned(),";
        let end = after + tail.len();
        let at_line_end = text[end..].chars().next().is_none_or(is_line_terminator);
        (text[after..].starts_with(tail) && at_line_end).then_some((name, end))
    })
}

fn app_only_tool_definitions(source: &str, errors: &mut Errors) -> Result<Vec<String>, String> {
    let Some(start) = source.find("if supports_apps {\n        definitions.push") else {
        errors.check(
            false,
            "could not locate MCP App-only tool declaration block",
        );
        return Ok(Vec::new());
    };
    let opening = start + source[start..].find('{').expect("marker contains a brace");
    Ok(sorted_unique(named_tool_definition_names(balanced_block(
        source, opening,
    )?)))
}

/// Methods matched by `/"(NAMESPACE\/[a-z/]+)"/g` inside the function's `match method { ... }`.
fn methods_from_dispatch(
    source: &str,
    function_marker: &str,
    namespace: &str,
    errors: &mut Errors,
) -> Result<Vec<String>, String> {
    let function_start = source.find(function_marker);
    let match_start = source[function_start.unwrap_or(0)..]
        .find("match method {")
        .map(|offset| function_start.unwrap_or(0) + offset);
    let (Some(_), Some(match_start)) = (function_start, match_start) else {
        errors.check(
            false,
            format!("could not locate {namespace} dispatch match"),
        );
        return Ok(Vec::new());
    };
    let opening = match_start + "match method ".len();
    let block = balanced_block(source, opening)?;
    let prefix = format!("\"{namespace}/");
    let mut methods = Vec::new();
    let mut search = 0;
    while let Some(offset) = block[search..].find(&prefix) {
        let start = search + offset;
        let rest = start + prefix.len();
        let length = word_length(block, rest, true);
        if length > 0 && block.as_bytes().get(rest + length) == Some(&b'"') {
            methods.push(block[start + 1..rest + length].to_owned());
            search = rest + length + 1;
        } else {
            search = start + 1;
        }
    }
    Ok(sorted_unique(methods))
}

/// Versions matched by `/^pub const (?:MODERN_)?PROTOCOL_VERSION: &str = "([^"]+)";$/gm`.
fn protocol_versions_from_dispatch(source: &str) -> Vec<String> {
    sorted_unique(line_matches(source, |text, at| {
        let value = [
            "pub const PROTOCOL_VERSION: &str = \"",
            "pub const MODERN_PROTOCOL_VERSION: &str = \"",
        ]
        .iter()
        .find(|prefix| text[at..].starts_with(*prefix))
        .map(|prefix| at + prefix.len())?;
        let close = value + text[value..].find('"')?;
        let end = close + "\";".len();
        let at_line_end = text[end..].chars().next().is_none_or(is_line_terminator);
        (close > value && text[close..].starts_with("\";") && at_line_end)
            .then(|| (text[value..close].to_owned(), end))
    }))
}

/// The first match of `/^version\s*=\s*"([^"]+)"/m`.
fn cargo_package_version(cargo: &str) -> Option<String> {
    line_matches(cargo, |text, at| {
        if !text[at..].starts_with("version") {
            return None;
        }
        let equals = skip_space(text, at + "version".len());
        if !text[equals..].starts_with('=') {
            return None;
        }
        let open = skip_space(text, equals + 1);
        if !text[open..].starts_with('"') {
            return None;
        }
        let close = open + 1 + text[open + 1..].find('"')?;
        (close > open + 1).then(|| (text[open + 1..close].to_owned(), close + 1))
    })
    .into_iter()
    .next()
}

/// `/"KEY"\s*:\s*"VALUE"/.test(text)`.
fn has_json_member(text: &str, key: &str, value: &str) -> bool {
    let needle = format!("\"{key}\"");
    text.match_indices(&needle).any(|(start, _)| {
        let colon = skip_space(text, start + needle.len());
        text[colon..].starts_with(':')
            && text[skip_space(text, colon + 1)..].starts_with(&format!("\"{value}\""))
    })
}

const REQUIRED_CONTRACT_PATHS: [&str; 5] = [
    "conformance/cases/mcp.tools-list.json",
    "conformance/cases/mcp.initialize.json",
    "conformance/cases/mcp.read-artifact.json",
    "tests/native/u25_api_key_capabilities.rs",
    "tests/native/u26_mcp_2026.rs",
];

/// Paths whose change makes an MCP compatibility review necessary. The checker and the
/// conformance runner moved from `scripts/` and `conformance/` into `tests/`, so both locations
/// are listed.
const SURFACE_PATH_PREFIXES: &[&str] = &[
    "src/mcp/",
    "src/security/oauth.rs",
    "src/http/routes/mcp.rs",
    "conformance/",
    "tests/conformance/",
    "scripts/check-mcp-surface",
    "tests/mcp_surface.rs",
    "README.md",
    "Cargo.toml",
    "tests/native/u25_",
    "tests/native/u26_",
];

fn strings(values: &[String]) -> Js {
    Js::Arr(values.iter().cloned().map(Js::Str).collect())
}

/// Paths changed between `base` and HEAD that touch the MCP surface.
fn changed_surface_paths(root: &Path, base: &str) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["diff", "--name-only", &format!("{base}...HEAD")])
        .current_dir(root)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .ok_or_else(|| {
            format!(
                "could not derive changed paths from {base}; fetch or pass a reachable base ref"
            )
        })?;
    let mut paths: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|path| !path.is_empty())
        .filter(|path| {
            SURFACE_PATH_PREFIXES
                .iter()
                .any(|prefix| path.starts_with(prefix))
        })
        .map(str::to_owned)
        .collect();
    paths.sort();
    Ok(paths)
}

/// Check the surface rooted at `root`. Returns the report, or every error found.
fn check_surface(root: &Path, base: Option<&str>) -> Result<Js, Vec<String>> {
    let mut errors = Errors::default();
    match check_surface_into(root, base, &mut errors) {
        Ok(report) if errors.0.is_empty() => Ok(report),
        Ok(_) => Err(errors.0),
        Err(fatal) => {
            errors.0.push(fatal);
            Err(errors.0)
        }
    }
}

fn check_surface_into(root: &Path, base: Option<&str>, errors: &mut Errors) -> Result<Js, String> {
    let read = |path: &str| {
        std::fs::read_to_string(root.join(path))
            .map_err(|error| format!("could not read {path}: {error}"))
    };
    let parse = |path: &str, text: &str| {
        js::parse(text).map_err(|error| format!("{path} is not valid JSON: {error}"))
    };
    let tool_definitions = read("src/mcp/tool_defs.rs")?;
    let dispatch = read("src/mcp/dispatch.rs")?;
    let resources = read("src/mcp/resources.rs")?;
    let tasks = read("src/mcp/tasks.rs")?;
    let oauth = read("src/security/oauth.rs")?;
    let mcp_docs = read("docs/mcp-api.md")?;
    let cargo = read("Cargo.toml")?;
    let native_tests = read("tests/native/main.rs")?;
    let frozen_tools = parse(
        "FROZEN_TOOL_DEFINITIONS_JSON",
        rust_raw_string(&tool_definitions, "FROZEN_TOOL_DEFINITIONS_JSON")?,
    )?;
    let schemas_path = "conformance/mcp.tool-output-schemas.json";
    let output_schemas = parse(schemas_path, &read(schemas_path)?)?;
    let golden_path = "conformance/goldens/mcp.tools-list.json";
    let golden = parse(golden_path, &read(golden_path)?)?;
    let golden_tools = ["steps", "0", "body", "json", "result", "tools"]
        .iter()
        .try_fold(golden, |node, key| {
            js::property(&node, key).filter(|value| !matches!(value, Js::Null))
        });

    let frozen = frozen_tools
        .as_array()
        .ok_or("FROZEN_TOOL_DEFINITIONS_JSON is not a JSON array")?;
    let legacy_tools: Vec<String> = frozen
        .iter()
        .map(|tool| {
            tool.get("name")
                .map_or_else(|| "undefined".to_owned(), js::to_js_string)
        })
        .collect();
    let modern_tool_declarations = named_tool_definition_names(&tool_definitions);
    let declared_modern_tools = sorted_unique(modern_tool_declarations.clone());
    let app_only_tools = app_only_tool_definitions(&tool_definitions, errors)?;
    let modern_base_tools: Vec<String> = declared_modern_tools
        .iter()
        .filter(|name| !app_only_tools.contains(name))
        .cloned()
        .collect();
    let modern_tools: Vec<String> = legacy_tools
        .iter()
        .chain(&declared_modern_tools)
        .cloned()
        .collect();
    let dispatch_tools = tool_names_from_dispatch(&dispatch, errors);
    let resource_methods =
        methods_from_dispatch(&resources, "pub async fn dispatch(", "resources", errors)?;
    let task_methods =
        methods_from_dispatch(&tasks, "pub fn dispatch_task_method(", "tasks", errors)?;
    let protocol_versions = protocol_versions_from_dispatch(&dispatch);
    let rejected_duplicates = [
        ("legacyTools", duplicates(&legacy_tools)),
        ("modernDeclarations", duplicates(&modern_tool_declarations)),
    ];

    errors.check(
        legacy_tools.len() == 34,
        format!(
            "legacy advertised tool count is {}, expected 34",
            legacy_tools.len()
        ),
    );
    for (surface, names) in &rejected_duplicates {
        errors.check(
            names.is_empty(),
            format!("{surface} contain duplicate names: {}", names.join(", ")),
        );
    }
    errors.check(
        golden_tools
            .as_ref()
            .is_some_and(|tools| js::stringify(tools) == js::stringify(&frozen_tools)),
        "frozen tool definitions drift from conformance/goldens/mcp.tools-list.json",
    );
    for (tool, name) in frozen.iter().zip(&legacy_tools) {
        errors.check(
            tool.get("description")
                .and_then(Js::as_str)
                .is_some_and(|description| !description.is_empty()),
            format!("{name} has no description"),
        );
        errors.check(
            tool.get("inputSchema")
                .and_then(|schema| schema.get("type"))
                .and_then(Js::as_str)
                == Some("object"),
            format!("{name} input schema is not an object"),
        );
    }

    let schemas = output_schemas.as_object().cloned().unwrap_or_default();
    for name in &modern_tools {
        let schema = schemas.get(name);
        errors.check(
            schema.is_some_and(Js::truthy),
            format!("{name} has no typed output schema"),
        );
        errors.check(
            schema
                .and_then(|schema| schema.get("type"))
                .and_then(Js::as_str)
                == Some("object"),
            format!("{name} output schema is not an object"),
        );
        errors.check(
            dispatch_tools.contains(name),
            format!("{name} is advertised but missing from tools/call dispatch"),
        );
        errors.check(
            oauth.contains(&format!("\"{name}\"")),
            format!("{name} is advertised but absent from OAuth scope mapping"),
        );
        errors.check(
            mcp_docs.contains(&format!("`{name}")),
            format!("{name} is advertised but absent from MCP documentation"),
        );
    }
    for name in schemas.keys() {
        errors.check(
            modern_tools.contains(name),
            format!("output schema exists for non-advertised tool {name}"),
        );
    }

    errors.check(
        dispatch.contains("modern_tool_definitions_for_client(allow_app_tools)")
            && dispatch.contains("frozen_tool_definitions()")
            && dispatch.contains("validate_schema_input(\n        &definition.input_schema"),
        "tools/call no longer validates input against the advertised definitions",
    );
    errors.check(
        dispatch.contains("let schema = tool_output_schema(name)")
            && dispatch.contains("validate_schema_input(&schema"),
        "tools/call no longer validates structured output against the typed output schema",
    );

    for (methods, owner, owner_source) in [
        (&resource_methods, "resource", &resources),
        (&task_methods, "task", &tasks),
    ] {
        for method in methods {
            errors.check(
                dispatch.contains(&format!("\"{method}\"")),
                format!("{method} is missing from modern protocol dispatch"),
            );
            errors.check(
                owner_source.contains(&format!("\"{method}\"")),
                format!("{method} is missing from {owner} dispatch"),
            );
            errors.check(
                oauth.contains(&format!("\"{method}\"")),
                format!("{method} is missing OAuth scope coverage"),
            );
            errors.check(
                mcp_docs.contains(&format!("`{method}`")),
                format!("{method} is missing from MCP documentation"),
            );
        }
    }
    errors.check(
        dispatch.contains("\"server/discover\"") && mcp_docs.contains("`server/discover`"),
        "server/discover implementation and MCP documentation are not aligned",
    );
    errors.check(
        !declared_modern_tools.is_empty() && !app_only_tools.is_empty(),
        "modern and MCP App tool declarations are no longer explicit",
    );

    let cargo_version = cargo_package_version(&cargo);
    errors.check(
        cargo_version.is_some(),
        "could not read the Cargo.toml package version",
    );
    errors.check(
        dispatch.contains("pub const SERVER_VERSION: &str = env!(\"CARGO_PKG_VERSION\")"),
        "MCP serverInfo is not derived from Cargo package version",
    );
    for version in &protocol_versions {
        errors.check(
            dispatch.contains(&format!("\"{version}\"")),
            format!("{version} is missing from Rust protocol declarations"),
        );
        errors.check(
            mcp_docs.contains(&format!("`{version}`")),
            format!("{version} is missing from MCP protocol documentation"),
        );
    }
    errors.check(
        mcp_docs.contains(&format!(
            "The legacy catalog contains {} tools",
            legacy_tools.len()
        )) && mcp_docs.contains(&format!(
            "for {}",
            legacy_tools.len() + modern_base_tools.len()
        )),
        "MCP tool-count statement is stale or missing",
    );

    for path in REQUIRED_CONTRACT_PATHS {
        errors.check(
            root.join(path).exists(),
            format!("required MCP contract test is missing: {path}"),
        );
    }
    errors.check(
        native_tests.contains("mod u25_api_key_capabilities;"),
        "API-key capability tests are not registered",
    );
    errors.check(
        native_tests.contains("mod u26_mcp_2026;"),
        "MCP 2026 tests are not registered",
    );
    let tools_list_case = read("conformance/cases/mcp.tools-list.json")?;
    let initialize_case = read("conformance/cases/mcp.initialize.json")?;
    let read_artifact_case = read("conformance/cases/mcp.read-artifact.json")?;
    let modern_contract_tests = read("tests/native/u26_mcp_2026.rs")?;
    errors.check(
        has_json_member(&tools_list_case, "method", "tools/list"),
        "tools/list conformance case no longer exercises tools/list",
    );
    errors.check(
        has_json_member(&initialize_case, "method", "initialize"),
        "initialize conformance case no longer exercises initialize",
    );
    errors.check(
        protocol_versions
            .iter()
            .any(|version| has_json_member(&initialize_case, "protocolVersion", version)),
        "initialize conformance case no longer exercises a declared protocol version",
    );
    errors.check(
        has_json_member(&read_artifact_case, "name", "read_artifact"),
        "read-artifact conformance case no longer calls read_artifact",
    );
    for name in declared_modern_tools
        .iter()
        .chain(&resource_methods)
        .chain(&task_methods)
    {
        errors.check(
            modern_contract_tests.contains(&format!("\"{name}\"")),
            format!("{name} has no reference in the MCP 2026 contract test"),
        );
    }

    // Informational only: which changed paths touch the surface since `base`.
    let changed = match base {
        Some(base) => changed_surface_paths(root, base).unwrap_or_else(|error| {
            errors.check(false, error);
            Vec::new()
        }),
        None => Vec::new(),
    };
    let mut duplicates_report = Obj::new();
    for (surface, names) in &rejected_duplicates {
        duplicates_report.insert(*surface, strings(names));
    }
    let count = |n: usize| Js::Num(n as f64);
    Ok(Js::Obj(
        Obj::new()
            .with("invariant", Js::str("PBI-059/mcp-surface-sync"))
            .with(
                "compatibilityDecision",
                Js::str(if changed.is_empty() {
                    "not-applicable"
                } else {
                    "review-required"
                }),
            )
            .with("rejectedDuplicates", Js::Obj(duplicates_report))
            .with(
                "versions",
                Js::Obj(
                    Obj::new()
                        .with("package", cargo_version.map_or(Js::Null, Js::Str))
                        .with("protocols", strings(&protocol_versions)),
                ),
            )
            .with(
                "tools",
                Js::Obj(
                    Obj::new()
                        .with("legacyCount", count(legacy_tools.len()))
                        .with(
                            "modernBaseCount",
                            count(legacy_tools.len() + modern_base_tools.len()),
                        )
                        .with("appsNegotiatedCount", count(modern_tools.len()))
                        .with("declaredModernOnly", strings(&declared_modern_tools))
                        .with("appOnly", strings(&app_only_tools))
                        .with("names", strings(&modern_tools)),
                ),
            )
            .with(
                "methods",
                Js::Obj(
                    Obj::new()
                        .with("resources", strings(&resource_methods))
                        .with("tasks", strings(&task_methods)),
                ),
            )
            .with(
                "tests",
                strings(&REQUIRED_CONTRACT_PATHS.map(str::to_owned)),
            )
            .with("changedSurfacePaths", strings(&changed))
            .with(
                "commands",
                strings(&[
                    "cargo test --test mcp_surface".to_owned(),
                    "cargo test --test conformance".to_owned(),
                    "cargo test --all-targets --locked".to_owned(),
                ]),
            ),
    ))
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The gate. Set `MCP_SURFACE_BASE=<git ref>` to also report the surface paths changed since
/// that ref (printed with `-- --nocapture`); an unreachable ref fails the check.
#[test]
fn the_advertised_mcp_surface_is_synchronized() {
    let base = std::env::var("MCP_SURFACE_BASE")
        .ok()
        .filter(|base| !base.is_empty());
    match check_surface(&repo_root(), base.as_deref()) {
        Ok(report) => println!("{}", js::stringify_pretty(&report)),
        Err(errors) => panic!(
            "MCP surface synchronization failed:\n{}",
            errors
                .iter()
                .map(|error| format!("- {error}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

// --- Tests of the checker itself (ported from scripts/check-mcp-surface.test.mjs) ---------------

const SURFACE_FILES: [&str; 15] = [
    "src/mcp/tool_defs.rs",
    "src/mcp/dispatch.rs",
    "src/mcp/resources.rs",
    "src/mcp/tasks.rs",
    "src/security/oauth.rs",
    "docs/mcp-api.md",
    "Cargo.toml",
    "tests/native/main.rs",
    "tests/native/u25_api_key_capabilities.rs",
    "tests/native/u26_mcp_2026.rs",
    "conformance/mcp.tool-output-schemas.json",
    "conformance/goldens/mcp.tools-list.json",
    "conformance/cases/mcp.tools-list.json",
    "conformance/cases/mcp.initialize.json",
    "conformance/cases/mcp.read-artifact.json",
];

/// A temporary copy of every file the checker reads, removed on drop.
struct SurfaceCopy(PathBuf);

impl SurfaceCopy {
    fn new(label: &str) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let root = std::env::temp_dir().join(format!(
            "artifact-mcp-surface-{label}-{}-{nanos}",
            std::process::id()
        ));
        for path in SURFACE_FILES {
            let destination = root.join(path);
            std::fs::create_dir_all(destination.parent().expect("file has a parent"))
                .expect("create surface copy directory");
            std::fs::copy(repo_root().join(path), &destination).expect("copy surface file");
        }
        Self(root)
    }

    fn edit(&self, path: &str, change: impl FnOnce(String) -> String) {
        let full = self.0.join(path);
        let original = std::fs::read_to_string(&full).expect("read surface copy");
        let changed = change(original.clone());
        assert_ne!(changed, original, "the edit to {path} must change it");
        std::fs::write(full, changed).expect("write surface copy");
    }

    fn errors(&self) -> Vec<String> {
        check_surface(&self.0, None).expect_err("the edited surface must fail the gate")
    }
}

impl Drop for SurfaceCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_reported(errors: &[String], expected: &str) {
    assert!(
        errors.iter().any(|error| error == expected),
        "expected {expected:?} in {errors:#?}"
    );
}

#[test]
fn an_unmodified_copy_of_the_surface_passes() {
    let copy = SurfaceCopy::new("unmodified");
    if let Err(errors) = check_surface(&copy.0, None) {
        panic!("{errors:#?}");
    }
}

#[test]
fn an_unsynchronized_modern_declaration_fails_the_derived_surface_gate() {
    let copy = SurfaceCopy::new("rename");
    copy.edit("src/mcp/tool_defs.rs", |source| {
        source.replace(
            "name: \"regenerate_artifact_preview\".to_owned(),",
            "name: \"regenerate_artifact_preview_extra\".to_owned(),",
        )
    });
    let errors = copy.errors();
    for expected in [
        "regenerate_artifact_preview_extra has no typed output schema",
        "regenerate_artifact_preview_extra is advertised but missing from tools/call dispatch",
        "regenerate_artifact_preview_extra is advertised but absent from MCP documentation",
    ] {
        assert_reported(&errors, expected);
    }
}

#[test]
fn a_duplicate_modern_declaration_is_rejected_instead_of_being_hidden_by_deduplication() {
    let copy = SurfaceCopy::new("duplicate");
    copy.edit("src/mcp/tool_defs.rs", |source| {
        format!("{source}\n    name: \"regenerate_artifact_preview\".to_owned(),\n")
    });
    assert_reported(
        &copy.errors(),
        "modernDeclarations contain duplicate names: regenerate_artifact_preview",
    );
}

#[test]
fn a_tool_missing_from_the_mcp_documentation_is_reported() {
    let copy = SurfaceCopy::new("docs");
    copy.edit("docs/mcp-api.md", |docs| {
        docs.replace("`read_artifact", "`read-artifact")
    });
    assert_reported(
        &copy.errors(),
        "read_artifact is advertised but absent from MCP documentation",
    );
}

#[test]
fn golden_drift_counts_and_missing_contract_tests_are_all_reported_together() {
    let copy = SurfaceCopy::new("several");
    copy.edit("conformance/goldens/mcp.tools-list.json", |golden| {
        golden.replacen(
            "Delete one of your artifacts by id.",
            "Delete an artifact.",
            1,
        )
    });
    copy.edit("docs/mcp-api.md", |docs| {
        docs.replace(
            "The legacy catalog contains 34 tools",
            "The legacy catalog contains 33 tools",
        )
    });
    copy.edit("tests/native/main.rs", |main| {
        main.replace("mod u26_mcp_2026;", "")
    });
    std::fs::remove_file(copy.0.join("conformance/cases/mcp.read-artifact.json"))
        .expect("remove contract case");
    let errors = copy.errors();
    for expected in [
        "frozen tool definitions drift from conformance/goldens/mcp.tools-list.json",
        "MCP tool-count statement is stale or missing",
        "MCP 2026 tests are not registered",
        "required MCP contract test is missing: conformance/cases/mcp.read-artifact.json",
    ] {
        assert_reported(&errors, expected);
    }
}

#[test]
fn guarded_match_arms_and_multiline_patterns_follow_the_original_regular_expressions() {
    let source = "let result = match name {\n        \"plain\" => a(),\n        \"guarded\" if x\n            == y => b(),\n        other => c(),\n    }?;";
    let mut errors = Errors::default();
    assert_eq!(
        tool_names_from_dispatch(source, &mut errors),
        ["guarded", "plain"]
    );
    assert!(errors.0.is_empty());
    assert_eq!(
        named_tool_definition_names(
            "x\n    name: \"a_b\".to_owned(),\nname: \"c\".to_owned(), // no\n"
        ),
        ["a_b"]
    );
    assert_eq!(
        protocol_versions_from_dispatch(
            "pub const PROTOCOL_VERSION: &str = \"1\";\npub const MODERN_PROTOCOL_VERSION: &str = \"2\";\n pub const PROTOCOL_VERSION: &str = \"3\";\n"
        ),
        ["1", "2"]
    );
    assert_eq!(
        cargo_package_version(
            "[package]\nname = \"x\"\nversion = \"1.2.3\"\n[dependencies]\nversion = \"9\"\n"
        )
        .as_deref(),
        Some("1.2.3")
    );
    assert!(has_json_member(
        "{ \"method\" :\n \"tools/list\" }",
        "method",
        "tools/list"
    ));
}
