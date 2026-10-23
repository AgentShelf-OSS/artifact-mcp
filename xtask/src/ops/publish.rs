//! `cargo xtask publish-data-artifact --mcp URL --html FILE --bindings FILE --title T
//! [--description D] [--org ORG] [--artifact-id ID]`: publish or update HTML through an MCP
//! endpoint, check every binding against `list_data_sources`, then attach the bindings.
//!
//! Port of the former `scripts/publish-data-artifact.mjs`, which an external homelab wrapper
//! runs as a prebuilt binary with a minimal environment (`PATH`, `LANG`, `LC_ALL`, `TMPDIR` and
//! the token). Flags, the `ARTIFACT_MCP_API_KEY` variable, the JSON-RPC requests (byte for
//! byte), stdout, stderr and exit status match the script. The token is sent only in the
//! `authorization` header and is never formatted into any message.

use std::io::Write;
use std::time::Duration;

use url::Url;

use super::http::{FetchError, Http, REDIRECT_STATUSES, fetch_header_value};
use super::js::{self, Js, Object, object};
use super::node_fs_error;

pub const TOKEN_ENV: &str = "ARTIFACT_MCP_API_KEY";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const HELP: &str = "Publish or update HTML and attach its live-data bindings.\n\n\
cargo xtask publish-data-artifact --mcp https://your-host/mcp \
--html dashboard.html --bindings bindings.json --title 'PR Watch'\n\n\
Set ARTIFACT_MCP_API_KEY through your secret manager. Optional --org selects an \
organization for an admin key; --artifact-id updates an existing artifact.";

/// Parsed flags (Node `util.parseArgs` with these string options and boolean `--help`).
#[derive(Debug, Default)]
struct Values {
    mcp: Option<String>,
    html: Option<String>,
    bindings: Option<String>,
    title: Option<String>,
    description: Option<String>,
    org: Option<String>,
    artifact_id: Option<String>,
    help: bool,
}

const STRING_OPTIONS: [&str; 7] = [
    "mcp",
    "html",
    "bindings",
    "title",
    "description",
    "org",
    "artifact-id",
];

enum Token {
    Option {
        name: String,
        raw_name: String,
        value: Option<String>,
        inline: bool,
    },
    Positional(String),
}

fn invalid_value(message: String) -> String {
    format!("TypeError [ERR_PARSE_ARGS_INVALID_OPTION_VALUE]: {message}")
}

/// `parseArgs({ options, strict: true, allowPositionals: false })`, with its error text.
fn parse_args(args: &[String]) -> Result<Values, String> {
    let mut tokens = Vec::new();
    let mut remaining: std::collections::VecDeque<String> = args.iter().cloned().collect();
    while let Some(arg) = remaining.pop_front() {
        if arg == "--" {
            tokens.extend(remaining.drain(..).map(Token::Positional));
            break;
        }
        let bytes = arg.as_bytes();
        let short = bytes.len() >= 2 && bytes[0] == b'-' && bytes[1] != b'-';
        if short {
            // No short options exist, so a lone `-x` or a group `-xyz` stops at its first letter.
            let letter: String = arg.chars().nth(1).map(String::from).unwrap_or_default();
            tokens.push(Token::Option {
                raw_name: format!("-{letter}"),
                name: letter,
                value: None,
                inline: false,
            });
            if arg.chars().count() > 2 {
                for letter in arg.chars().skip(2).collect::<Vec<_>>().into_iter().rev() {
                    remaining.push_front(format!("-{letter}"));
                }
            }
            continue;
        }
        if arg.len() > 2 && arg.starts_with("--") {
            // Node looks for `=` from index 3 (`arg.includes('=', 3)`), so `--=x` is a name.
            if let Some(equals) = arg.match_indices('=').map(|(at, _)| at).find(|at| *at >= 3) {
                let name = &arg[2..equals];
                tokens.push(Token::Option {
                    raw_name: format!("--{name}"),
                    name: name.to_owned(),
                    value: Some(arg[equals + 1..].to_owned()),
                    inline: true,
                });
            } else {
                let name = arg[2..].to_owned();
                let value = if STRING_OPTIONS.contains(&name.as_str()) {
                    remaining.pop_front()
                } else {
                    None
                };
                tokens.push(Token::Option {
                    raw_name: arg.clone(),
                    name,
                    value,
                    inline: false,
                });
            }
            continue;
        }
        tokens.push(Token::Positional(arg));
    }

    let mut values = Values::default();
    for token in tokens {
        match token {
            Token::Positional(value) => {
                return Err(format!(
                    "TypeError [ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL]: Unexpected argument '{value}'. This command does not take positional arguments"
                ));
            }
            Token::Option {
                name,
                raw_name,
                value,
                inline,
            } => {
                let is_string = STRING_OPTIONS.contains(&name.as_str());
                if !is_string && name != "help" {
                    return Err(format!(
                        "TypeError [ERR_PARSE_ARGS_UNKNOWN_OPTION]: Unknown option '{raw_name}'"
                    ));
                }
                if is_string && value.is_none() {
                    return Err(invalid_value(format!(
                        "Option '--{name} <value>' argument missing"
                    )));
                }
                if !is_string && value.is_some() {
                    return Err(invalid_value(format!(
                        "Option '--{name}' does not take an argument"
                    )));
                }
                if let Some(value) = &value
                    && !inline
                    && value.len() > 1
                    && value.starts_with('-')
                {
                    return Err(invalid_value(format!(
                        "Option '{raw_name}' argument is ambiguous.\nDid you forget to specify the option argument for '{raw_name}'?\nTo specify an option argument starting with a dash use '{raw_name}=-XYZ'."
                    )));
                }
                let slot = match name.as_str() {
                    "mcp" => &mut values.mcp,
                    "html" => &mut values.html,
                    "bindings" => &mut values.bindings,
                    "title" => &mut values.title,
                    "description" => &mut values.description,
                    "org" => &mut values.org,
                    "artifact-id" => &mut values.artifact_id,
                    _ => {
                        values.help = true;
                        continue;
                    }
                };
                *slot = value;
            }
        }
    }
    Ok(values)
}

/// The value of a flag when it is truthy (present and non-empty).
fn given(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|value| !value.is_empty())
}

/// Entry point for `cargo xtask publish-data-artifact`.
pub fn main(args: &[String]) -> bool {
    let token = super::env_var(TOKEN_ENV);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run(
        args,
        token.as_deref(),
        &mut stdout.lock(),
        &mut stderr.lock(),
    )
}

/// Run the command with an injectable token and output streams.
pub(crate) fn run(
    args: &[String],
    token: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> bool {
    let values = match parse_args(args) {
        Ok(values) => values,
        Err(message) => {
            let _ = writeln!(err, "{message}");
            return false;
        }
    };
    if values.help {
        let _ = writeln!(out, "{HELP}");
        return true;
    }
    match publish(&values, token) {
        Ok(summary) => {
            let _ = writeln!(out, "{summary}");
            true
        }
        Err(message) => {
            let _ = writeln!(err, "{message}");
            false
        }
    }
}

/// `readFile(path, "utf8")`.
fn read_text(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| node_fs_error(&error, "open", path))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

struct Client<'a> {
    http: Option<Http>,
    endpoint: &'a str,
    token: &'a str,
    request_id: u64,
}

impl Client<'_> {
    fn rpc(&mut self, method: &str, params: Js) -> Result<Option<Js>, String> {
        self.request_id += 1;
        let label = if method == "tools/call" {
            js::to_js_string(params.property("name"))
        } else {
            method.to_owned()
        };
        let body = object([
            ("jsonrpc", Js::from("2.0")),
            ("id", Js::from(self.request_id)),
            ("method", Js::from(method)),
            ("params", params),
        ]);
        let authorization = format!("Bearer {}", self.token);
        let Some(authorization) = fetch_header_value(&authorization) else {
            return Err(
                "Headers.append: \"Bearer [REDACTED]\" is an invalid header value.".to_owned(),
            );
        };
        if self.http.is_none() {
            self.http = Some(
                Http::new(Some(REQUEST_TIMEOUT), false).map_err(|_| "fetch failed".to_owned())?,
            );
        }
        let http = self.http.as_ref().expect("initialised above");
        let response = http
            .send(
                "POST",
                self.endpoint,
                &[
                    ("authorization", authorization),
                    ("content-type", "application/json"),
                    ("accept", "application/json, text/event-stream"),
                ],
                Some(js::stringify(&body).into_bytes()),
            )
            .map_err(|error| match error {
                FetchError::Timeout => "The operation was aborted due to timeout".to_owned(),
                FetchError::Network => "fetch failed".to_owned(),
                FetchError::InvalidHeader(_) => {
                    "Headers.append: \"Bearer [REDACTED]\" is an invalid header value.".to_owned()
                }
            })?;
        if REDIRECT_STATUSES.contains(&response.status) {
            return Err("fetch failed".to_owned());
        }
        if !response.ok() {
            return Err(format!("MCP request failed with HTTP {}.", response.status));
        }
        let text = response.text();
        let result = js::parse(&text).map_err(|error| js::parse_error_message(&text, &error))?;
        let failed = js::truthy(js::get(Some(&result), "error")?)
            || js::truthy(js::get_opt(js::get(Some(&result), "result")?, "isError"));
        if failed {
            return Err(format!(
                "MCP {label} failed. Check source configuration and publisher permissions."
            ));
        }
        Ok(result.property("result").cloned())
    }

    fn call(&mut self, name: &str, args: Js) -> Result<Js, String> {
        let result = self.rpc(
            "tools/call",
            object([("name", Js::from(name)), ("arguments", args)]),
        )?;
        let structured = js::get(result.as_ref(), "structuredContent")?;
        if js::truthy(structured) {
            return Ok(structured.cloned().unwrap_or(Js::Null));
        }
        let text = match js::get(result.as_ref(), "content")? {
            None | Some(Js::Null) => None,
            Some(Js::Array(items)) => {
                let mut found = None;
                for item in items {
                    if js::is_str(js::get(Some(item), "type")?, "text") {
                        found = Some(item);
                        break;
                    }
                }
                js::get_opt(found, "text")
            }
            Some(_) => return Err("result.content?.find is not a function".to_owned()),
        };
        match text {
            Some(Js::String(text)) => {
                js::parse(text).map_err(|error| js::parse_error_message(text, &error))
            }
            _ => Err(format!("MCP {name} returned no structured result.")),
        }
    }
}

/// `for (const item of value ?? [])`.
fn iterate(value: Option<&Js>, label: &str) -> Result<Vec<Js>, String> {
    match value {
        None | Some(Js::Null) => Ok(Vec::new()),
        Some(Js::Array(items)) => Ok(items.clone()),
        Some(Js::String(text)) => Ok(text.chars().map(|c| Js::String(c.to_string())).collect()),
        Some(_) => Err(format!("{label} is not iterable")),
    }
}

/// `Object.hasOwn(target, key)`.
fn has_own(target: Option<&Js>, key: &Js) -> Result<bool, String> {
    let key = js::to_js_string(Some(key));
    match target {
        None | Some(Js::Null) => Err("Cannot convert undefined or null to object".to_owned()),
        Some(Js::Object(object)) => Ok(object.get(&key).is_some()),
        Some(Js::Array(items)) => Ok(key == "length"
            || key
                .parse::<usize>()
                .is_ok_and(|index| index < items.len() && index.to_string() == key)),
        Some(Js::String(text)) => Ok(key == "length"
            || key.parse::<usize>().is_ok_and(|index| {
                index < text.encode_utf16().count() && index.to_string() == key
            })),
        Some(_) => Ok(false),
    }
}

fn publish(values: &Values, token: Option<&str>) -> Result<String, String> {
    let token = token
        .filter(|token| !token.is_empty())
        .ok_or_else(|| format!("Set {TOKEN_ENV} through your secret manager."))?;
    let (Some(mcp), Some(html_path), Some(bindings_path), Some(title)) = (
        given(values.mcp.as_ref()),
        given(values.html.as_ref()),
        given(values.bindings.as_ref()),
        given(values.title.as_ref()),
    ) else {
        return Err(
            "Required arguments: --mcp, --html, --bindings, --title. Use --help for an example."
                .to_owned(),
        );
    };
    let endpoint = Url::parse(mcp).map_err(|_| "Invalid URL".to_owned())?;
    if !["http", "https"].contains(&endpoint.scheme())
        || !endpoint.username().is_empty()
        || endpoint
            .password()
            .is_some_and(|password| !password.is_empty())
        || endpoint
            .fragment()
            .is_some_and(|fragment| !fragment.is_empty())
        || endpoint.query().is_some_and(|query| !query.is_empty())
    {
        return Err(
            "--mcp must be an HTTP(S) endpoint without credentials, query or fragment.".to_owned(),
        );
    }
    let html = read_text(html_path)?;
    let manifest_text = read_text(bindings_path)?;
    let manifest = js::parse(&manifest_text)
        .map_err(|error| js::parse_error_message(&manifest_text, &error))?;
    let bindings = match js::get(Some(&manifest), "bindings")? {
        Some(Js::Object(bindings)) => bindings.clone(),
        _ => return Err("The bindings file must contain a bindings object.".to_owned()),
    };

    let mut client = Client {
        http: None,
        endpoint: endpoint.as_str(),
        token,
        request_id: 0,
    };
    client.rpc(
        "initialize",
        object([
            ("protocolVersion", Js::from("2025-06-18")),
            ("capabilities", Js::Object(Object::new())),
            (
                "clientInfo",
                object([
                    ("name", Js::from("artifact-data-publisher")),
                    ("version", Js::from("1.0.0")),
                ]),
            ),
        ]),
    )?;
    let org = given(values.org.as_ref());
    let discovery = client.call(
        "list_data_sources",
        match org {
            Some(org) => object([("org", Js::from(org))]),
            None => Js::Object(Object::new()),
        },
    )?;
    let sources = match js::get(Some(&discovery), "sources")? {
        Some(Js::Array(sources)) => sources.clone(),
        None => return Err("Cannot read properties of undefined (reading 'map')".to_owned()),
        Some(Js::Null) => return Err("Cannot read properties of null (reading 'map')".to_owned()),
        Some(_) => return Err("discovery.sources.map is not a function".to_owned()),
    };
    let mut by_id: Vec<(Option<&Js>, &Js)> = Vec::new();
    for source in &sources {
        by_id.push((js::get(Some(source), "id")?, source));
    }
    for binding in bindings.values() {
        let wanted = js::get(Some(binding), "source")?;
        let Some((_, source)) = by_id
            .iter()
            .rev()
            .find(|(id, _)| js::same_value_zero(*id, wanted))
        else {
            return Err("A bound source is unavailable to this publisher. Check the source organization before publishing.".to_owned());
        };
        for operation in iterate(binding.property("operations"), "binding.operations")? {
            if !has_own(source.property("operations"), &operation)? {
                return Err("A binding requests an unavailable operation.".to_owned());
            }
        }
        for subscription in iterate(binding.property("subscriptions"), "binding.subscriptions")? {
            if !has_own(source.property("subscriptions"), &subscription)? {
                return Err("A binding requests an unavailable subscription.".to_owned());
            }
        }
    }

    let description = given(values.description.as_ref());
    let publication = if let Some(artifact_id) = given(values.artifact_id.as_ref()) {
        let mut args = Object::new();
        args.insert("id", Js::from(artifact_id));
        args.insert("html", Js::from(html.as_str()));
        args.insert("title", Js::from(title));
        if let Some(description) = description {
            args.insert("description", Js::from(description));
        }
        client.call("update_artifact", Js::Object(args))?
    } else {
        let mut args = Object::new();
        args.insert("html", Js::from(html.as_str()));
        args.insert("title", Js::from(title));
        if let Some(description) = description {
            args.insert("description", Js::from(description));
        }
        if let Some(org) = org {
            args.insert("org", Js::from(org));
        }
        client.call("publish_artifact", Js::Object(args))?
    };
    // `publication.id ?? values["artifact-id"]`
    let artifact_id = match js::get(Some(&publication), "id")? {
        Some(id) if *id != Js::Null => Some(id.clone()),
        _ => values.artifact_id.clone().map(Js::String),
    };
    let mut binding_args = Object::new();
    if let Some(id) = &artifact_id {
        binding_args.insert("id", id.clone());
    }
    binding_args.insert("bindings", Js::Object(bindings.clone()));
    if let Err(message) = client.call("set_data_bindings", Js::Object(binding_args)) {
        let id = js::to_js_string(artifact_id.as_ref());
        return Err(format!(
            "{message} HTML was published at artifact {id}; retry with --artifact-id {id} after correcting the bindings."
        ));
    }
    let mut summary = Object::new();
    if let Some(id) = artifact_id {
        summary.insert("id", id);
    }
    if let Some(url) = publication.property("url") {
        summary.insert("url", url.clone());
    }
    summary.insert(
        "bindings",
        Js::Array(bindings.keys().map(Js::from).collect()),
    );
    Ok(js::stringify_pretty(&Js::Object(summary)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn parse_error(list: &[&str]) -> String {
        parse_args(&args(list)).expect_err("parseArgs rejects these arguments")
    }

    #[test]
    fn flags_parse_like_node_parse_args() {
        let values = parse_args(&args(&[
            "--mcp=http://a/mcp",
            "--html",
            "-",
            "--title=-x",
            "--title",
            "T",
            "--description=",
            "--help",
        ]))
        .unwrap();
        assert_eq!(values.mcp.as_deref(), Some("http://a/mcp"));
        assert_eq!(values.html.as_deref(), Some("-"));
        assert_eq!(values.title.as_deref(), Some("T"));
        assert_eq!(values.description.as_deref(), Some(""));
        assert!(values.help);
    }

    #[test]
    fn flag_errors_match_node_parse_args() {
        assert_eq!(
            parse_error(&["--foo"]),
            "TypeError [ERR_PARSE_ARGS_UNKNOWN_OPTION]: Unknown option '--foo'"
        );
        assert_eq!(
            parse_error(&["--=x"]),
            "TypeError [ERR_PARSE_ARGS_UNKNOWN_OPTION]: Unknown option '--=x'"
        );
        assert_eq!(
            parse_error(&["-abc"]),
            "TypeError [ERR_PARSE_ARGS_UNKNOWN_OPTION]: Unknown option '-a'"
        );
        assert_eq!(
            parse_error(&["-h"]),
            "TypeError [ERR_PARSE_ARGS_UNKNOWN_OPTION]: Unknown option '-h'"
        );
        assert_eq!(
            parse_error(&["--help", "--foo=1"]),
            "TypeError [ERR_PARSE_ARGS_UNKNOWN_OPTION]: Unknown option '--foo'"
        );
        assert_eq!(
            parse_error(&["x"]),
            "TypeError [ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL]: Unexpected argument 'x'. This command does not take positional arguments"
        );
        assert_eq!(
            parse_error(&["--", "x"]),
            "TypeError [ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL]: Unexpected argument 'x'. This command does not take positional arguments"
        );
        assert_eq!(
            parse_error(&["--mcp"]),
            "TypeError [ERR_PARSE_ARGS_INVALID_OPTION_VALUE]: Option '--mcp <value>' argument missing"
        );
        assert_eq!(
            parse_error(&["--help=1"]),
            "TypeError [ERR_PARSE_ARGS_INVALID_OPTION_VALUE]: Option '--help' does not take an argument"
        );
        assert_eq!(
            parse_error(&["--title", "-x"]),
            "TypeError [ERR_PARSE_ARGS_INVALID_OPTION_VALUE]: Option '--title' argument is ambiguous.\nDid you forget to specify the option argument for '--title'?\nTo specify an option argument starting with a dash use '--title=-XYZ'."
        );
    }
}
