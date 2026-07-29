//! Direct streamable-HTTP JSON-RPC envelopes and batching.
//!
//! The request AST deliberately retains object insertion order. Two observable Node behaviours
//! depend on it: `Object.keys` determines validation-error traversal order, and `Object.entries`
//! determines the first bundle HTML file selected when no entry is supplied.

use std::{fmt, future::Future};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Number, Value};

use crate::{AppDeps, error::McpError, model::PublisherIdentity};

use super::dispatch::{ProtocolEra, SERVER_NAME, SERVER_VERSION, SUPPORTED_PROTOCOL_VERSIONS};

/// JSON value whose objects retain the insertion order observed by `JSON.parse`.
#[derive(Clone, Debug, PartialEq)]
pub enum OrderedJson {
    Null,
    Bool(bool),
    Number(Number),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl OrderedJson {
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        match self {
            Self::Object(entries) => entries
                .iter()
                .find_map(|(candidate, value)| (candidate == key).then_some(value)),
            _ => None,
        }
    }

    #[must_use]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    #[must_use]
    pub const fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value.as_str()),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_number(&self) -> Option<&Number> {
        match self {
            Self::Number(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(values) => Some(values.as_slice()),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_object(&self) -> Option<&[(String, Self)]> {
        match self {
            Self::Object(entries) => Some(entries.as_slice()),
            _ => None,
        }
    }

    /// `Object.keys` / `Object.entries` order: array indices numerically first, then other keys
    /// in insertion order. Duplicate JSON keys retain their first position and last value.
    #[must_use]
    pub fn object_entries(&self) -> Vec<(&str, &Self)> {
        let Some(entries) = self.as_object() else {
            return Vec::new();
        };
        let mut indices = entries
            .iter()
            .filter_map(|(key, value)| array_index(key).map(|index| (index, key.as_str(), value)))
            .collect::<Vec<_>>();
        indices.sort_by_key(|(index, _, _)| *index);
        let mut ordered = indices
            .into_iter()
            .map(|(_, key, value)| (key, value))
            .collect::<Vec<_>>();
        ordered.extend(
            entries
                .iter()
                .filter(|(key, _)| array_index(key).is_none())
                .map(|(key, value)| (key.as_str(), value)),
        );
        ordered
    }

    #[must_use]
    pub fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(values) => Value::Array(values.into_iter().map(Self::into_value).collect()),
            Self::Object(entries) => {
                let mut object = Map::new();
                for (key, value) in entries {
                    object.insert(key, value.into_value());
                }
                Value::Object(object)
            }
        }
    }

    /// Compact `JSON.stringify` equivalent used by MCP's embedded text result.
    pub fn to_json_string(&self) -> Result<String, serde_json::Error> {
        let mut output = String::new();
        self.write_json(&mut output)?;
        Ok(output)
    }

    fn write_json(&self, output: &mut String) -> Result<(), serde_json::Error> {
        match self {
            Self::Null => output.push_str("null"),
            Self::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
            Self::Number(value) => output.push_str(&js_number(value)),
            Self::String(value) => output.push_str(&serde_json::to_string(value)?),
            Self::Array(values) => {
                output.push('[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    value.write_json(output)?;
                }
                output.push(']');
            }
            Self::Object(_) => {
                output.push('{');
                for (index, (key, value)) in self.object_entries().iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    output.push_str(&serde_json::to_string(key)?);
                    output.push(':');
                    value.write_json(output)?;
                }
                output.push('}');
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn object(entries: impl IntoIterator<Item = (impl Into<String>, Self)>) -> Self {
        Self::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.into(), value))
                .collect(),
        )
    }

    #[must_use]
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }

    #[must_use]
    pub fn number_u64(value: u64) -> Self {
        Self::Number(Number::from(value))
    }

    #[must_use]
    pub fn number_i64(value: i64) -> Self {
        Self::Number(Number::from(value))
    }

    #[must_use]
    pub fn number_f64(value: f64) -> Self {
        if value == 0.0 {
            return Self::number_i64(0);
        }
        if value.is_finite()
            && value.fract() == 0.0
            && value >= i64::MIN as f64
            && value <= i64::MAX as f64
        {
            return Self::number_i64(value as i64);
        }
        Number::from_f64(value).map_or(Self::Null, Self::Number)
    }
}

fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let index = key.parse::<u32>().ok()?;
    (index < u32::MAX && index.to_string() == key).then_some(index)
}

fn js_number(value: &Number) -> String {
    if let Some(integer) = value.as_i64() {
        return integer.to_string();
    }
    if let Some(integer) = value.as_u64() {
        return integer.to_string();
    }
    let Some(number) = value.as_f64() else {
        return "null".to_owned();
    };
    if !number.is_finite() {
        return "null".to_owned();
    }
    if number == 0.0 {
        return "0".to_owned();
    }
    let raw = match serde_json::to_string(&number) {
        Ok(raw) => raw,
        Err(_) => return "null".to_owned(),
    };
    let absolute = number.abs();
    if (1e-6..1e21).contains(&absolute) {
        decimal_notation(&raw)
    } else {
        exponent_notation(&raw)
    }
}

fn decimal_notation(raw: &str) -> String {
    let Some((mantissa, exponent)) = split_exponent(raw) else {
        return raw.strip_suffix(".0").unwrap_or(raw).to_owned();
    };
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.trim_start_matches('-');
    let mut digits = unsigned.replace('.', "");
    let decimal_at = unsigned.find('.').unwrap_or(unsigned.len()) as i32 + exponent;
    let rendered = if decimal_at <= 0 {
        format!("0.{}{}", "0".repeat((-decimal_at) as usize), digits)
    } else if decimal_at as usize >= digits.len() {
        digits.push_str(&"0".repeat(decimal_at as usize - digits.len()));
        digits
    } else {
        digits.insert(decimal_at as usize, '.');
        digits
    };
    if negative {
        format!("-{rendered}")
    } else {
        rendered
    }
}

fn exponent_notation(raw: &str) -> String {
    let Some((mantissa, exponent)) = split_exponent(raw) else {
        return raw.strip_suffix(".0").unwrap_or(raw).to_owned();
    };
    let mantissa = mantissa.strip_suffix(".0").unwrap_or(mantissa);
    if exponent >= 0 {
        format!("{mantissa}e+{exponent}")
    } else {
        format!("{mantissa}e{exponent}")
    }
}

fn split_exponent(raw: &str) -> Option<(&str, i32)> {
    let position = raw.find(['e', 'E'])?;
    let exponent = raw.get(position + 1..)?.parse().ok()?;
    Some((&raw[..position], exponent))
}

impl<'de> Deserialize<'de> for OrderedJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OrderedJsonVisitor)
    }
}

struct OrderedJsonVisitor;

impl<'de> Visitor<'de> for OrderedJsonVisitor {
    type Value = OrderedJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(OrderedJson::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(OrderedJson::Number(Number::from(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(OrderedJson::Number(Number::from(value)))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        Number::from_f64(value)
            .map(OrderedJson::Number)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(OrderedJson::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(OrderedJson::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(OrderedJson::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(OrderedJson::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element()? {
            values.push(value);
        }
        Ok(OrderedJson::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut entries: Vec<(String, OrderedJson)> = Vec::new();
        while let Some((key, value)) = map.next_entry::<String, OrderedJson>()? {
            if let Some((_, existing)) = entries.iter_mut().find(|(candidate, _)| *candidate == key)
            {
                *existing = value;
            } else {
                entries.push((key, value));
            }
        }
        Ok(OrderedJson::Object(entries))
    }
}

impl Serialize for OrderedJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.clone().into_value().serialize(serializer)
    }
}

/// Run one JSON-RPC payload, preserving batch request order and omitting notification responses.
pub async fn handle_mcp(
    payload: OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
) -> Option<Value> {
    handle_mcp_for_era(payload, auth, deps, ProtocolEra::Legacy).await
}

/// Run one payload using either the legacy handshake contract or modern per-request semantics.
pub async fn handle_mcp_for_era(
    payload: OrderedJson,
    auth: &PublisherIdentity,
    deps: &AppDeps,
    era: ProtocolEra,
) -> Option<Value> {
    let auth = auth.clone();
    let deps = deps.clone();
    handle_mcp_with_era(payload, era, move |message| {
        let auth = auth.clone();
        let deps = deps.clone();
        async move { crate::mcp::dispatch::dispatch_for_era(&message, &auth, &deps, era).await }
    })
    .await
}

/// Protocol engine with an injected method dispatcher, used by the Node parity proof.
pub async fn handle_mcp_with<F, Fut>(payload: OrderedJson, dispatch: F) -> Option<Value>
where
    F: Fn(OrderedJson) -> Fut,
    Fut: Future<Output = Result<Value, McpError>>,
{
    handle_mcp_with_era(payload, ProtocolEra::Legacy, dispatch).await
}

/// Protocol engine with explicit era selection and an injected method dispatcher.
pub async fn handle_mcp_with_era<F, Fut>(
    payload: OrderedJson,
    era: ProtocolEra,
    dispatch: F,
) -> Option<Value>
where
    F: Fn(OrderedJson) -> Fut,
    Fut: Future<Output = Result<Value, McpError>>,
{
    match payload {
        OrderedJson::Array(messages) => {
            if era == ProtocolEra::Modern {
                return Some(rpc_error(
                    Value::Null,
                    -32_600,
                    "Batch requests are not supported by MCP 2026-07-28",
                ));
            }
            if messages.is_empty() {
                return Some(rpc_error(Value::Null, -32_600, "Invalid Request"));
            }
            let mut responses = Vec::new();
            for message in messages {
                if let Some(response) = handle_one(message, era, &dispatch).await {
                    responses.push(response);
                }
            }
            (!responses.is_empty()).then_some(Value::Array(responses))
        }
        message => handle_one(message, era, &dispatch).await,
    }
}

async fn handle_one<F, Fut>(message: OrderedJson, era: ProtocolEra, dispatch: &F) -> Option<Value>
where
    F: Fn(OrderedJson) -> Fut,
    Fut: Future<Output = Result<Value, McpError>>,
{
    let valid = message.as_object().is_some()
        && message.get("jsonrpc").and_then(OrderedJson::as_str) == Some("2.0")
        && message
            .get("method")
            .and_then(OrderedJson::as_str)
            .is_some();
    if !valid {
        let id = if message.as_object().is_some() && message.contains_key("id") {
            message
                .get("id")
                .cloned()
                .map_or(Value::Null, OrderedJson::into_value)
        } else {
            Value::Null
        };
        return Some(rpc_error(id, -32_600, "Invalid Request"));
    }
    let expects_response = message
        .get("id")
        .is_some_and(|id| !matches!(id, OrderedJson::Null));
    let id = message
        .get("id")
        .cloned()
        .map_or(Value::Null, OrderedJson::into_value);
    let dispatched = dispatch(message).await;
    if !expects_response {
        return None;
    }
    Some(match dispatched {
        Ok(result) => response_result(id, result_for_era(result, era)),
        Err(McpError::Protocol(error)) => protocol_error(id, &error),
        Err(McpError::Tool(error)) => tool_error(id, &error.to_string(), era),
    })
}

fn result_for_era(mut result: Value, era: ProtocolEra) -> Value {
    if era != ProtocolEra::Modern {
        return result;
    }
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    object
        .entry("resultType")
        .or_insert_with(|| Value::String("complete".to_owned()));
    let meta = object
        .entry("_meta")
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    if let Some(meta) = meta.as_object_mut() {
        meta.insert(
            "io.modelcontextprotocol/serverInfo".to_owned(),
            serde_json::json!({ "name": SERVER_NAME, "version": SERVER_VERSION }),
        );
    }
    result
}

fn response_result(id: Value, result: Value) -> Value {
    let mut response = Map::new();
    response.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
    response.insert("id".to_owned(), id);
    response.insert("result".to_owned(), result);
    Value::Object(response)
}

#[must_use]
pub fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    rpc_error_with_data(id, code, message, None)
}

#[must_use]
pub fn rpc_error_with_data(id: Value, code: i32, message: &str, data: Option<Value>) -> Value {
    let mut error = Map::new();
    error.insert("code".to_owned(), Value::Number(Number::from(code)));
    error.insert("message".to_owned(), Value::String(message.to_owned()));
    if let Some(data) = data {
        error.insert("data".to_owned(), data);
    }
    let mut response = Map::new();
    response.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
    response.insert("id".to_owned(), id);
    response.insert("error".to_owned(), Value::Object(error));
    Value::Object(response)
}

fn protocol_error(id: Value, error: &crate::error::JsonRpcError) -> Value {
    let data = match error {
        crate::error::JsonRpcError::UnsupportedProtocolVersion { requested } => {
            Some(serde_json::json!({
                "supported": SUPPORTED_PROTOCOL_VERSIONS,
                "requested": requested
            }))
        }
        _ => None,
    };
    rpc_error_with_data(id, error.code(), &error.to_string(), data)
}

fn tool_error(id: Value, message: &str, era: ProtocolEra) -> Value {
    let content = serde_json::json!([{ "type": "text", "text": message }]);
    response_result(
        id,
        result_for_era(
            serde_json::json!({ "content": content, "isError": true }),
            era,
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_entries_match_javascript_key_order_and_duplicate_semantics() {
        let parsed: OrderedJson =
            serde_json::from_str(r#"{"b":1,"10":2,"a":3,"2":4,"b":5,"4294967295":6}"#)
                .expect("valid ordered JSON");
        assert_eq!(
            parsed
                .object_entries()
                .into_iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            ["2", "10", "b", "a", "4294967295"]
        );
        assert_eq!(parsed.get("b"), Some(&OrderedJson::number_u64(5)));
    }

    #[test]
    fn compact_json_uses_javascript_number_rendering() {
        let value = OrderedJson::Array(vec![
            OrderedJson::number_f64(1.0),
            OrderedJson::number_f64(1e-6),
            OrderedJson::number_f64(1e20),
            OrderedJson::number_f64(1e21),
            OrderedJson::number_f64(-0.0),
        ]);
        assert_eq!(
            value.to_json_string().expect("serializable JSON"),
            "[1,0.000001,100000000000000000000,1e+21,0]"
        );
    }
}
