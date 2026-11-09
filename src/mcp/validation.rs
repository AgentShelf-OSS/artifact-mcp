//! Exact purpose-built validation for the frozen MCP input schemas.

use serde_json::Value;

use crate::error::AppError;

use super::protocol::OrderedJson;

/// Validate input against the advertised schema.
#[must_use]
pub fn validate_schema_input(
    schema: &Value,
    value: &OrderedJson,
    property_order: &[&str],
) -> Vec<String> {
    let mut errors = Vec::new();
    validate_node(schema, value, "", property_order, &mut errors);
    errors
        .into_iter()
        .map(|error| {
            error
                .strip_prefix(" must")
                .map_or(error.clone(), |suffix| format!("arguments must{suffix}"))
        })
        .collect()
}

fn validate_node(
    schema: &Value,
    value: &OrderedJson,
    path: &str,
    property_order: &[&str],
    errors: &mut Vec<String>,
) {
    let schema_type = match schema.get("type") {
        Some(Value::Array(types)) => {
            let types = types.iter().filter_map(Value::as_str).collect::<Vec<_>>();
            let Some(matching) = types
                .iter()
                .copied()
                .find(|schema_type| matches_schema_type(schema_type, value))
            else {
                errors.push(format!("{path} must be one of: {}", types.join(", ")));
                return;
            };
            Some(matching)
        }
        Some(schema_type) => schema_type.as_str(),
        None => None,
    };
    if schema_type == Some("array") {
        let Some(values) = value.as_array() else {
            errors.push(format!("{path} must be an array"));
            return;
        };
        if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64)
            && u64::try_from(values.len()).unwrap_or(u64::MAX) < minimum
        {
            let suffix = if minimum == 1 { "" } else { "s" };
            errors.push(format!(
                "{path} must contain at least {minimum} item{suffix}"
            ));
        }
        if let Some(items) = schema.get("items") {
            for (index, item) in values.iter().enumerate() {
                validate_node(items, item, &format!("{path}.{index}"), &[], errors);
            }
        }
        return;
    }
    if schema_type == Some("object") {
        if value.as_object().is_none() {
            errors.push(format!("{path} must be an object"));
            return;
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !value.contains_key(key) {
                    errors.push(format!("{}{key} is required", path_prefix(path)));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        let unknown = value
            .object_entries()
            .into_iter()
            .filter(|(key, _)| properties.is_none_or(|known| !known.contains_key(*key)))
            .collect::<Vec<_>>();
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            for (key, _) in &unknown {
                errors.push(format!("{}{key} is not allowed", path_prefix(path)));
            }
        }
        if let Some(properties) = properties {
            let fallback = properties.keys().map(String::as_str).collect::<Vec<_>>();
            let ordered = if property_order.is_empty() {
                fallback.as_slice()
            } else {
                property_order
            };
            for key in ordered {
                if let (Some(child_schema), Some(child)) = (properties.get(*key), value.get(key)) {
                    let child_path = if path.is_empty() {
                        (*key).to_owned()
                    } else {
                        format!("{path}.{key}")
                    };
                    validate_node(child_schema, child, &child_path, &[], errors);
                }
            }
        }
        if let Some(additional_schema) = schema
            .get("additionalProperties")
            .filter(|value| value.is_object())
        {
            for (key, child) in unknown {
                let child_path = if path.is_empty() {
                    key.to_owned()
                } else {
                    format!("{path}.{key}")
                };
                validate_node(additional_schema, child, &child_path, &[], errors);
            }
        }
        return;
    }

    if let Some(schema_type) = schema_type
        && !matches_schema_type(schema_type, value)
    {
        let article = if matches!(schema_type, "object" | "integer" | "array") {
            "an"
        } else {
            "a"
        };
        errors.push(format!("{path} must be {article} {schema_type}"));
    }
}

fn matches_schema_type(schema_type: &str, value: &OrderedJson) -> bool {
    match schema_type {
        "null" => matches!(value, OrderedJson::Null),
        "string" => matches!(value, OrderedJson::String(_)),
        "number" => matches!(value, OrderedJson::Number(_)),
        "integer" => value
            .as_number()
            .and_then(serde_json::Number::as_f64)
            .is_some_and(|number| number.is_finite() && number.fract() == 0.0),
        "array" => matches!(value, OrderedJson::Array(_)),
        "object" => matches!(value, OrderedJson::Object(_)),
        "boolean" => matches!(value, OrderedJson::Bool(_)),
        _ => true,
    }
}

#[derive(Debug, PartialEq, Eq)]
struct ResolvedEdit {
    number: usize,
    start: usize,
    end: usize,
    replacement: Vec<u8>,
}

/// Apply one atomic batch against the original UTF-8 bytes.
///
/// The returned buffer is not persisted here. Dispatch validates every edit first and only then
/// passes the complete result through the existing artifact update lifecycle.
pub fn apply_utf8_edits(content: &[u8], edits: &OrderedJson) -> Result<Vec<u8>, AppError> {
    let values = edits
        .as_array()
        .ok_or_else(|| AppError::Validation("edits must be an array".to_owned()))?;
    let mut resolved = resolve_edits(content, values)?;
    resolved.sort_by_key(|edit| (edit.start, edit.number));
    reject_overlapping_edits(&resolved)?;
    apply_resolved_edits(content, resolved)
}

fn resolve_edits(content: &[u8], values: &[OrderedJson]) -> Result<Vec<ResolvedEdit>, AppError> {
    let mut resolved = Vec::with_capacity(values.len());
    for (index, edit) in values.iter().enumerate() {
        resolved.push(resolve_edit(content, edit, index + 1)?);
    }
    Ok(resolved)
}

fn reject_overlapping_edits(resolved: &[ResolvedEdit]) -> Result<(), AppError> {
    for pair in resolved.windows(2) {
        let previous = &pair[0];
        let current = &pair[1];
        if current.start < previous.end {
            return Err(AppError::Validation(format!(
                "edits {} and {} overlap in the original content",
                previous.number, current.number
            )));
        }
    }
    Ok(())
}

fn apply_resolved_edits(content: &[u8], resolved: Vec<ResolvedEdit>) -> Result<Vec<u8>, AppError> {
    let replacement_bytes = resolved.iter().fold(0usize, |total, edit| {
        total.saturating_add(edit.replacement.len())
    });
    let removed_bytes = resolved.iter().fold(0usize, |total, edit| {
        total.saturating_add(edit.end - edit.start)
    });
    let mut output = Vec::with_capacity(
        content
            .len()
            .saturating_sub(removed_bytes)
            .saturating_add(replacement_bytes),
    );
    let mut cursor = 0;
    for edit in resolved {
        output.extend_from_slice(&content[cursor..edit.start]);
        output.extend_from_slice(&edit.replacement);
        cursor = edit.end;
    }
    output.extend_from_slice(&content[cursor..]);
    if output == content {
        return Err(AppError::Validation(
            "Patch did not change artifact content".to_owned(),
        ));
    }
    Ok(output)
}

fn resolve_edit(
    content: &[u8],
    edit: &OrderedJson,
    number: usize,
) -> Result<ResolvedEdit, AppError> {
    let has_find = edit.contains_key("find");
    let has_range = edit.contains_key("offset") || edit.contains_key("length");
    if has_find == has_range {
        return Err(AppError::Validation(format!(
            "edit {number} must use either find/replace or offset/length/replace"
        )));
    }
    let replacement = edit
        .get("replace")
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| AppError::Validation(format!("edit {number} replace is required")))?
        .as_bytes()
        .to_vec();
    if has_find {
        return resolve_find_edit(content, edit, number, replacement);
    }
    resolve_range_edit(content, edit, number, replacement)
}

fn resolve_find_edit(
    content: &[u8],
    edit: &OrderedJson,
    number: usize,
    replacement: Vec<u8>,
) -> Result<ResolvedEdit, AppError> {
    let needle = edit
        .get("find")
        .and_then(OrderedJson::as_str)
        .ok_or_else(|| AppError::Validation(format!("edit {number} find is required")))?
        .as_bytes();
    if needle.is_empty() {
        return Err(AppError::Validation(format!(
            "edit {number} find must not be empty"
        )));
    }
    let matches = content
        .windows(needle.len())
        .enumerate()
        .filter_map(|(offset, window)| (window == needle).then_some(offset))
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        return Err(AppError::Validation(format!(
            "edit {number} find matched {} times; expected exactly once",
            matches.len()
        )));
    }
    let start = matches[0];
    Ok(ResolvedEdit {
        number,
        start,
        end: start + needle.len(),
        replacement,
    })
}

fn resolve_range_edit(
    content: &[u8],
    edit: &OrderedJson,
    number: usize,
    replacement: Vec<u8>,
) -> Result<ResolvedEdit, AppError> {
    let offset = edit_non_negative_integer(edit, number, "offset")?;
    let length = edit_non_negative_integer(edit, number, "length")?;
    if offset > content.len() || length > content.len() - offset {
        return Err(AppError::Validation(format!(
            "edit {number} range exceeds content length of {} bytes",
            content.len()
        )));
    }
    let end = offset + length;
    if offset < content.len() && content[offset] & 0xc0 == 0x80 {
        return Err(AppError::Validation(format!(
            "edit {number} offset {offset} is not a UTF-8 boundary"
        )));
    }
    if end < content.len() && content[end] & 0xc0 == 0x80 {
        return Err(AppError::Validation(format!(
            "edit {number} range end {end} is not a UTF-8 boundary"
        )));
    }
    Ok(ResolvedEdit {
        number,
        start: offset,
        end,
        replacement,
    })
}

fn edit_non_negative_integer(
    edit: &OrderedJson,
    number: usize,
    field: &str,
) -> Result<usize, AppError> {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    let valid = edit
        .get(field)
        .and_then(OrderedJson::as_number)
        .and_then(serde_json::Number::as_f64)
        .filter(|value| {
            value.is_finite() && *value >= 0.0 && *value <= MAX_SAFE_INTEGER && value.fract() == 0.0
        });
    let Some(value) = valid else {
        return Err(AppError::Validation(format!(
            "edit {number} {field} must be a non-negative integer"
        )));
    };
    Ok(if value >= usize::MAX as f64 {
        usize::MAX
    } else {
        value as usize
    })
}

fn path_prefix(path: &str) -> String {
    if path.is_empty() {
        String::new()
    } else {
        format!("{path}.")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    // Descriptions annotate schemas; they impose no validation constraint.
    const ANNOTATIONS: &[&str] = &["description"];
    // Advertised client constraints are intentionally not enforced by the schema validator.
    // src/mcp/collections.rs and src/persistence/collections.rs normalize inputs first and
    // own the existing error contract, so enforcing these here would change accepted inputs.
    const HANDLER_ENFORCED: &[&str] = &["maxItems", "minLength", "maxLength", "minimum", "maximum"];

    fn schema_errors(schema: &Value, value: Value) -> Vec<String> {
        let value = serde_json::from_value(value).expect("valid ordered JSON");
        validate_schema_input(schema, &value, &[])
    }

    fn unsupported_schema_keywords(
        schema: &Value,
        path: &str,
        seen: &mut BTreeSet<String>,
        errors: &mut Vec<String>,
    ) {
        for (keyword, value) in schema.as_object().expect("schema object") {
            let child_path = format!("{path}[{keyword:?}]");
            seen.insert(keyword.clone());
            match keyword.as_str() {
                "type" | "required" | "minItems" => {}
                keyword
                    if ANNOTATIONS.contains(&keyword) || HANDLER_ENFORCED.contains(&keyword) => {}
                "properties" => {
                    for (name, child) in value.as_object().expect("schema properties") {
                        unsupported_schema_keywords(
                            child,
                            &format!("{child_path}[{name:?}]"),
                            seen,
                            errors,
                        );
                    }
                }
                "items" | "additionalProperties" => {
                    if value.is_object() {
                        unsupported_schema_keywords(value, &child_path, seen, errors);
                    }
                }
                _ => errors.push(format!(
                    "{child_path}: unsupported schema keyword {keyword}"
                )),
            }
        }
    }

    #[test]
    fn every_tool_schema_uses_only_implemented_or_explicitly_allowed_keywords() {
        let schemas: Value = serde_json::from_str(include_str!(
            "../../conformance/mcp.tool-output-schemas.json"
        ))
        .expect("valid output schemas");
        let mut errors = Vec::new();
        let mut seen = BTreeSet::new();
        for (tool, schema) in schemas.as_object().expect("output schemas") {
            unsupported_schema_keywords(
                schema,
                &format!("output {tool}: $"),
                &mut seen,
                &mut errors,
            );
        }
        for definition in
            super::super::tool_defs::frozen_tool_definitions().expect("frozen tool definitions")
        {
            unsupported_schema_keywords(
                &definition.input_schema,
                &format!("input {}: $", definition.name),
                &mut seen,
                &mut errors,
            );
        }
        assert!(errors.is_empty(), "{}", errors.join("\n"));
        for (kind, keywords) in [
            ("annotation", ANNOTATIONS),
            ("handler-enforced", HANDLER_ENFORCED),
        ] {
            for keyword in keywords {
                assert!(seen.contains(*keyword), "unused {kind} keyword: {keyword}");
            }
        }
    }

    #[test]
    fn keyword_guard_reports_typos_and_unimplemented_constraints_at_schema_paths() {
        let schema = serde_json::json!({
            "requird": ["items"],
            "propertes": {},
            "properties": {
                "enum": {
                    "type": "array",
                    "items": { "enum": ["allowed"] }
                },
                "values": {
                    "type": "object",
                    "additionalProperties": { "format": "date-time" }
                }
            }
        });
        let mut errors = Vec::new();
        unsupported_schema_keywords(
            &schema,
            "input example: $",
            &mut BTreeSet::new(),
            &mut errors,
        );
        assert_eq!(errors.len(), 4, "{errors:?}");
        for path in [
            "$[\"requird\"]",
            "$[\"propertes\"]",
            "$[\"properties\"][\"enum\"][\"items\"][\"enum\"]",
            "$[\"properties\"][\"values\"][\"additionalProperties\"][\"format\"]",
        ] {
            assert!(
                errors
                    .iter()
                    .any(|error| error.starts_with(&format!("input example: {path}:"))),
                "missing schema path {path}: {errors:?}"
            );
        }
    }

    #[test]
    fn collection_constraints_keep_handler_normalization_and_error_contracts() {
        let definitions =
            super::super::tool_defs::frozen_tool_definitions().expect("frozen tool definitions");
        for (tool, value) in [
            ("list_collections", serde_json::json!({ "limit": -1 })),
            ("list_collections", serde_json::json!({ "limit": 0 })),
            ("list_collections", serde_json::json!({ "limit": 101 })),
            (
                "create_collection",
                serde_json::json!({ "name": format!(" {} ", "a".repeat(79)) }),
            ),
            (
                "create_collection",
                serde_json::json!({ "name": "example", "description": " ".repeat(501) }),
            ),
            ("create_collection", serde_json::json!({ "name": "" })),
            (
                "create_collection",
                serde_json::json!({ "name": "example", "artifact_ids": vec!["artifact"; 101] }),
            ),
        ] {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == tool)
                .expect("frozen collection tool");
            assert_eq!(
                schema_errors(&definition.input_schema, value.clone()),
                Vec::<String>::new(),
                "{tool} must leave normalized constraints and errors to handlers: {value}"
            );
        }
    }

    #[test]
    fn nullable_type_union_accepts_members_and_rejects_other_types() {
        let schema = serde_json::json!({ "type": ["string", "null"] });
        assert!(schema_errors(&schema, Value::Null).is_empty());
        assert!(schema_errors(&schema, serde_json::json!("text")).is_empty());
        assert_eq!(
            schema_errors(&schema, serde_json::json!(42)),
            ["arguments must be one of: string, null"]
        );
    }

    #[test]
    fn type_unions_validate_matching_object_and_array_members() {
        let schema = serde_json::json!({
            "type": ["object", "null"],
            "properties": {
                "items": {
                    "type": ["array", "null"],
                    "minItems": 1,
                    "items": { "type": ["integer", "null"] }
                }
            },
            "required": ["items"]
        });
        for value in [
            Value::Null,
            serde_json::json!({ "items": null }),
            serde_json::json!({ "items": [1, null] }),
        ] {
            assert!(schema_errors(&schema, value).is_empty());
        }
        for (value, expected) in [
            (serde_json::json!({}), "items is required"),
            (
                serde_json::json!({ "items": [] }),
                "items must contain at least 1 item",
            ),
            (
                serde_json::json!({ "items": [1.5] }),
                "items.0 must be one of: integer, null",
            ),
        ] {
            assert_eq!(schema_errors(&schema, value), [expected]);
        }
    }

    #[test]
    fn single_type_errors_keep_their_exact_messages() {
        for (schema_type, value, expected) in [
            (
                "string",
                serde_json::json!(42),
                "arguments must be a string",
            ),
            ("number", Value::Null, "arguments must be a number"),
            (
                "integer",
                serde_json::json!(1.5),
                "arguments must be an integer",
            ),
            ("boolean", Value::Null, "arguments must be a boolean"),
            ("array", Value::Null, "arguments must be an array"),
            ("object", Value::Null, "arguments must be an object"),
        ] {
            let schema = serde_json::json!({ "type": schema_type });
            assert_eq!(schema_errors(&schema, value), [expected]);
        }
    }

    #[test]
    fn artifact_output_schemas_accept_recorded_list_and_read_results() {
        let schemas: Value = serde_json::from_str(include_str!(
            "../../conformance/mcp.tool-output-schemas.json"
        ))
        .expect("valid output schemas");
        let fixtures = [
            (
                "mcp.read-artifact.json",
                include_str!("../../conformance/goldens/mcp.read-artifact.json"),
            ),
            (
                "mcp.publish-artifact-key.json",
                include_str!("../../conformance/goldens/mcp.publish-artifact-key.json"),
            ),
        ];
        let mut listed = 0;
        let mut read = 0;
        for (name, fixture) in fixtures {
            let mut checked = 0;
            let golden: Value = serde_json::from_str(fixture).expect("valid golden");
            for (index, step) in golden["steps"]
                .as_array()
                .expect("golden steps")
                .iter()
                .enumerate()
            {
                let Some(output) = step.pointer("/body/json/result/structuredContent") else {
                    continue;
                };
                let tool = if output.get("artifacts").is_some() {
                    listed += 1;
                    "list_artifacts"
                } else if output.get("content_type").is_some() {
                    read += 1;
                    "read_artifact"
                } else {
                    continue;
                };
                assert_eq!(
                    schema_errors(&schemas[tool], output.clone()),
                    Vec::<String>::new(),
                    "{name} step {index}: {tool}"
                );
                checked += 1;
            }
            assert!(checked > 0, "no list/read results checked in {name}");
        }
        assert!(listed > 0, "no list_artifacts results checked");
        assert!(read > 0, "no read_artifact results checked");
    }

    #[test]
    fn artifact_output_schemas_reject_malformed_item_fields() {
        let schemas: Value = serde_json::from_str(include_str!(
            "../../conformance/mcp.tool-output-schemas.json"
        ))
        .expect("valid output schemas");
        let golden: Value = serde_json::from_str(include_str!(
            "../../conformance/goldens/mcp.read-artifact.json"
        ))
        .expect("valid golden");
        let listed = &golden["steps"][1]["body"]["json"]["result"]["structuredContent"];
        let mut wrong_type = listed.clone();
        wrong_type["artifacts"][0]["revision"] = serde_json::json!("1");
        assert_eq!(
            schema_errors(&schemas["list_artifacts"], wrong_type),
            ["artifacts.0.revision must be an integer"]
        );
        let mut missing = listed.clone();
        missing["artifacts"][0]
            .as_object_mut()
            .expect("artifact item")
            .remove("title");
        assert_eq!(
            schema_errors(&schemas["list_artifacts"], missing),
            ["artifacts.0.title is required"]
        );
        let mut extra = listed.clone();
        extra["artifacts"][0]["surprise"] = serde_json::json!(true);
        assert_eq!(
            schema_errors(&schemas["list_artifacts"], extra),
            ["artifacts.0.surprise is not allowed"]
        );

        let bundle = &golden["steps"][6]["body"]["json"]["result"]["structuredContent"];
        let mut wrong_type = bundle.clone();
        wrong_type["files"][0]["entry"] = serde_json::json!(1);
        assert_eq!(
            schema_errors(&schemas["read_artifact"], wrong_type),
            ["files.0.entry must be a boolean"]
        );
        let mut missing = bundle.clone();
        missing["files"][0]
            .as_object_mut()
            .expect("bundle file")
            .remove("path");
        assert_eq!(
            schema_errors(&schemas["read_artifact"], missing),
            ["files.0.path is required"]
        );
        let mut extra = bundle.clone();
        extra["files"][0]["surprise"] = serde_json::json!(true);
        assert_eq!(
            schema_errors(&schemas["read_artifact"], extra),
            ["files.0.surprise is not allowed"]
        );
        let mut extra = bundle.clone();
        extra["surprise"] = serde_json::json!(true);
        assert_eq!(
            schema_errors(&schemas["read_artifact"], extra),
            ["surprise is not allowed"]
        );
    }

    #[test]
    fn reports_required_unknown_known_and_nested_errors_in_node_order() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "html": { "type": "string" },
                "files": { "type": "object", "additionalProperties": { "type": "string" } }
            },
            "required": ["html"],
            "additionalProperties": false
        });
        let input: OrderedJson =
            serde_json::from_str(r#"{"z":true,"files":{"z.css":42,"a.css":false},"a":true}"#)
                .expect("valid input");
        assert_eq!(
            validate_schema_input(&schema, &input, &["html", "files"]),
            [
                "html is required",
                "z is not allowed",
                "a is not allowed",
                "files.z.css must be a string",
                "files.a.css must be a string",
            ]
        );
    }

    #[test]
    fn validates_nested_patch_edit_arrays_in_node_order() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "find": { "type": "string" },
                            "length": { "type": "integer" },
                            "offset": { "type": "integer" },
                            "replace": { "type": "string" }
                        },
                        "required": ["replace"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["edits"],
            "additionalProperties": false
        });
        let empty: OrderedJson = serde_json::from_str(r#"{"edits":[]}"#).expect("valid input");
        assert_eq!(
            validate_schema_input(&schema, &empty, &["edits"]),
            ["edits must contain at least 1 item"]
        );

        let malformed: OrderedJson =
            serde_json::from_str(r#"{"edits":[{"find":"x","replace":42,"surprise":true}]}"#)
                .expect("valid input");
        assert_eq!(
            validate_schema_input(&schema, &malformed, &["edits"]),
            [
                "edits.0.surprise is not allowed",
                "edits.0.replace must be a string",
            ]
        );
    }

    #[test]
    fn patch_edits_count_overlapping_finds_and_reject_split_utf8_ranges() {
        let ambiguous: OrderedJson =
            serde_json::from_str(r#"[{"find":"aa","replace":"x"}]"#).expect("valid edits");
        assert_eq!(
            apply_utf8_edits(b"aaa", &ambiguous),
            Err(AppError::Validation(
                "edit 1 find matched 2 times; expected exactly once".to_owned()
            ))
        );

        let split: OrderedJson = serde_json::from_str(r#"[{"offset":2,"length":1,"replace":"x"}]"#)
            .expect("valid edits");
        assert_eq!(
            apply_utf8_edits("A🎉B".as_bytes(), &split),
            Err(AppError::Validation(
                "edit 1 offset 2 is not a UTF-8 boundary".to_owned()
            ))
        );
    }

    #[test]
    fn patch_edits_report_shape_before_missing_replacement() {
        let invalid: OrderedJson =
            serde_json::from_str(r#"[{"find":"x","offset":0,"length":1}]"#).expect("valid edits");

        assert_eq!(
            apply_utf8_edits(b"x", &invalid),
            Err(AppError::Validation(
                "edit 1 must use either find/replace or offset/length/replace".to_owned()
            ))
        );
    }
}
