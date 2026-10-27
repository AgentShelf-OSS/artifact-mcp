//! Outbound HTTP proxy for operations, NDJSON relay, and source health recording.
use super::{DataBroker, OperationConfig, STREAM_IDLE, SourceConfig, validate::build_request};
use crate::{error::AppError, persistence::db::interact};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::time::Duration;

impl DataBroker {
    pub async fn query(
        &self,
        id: &str,
        binding: &str,
        operation: &str,
        params: Value,
    ) -> Result<Value, String> {
        let (b, s) = self.binding_source(id, binding).await?;
        if !b.operations.iter().any(|n| n == operation) {
            return Err("not_found".into());
        }
        let o = s.operations.get(operation).ok_or("not_found")?;
        if s.kind == "push" {
            if !params.as_object().is_some_and(|p| p.is_empty()) {
                return Err("bad_params".into());
            }
            let key = o.key.as_deref().unwrap_or(operation);
            if let Some(pool) = &self.pool {
                let a = id.to_owned();
                let b = binding.to_owned();
                let k = key.to_owned();
                let raw=interact(pool,move|c|c.query_row("SELECT value FROM artifact_data_snapshots WHERE artifact_id=?1 AND binding=?2 AND key=?3",(&a,&b,&k),|r|r.get::<_,String>(0)).optional().map_err(|_|AppError::Internal)).await.map_err(|_|"data_unavailable")?.ok_or("not_found")?;
                return serde_json::from_str(&raw).map_err(|_| "data_unavailable".into());
            }
            return self
                .memory
                .read()
                .await
                .snapshots
                .get(&(id.into(), binding.into(), key.into()))
                .map(|(_, v)| v.clone())
                .ok_or("not_found".into());
        }
        let result = self.http_query(&s, o, &params).await;
        self.record_query_health(&s.id, result.as_ref().map(|_| ()).map_err(|e| e.as_str()))
            .await;
        result
    }
    /// Sends the operation's request and checks the status (and the content type of a stream).
    pub(super) async fn open_upstream(
        &self,
        s: &SourceConfig,
        o: &OperationConfig,
        params: &Value,
    ) -> Result<reqwest::Response, String> {
        let (url, body) = build_request(s, o, params)?;
        let request = match body {
            Some(body) => self.client.post(url).json(&body),
            None => self.client.get(url),
        };
        let response = request
            .headers(Self::resolved_headers(s)?)
            .timeout(Duration::from_millis(o.timeout_ms))
            .send()
            .await
            .map_err(|_| "data_unavailable")?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err("not_found".into());
        }
        if !response.status().is_success() {
            return Err("data_unavailable".into());
        }
        if o.stream
            && !response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|t| t.trim().eq_ignore_ascii_case("application/x-ndjson"))
                })
        {
            return Err("data_unavailable".into());
        }
        Ok(response)
    }
    /// ADR-0017: relays a streaming operation. Errors before the upstream answers are returned
    /// directly; later ones arrive as the channel's last item. Dropping the receiver cancels the
    /// upstream request.
    pub async fn query_stream(
        &self,
        id: &str,
        binding: &str,
        operation: &str,
        params: Value,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<Value, String>>, String> {
        let (b, s) = self.binding_source(id, binding).await?;
        if !b.operations.iter().any(|n| n == operation) {
            return Err("not_found".into());
        }
        let o = s.operations.get(operation).ok_or("not_found")?.clone();
        if s.kind != "http" || !o.stream {
            return Err("bad_params".into());
        }
        let opened = self.open_upstream(&s, &o, &params).await;
        self.record_query_health(&s.id, opened.as_ref().map(|_| ()).map_err(|e| e.as_str()))
            .await;
        let mut response = opened?;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(o.timeout_ms);
        tokio::spawn(async move {
            let mut lines = NdjsonLines::new(o.max_bytes);
            loop {
                let wait = tokio::time::timeout_at(
                    deadline.min(tokio::time::Instant::now() + STREAM_IDLE),
                    response.chunk(),
                )
                .await;
                let chunk = match wait {
                    Ok(Ok(Some(chunk))) => chunk,
                    Ok(Ok(None)) => break,
                    _ => {
                        let _ = tx.send(Err("data_unavailable".into())).await;
                        return;
                    }
                };
                match lines.push(&chunk) {
                    Ok(values) => {
                        for value in values {
                            if tx.send(Ok(value)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(reason) => {
                        let _ = tx.send(Err(reason)).await;
                        return;
                    }
                }
            }
            match lines.finish() {
                Ok(Some(value)) => {
                    let _ = tx.send(Ok(value)).await;
                }
                Ok(None) => {}
                Err(reason) => {
                    let _ = tx.send(Err(reason)).await;
                }
            }
        });
        Ok(rx)
    }
    pub(super) async fn record_query_health(&self, id: &str, result: Result<(), &str>) {
        let mut health = self.health.write().await;
        let entry = health.entry(id.to_owned()).or_default();
        entry.last_query_at = Some(
            time::OffsetDateTime::now_utc()
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| "".into()),
        );
        match result {
            Ok(()) => {
                entry.state = "connected".into();
                entry.last_success_at = entry.last_query_at.clone();
                entry.error = None;
                entry.retry_count = 0;
            }
            Err(error) => {
                entry.state = "unavailable".into();
                entry.error = Some(
                    match error {
                        "too_large" => "too_large",
                        _ => "data_unavailable",
                    }
                    .into(),
                );
                entry.retry_count = entry.retry_count.saturating_add(1);
            }
        }
    }
    pub(super) async fn record_event_health(&self, id: &str, result: Result<(), &str>) {
        let mut health = self.health.write().await;
        let entry = health.entry(id.to_owned()).or_default();
        let now = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        match result {
            Ok(()) => {
                entry.state = "connected".into();
                entry.last_event_at = Some(now.clone());
                entry.last_success_at = Some(now);
                entry.error = None;
                entry.retry_count = 0;
            }
            Err(error) => {
                entry.state = "unavailable".into();
                entry.error = Some(if error == "too_large" {
                    "too_large".into()
                } else {
                    "data_unavailable".into()
                });
                entry.retry_count = entry.retry_count.saturating_add(1);
            }
        }
    }
    pub(super) async fn http_query(
        &self,
        s: &SourceConfig,
        o: &OperationConfig,
        params: &Value,
    ) -> Result<Value, String> {
        let mut response = self.open_upstream(s, o, params).await?;
        if o.stream {
            // Older clients use /data/query for a streaming operation; they receive its last line.
            let mut lines = NdjsonLines::new(o.max_bytes);
            let mut last = None;
            while let Some(chunk) = response.chunk().await.map_err(|_| "data_unavailable")? {
                for value in lines.push(&chunk)? {
                    last = Some(value);
                }
            }
            if let Some(value) = lines.finish()? {
                last = Some(value);
            }
            return last.ok_or_else(|| "data_unavailable".into());
        }
        if response
            .content_length()
            .is_some_and(|n| n > o.max_bytes as u64)
        {
            return Err("too_large".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "data_unavailable")? {
            if bytes.len().saturating_add(chunk.len()) > o.max_bytes {
                return Err("too_large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| "data_unavailable".into())
    }
}
/// Splits newline-delimited JSON across arbitrary chunk boundaries and bounds the total size.
pub(crate) struct NdjsonLines {
    pending: Vec<u8>,
    total: usize,
    max_bytes: usize,
}
impl NdjsonLines {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            pending: Vec::new(),
            total: 0,
            max_bytes,
        }
    }
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Result<Vec<Value>, String> {
        self.total = self.total.saturating_add(chunk.len());
        if self.total > self.max_bytes {
            return Err("too_large".into());
        }
        self.pending.extend_from_slice(chunk);
        let mut values = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            if let Some(value) = Self::parse(&line[..line.len() - 1])? {
                values.push(value);
            }
        }
        Ok(values)
    }
    pub(crate) fn finish(&mut self) -> Result<Option<Value>, String> {
        let rest = std::mem::take(&mut self.pending);
        Self::parse(&rest)
    }
    fn parse(line: &[u8]) -> Result<Option<Value>, String> {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        serde_json::from_slice(line)
            .map(Some)
            .map_err(|_| "data_unavailable".into())
    }
}
