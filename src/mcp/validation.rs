//! Exact purpose-built validation for the frozen MCP input schemas.

use serde_json::Value;

use crate::error::AppError;

use super::protocol::OrderedJson;

/// Port of `validateSchemaInput` from `lib/contracts.js`.
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
    let schema_type = schema.get("type").and_then(Value::as_str);
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

    let matches = match schema_type {
        Some("string") => matches!(value, OrderedJson::String(_)),
        Some("number") => matches!(value, OrderedJson::Number(_)),
        Some("integer") => value
            .as_number()
            .and_then(serde_json::Number::as_f64)
            .is_some_and(|number| number.is_finite() && number.fract() == 0.0),
        Some("array") => matches!(value, OrderedJson::Array(_)),
        Some("boolean") => matches!(value, OrderedJson::Bool(_)),
        Some(_) | None => true,
    };
    if !matches && let Some(schema_type) = schema_type {
        let article = if matches!(schema_type, "object" | "integer" | "array") {
            "an"
        } else {
            "a"
        };
        errors.push(format!("{path} must be {article} {schema_type}"));
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
    let mut resolved = Vec::with_capacity(values.len());
    for (index, edit) in values.iter().enumerate() {
        let number = index + 1;
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
            resolved.push(ResolvedEdit {
                number,
                start,
                end: start + needle.len(),
                replacement,
            });
            continue;
        }

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
        resolved.push(ResolvedEdit {
            number,
            start: offset,
            end,
            replacement,
        });
    }

    resolved.sort_by_key(|edit| (edit.start, edit.number));
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
    use super::*;

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
}
