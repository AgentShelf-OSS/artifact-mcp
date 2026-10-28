//! Viewer subscription streams over push, poll, and SSE transports.
use super::{
    Binding, DataBroker, DataEnvelope, DataStream, MAX_BYTES, RequestedSubscription, SourceConfig,
    SubscriptionConfig,
    validate::{valid_event_text, valid_name},
};
use reqwest::Url;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    time::Duration,
};
use tokio::sync::mpsc;

impl DataBroker {
    pub async fn stream(
        &self,
        id: &str,
        topics: Vec<RequestedSubscription>,
        cursor: BTreeMap<String, String>,
    ) -> Result<DataStream, String> {
        if topics.is_empty() || topics.len() > 16 {
            return Err("bad_params".into());
        }
        let mut unique = HashSet::new();
        let mut selected = Vec::new();
        for topic in topics {
            if !valid_name(&topic.binding)
                || !valid_name(&topic.subscription)
                || !unique.insert(format!("{}:{}", topic.binding, topic.subscription))
            {
                return Err("bad_params".into());
            }
            let (binding, source) = self.binding_source(id, &topic.binding).await?;
            if !binding.subscriptions.contains(&topic.subscription) {
                return Err("not_found".into());
            }
            let sub = source
                .subscriptions
                .get(&topic.subscription)
                .ok_or("not_found")?
                .clone();
            selected.push((topic, binding, source, sub));
        }
        let permit = self
            .stream_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| "data_unavailable")?;
        let (tx, receiver) = mpsc::channel(32);
        let mut tasks = Vec::new();
        for (topic, binding, source, sub) in selected {
            let broker = self.clone();
            let tx = tx.clone();
            let id = id.to_owned();
            let previous = cursor
                .get(&format!("{}:{}", topic.binding, topic.subscription))
                .cloned();
            tasks.push(tokio::spawn(async move {
                broker
                    .run_subscription(id, topic, binding, source, sub, previous, tx)
                    .await;
            }));
        }
        Ok(DataStream {
            receiver,
            tasks,
            _permit: permit,
        })
    }
    pub(super) async fn current_binding(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        expected: &Binding,
    ) -> bool {
        let Ok((org, manifest)) = self.load_bindings(id).await else {
            return false;
        };
        let Some(current) = manifest.bindings.get(&topic.binding) else {
            return false;
        };
        self.source_record(&current.source)
            .await
            .is_ok_and(|(source, _, _, _)| {
                source.org == org
                    && current == expected
                    && current.subscriptions.contains(&topic.subscription)
            })
    }
    pub(super) async fn current_source(
        &self,
        id: &str,
        binding: &Binding,
        subscription: &str,
        source: &SourceConfig,
    ) -> Option<(SourceConfig, SubscriptionConfig)> {
        let current = self.source_record(&binding.source).await.ok()?;
        if current.0.org != self.load_bindings(id).await.ok()?.0 {
            return None;
        }
        if serde_json::to_vec(&current.0).ok() == serde_json::to_vec(source).ok() {
            return None;
        }
        Some((
            current.0.clone(),
            current.0.subscriptions.get(subscription).cloned()?,
        ))
    }
    #[allow(clippy::too_many_arguments)] // One resolved subscription and its cancellation channel.
    pub(super) async fn run_subscription(
        &self,
        id: String,
        topic: RequestedSubscription,
        binding: Binding,
        mut source: SourceConfig,
        mut sub: SubscriptionConfig,
        mut cursor: Option<String>,
        tx: mpsc::Sender<DataEnvelope>,
    ) {
        let mut backoff = 1;
        loop {
            if tx.is_closed() || !self.current_binding(&id, &topic, &binding).await {
                break;
            }
            if let Ok((_, enabled, _, _)) = self.source_record(&binding.source).await
                && !enabled
            {
                if emit(&tx, &topic, "data:status", "", json!({"state":"disabled"}))
                    .await
                    .is_err()
                {
                    break;
                }
                tokio::select! { _=tx.closed()=>break, _=tokio::time::sleep(Duration::from_secs(2))=>{} }
                continue;
            }
            if let Some((changed_source, changed_sub)) = self
                .current_source(&id, &binding, &topic.subscription, &source)
                .await
            {
                source = changed_source;
                sub = changed_sub;
                cursor = None;
                if emit(
                    &tx,
                    &topic,
                    "data:resync",
                    "",
                    json!({"reason":"source_changed"}),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
            let result = match sub.transport.as_str() {
                "push" => {
                    self.push_stream(&id, &topic, &binding, &mut cursor, &tx)
                        .await
                }
                "poll" => {
                    self.poll_stream(&id, &topic, &binding, &source, &sub, &tx)
                        .await
                }
                _ => {
                    self.sse_stream(&id, &topic, &binding, &source, &sub, &mut cursor, &tx)
                        .await
                }
            };
            if sub.transport == "sse"
                && let Err(error) = &result
            {
                self.record_event_health(&binding.source, Err(error)).await;
            }
            if tx.is_closed() || !self.current_binding(&id, &topic, &binding).await {
                break;
            }
            let state = if result.is_err() {
                "unavailable"
            } else {
                "reconnecting"
            };
            if emit(&tx, &topic, "data:status", "", json!({"state":state}))
                .await
                .is_err()
            {
                break;
            }
            tokio::select! { _=tx.closed()=>break, _=tokio::time::sleep(Duration::from_secs(backoff))=>{} }
            backoff = (backoff * 2).min(30);
        }
    }
    pub(super) async fn push_stream(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        binding: &Binding,
        cursor: &mut Option<String>,
        tx: &mpsc::Sender<DataEnvelope>,
    ) -> Result<(), String> {
        emit(tx, topic, "data:status", "", json!({"state":"connected"})).await?;
        loop {
            if !self.current_binding(id, topic, binding).await {
                return Ok(());
            }
            if !self
                .source_record(&binding.source)
                .await
                .is_ok_and(|(_, enabled, _, _)| enabled)
            {
                return Ok(());
            }
            let events = self
                .stored_events(id, &topic.binding, &topic.subscription)
                .await?;
            let start = if let Some(previous) = cursor.as_ref() {
                if let Some(pos) = events.iter().position(|e| &e.id == previous) {
                    pos + 1
                } else {
                    emit(tx, topic, "data:resync", "", json!({"reason":"gap"})).await?;
                    *cursor = None;
                    0
                }
            } else {
                0
            };
            for e in events.into_iter().skip(start) {
                self.record_event_health(&binding.source, Ok(())).await;
                emit(tx, topic, &e.event, &e.id, e.data).await?;
                *cursor = Some(e.id);
            }
            tokio::select! { _=tx.closed()=>return Ok(()), _=tokio::time::sleep(Duration::from_millis(250))=>{} }
        }
    }
    pub(super) async fn poll_stream(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        binding: &Binding,
        source: &SourceConfig,
        sub: &SubscriptionConfig,
        tx: &mpsc::Sender<DataEnvelope>,
    ) -> Result<(), String> {
        let operation = source
            .operations
            .get(sub.operation.as_deref().ok_or("data_unavailable")?)
            .ok_or("data_unavailable")?;
        loop {
            if !self.current_binding(id, topic, binding).await {
                return Ok(());
            }
            if !self
                .source_record(&binding.source)
                .await
                .is_ok_and(|(_, enabled, _, _)| enabled)
            {
                return Ok(());
            }
            let result = self.http_query(source, operation, &json!({})).await;
            self.record_query_health(
                &binding.source,
                result.as_ref().map(|_| ()).map_err(|error| error.as_str()),
            )
            .await;
            let value = result?;
            emit(tx, topic, "data:status", "", json!({"state":"connected"})).await?;
            self.record_event_health(&binding.source, Ok(())).await;
            emit(tx, topic, "message", "", value).await?;
            tokio::select! { _=tx.closed()=>return Ok(()), _=tokio::time::sleep(Duration::from_millis(sub.interval_ms))=>{} }
        }
    }
    #[allow(clippy::too_many_arguments)] // The connector needs its binding for revocation checks.
    pub(super) async fn sse_stream(
        &self,
        id: &str,
        topic: &RequestedSubscription,
        binding: &Binding,
        source: &SourceConfig,
        sub: &SubscriptionConfig,
        cursor: &mut Option<String>,
        tx: &mpsc::Sender<DataEnvelope>,
    ) -> Result<(), String> {
        let mut url = Url::parse(source.base_url.as_deref().ok_or("data_unavailable")?)
            .map_err(|_| "data_unavailable")?;
        url.set_path(&format!(
            "{}{}",
            url.path().trim_end_matches('/'),
            sub.path.as_deref().ok_or("data_unavailable")?
        ));
        let mut request = self
            .client
            .get(url)
            .headers(Self::resolved_headers(source)?)
            .header("accept", "text/event-stream");
        if let Some(previous) = cursor.as_ref() {
            request = request.header("last-event-id", previous);
        }
        let mut response = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .map_err(|_| "data_unavailable")?
            .map_err(|_| "data_unavailable")?;
        if !response.status().is_success()
            || !response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| s.starts_with("text/event-stream"))
        {
            return Err("data_unavailable".into());
        }
        emit(tx, topic, "data:status", "", json!({"state":"connected"})).await?;
        let mut activity = tokio::time::Instant::now();
        let mut pending = Vec::new();
        let mut event = String::new();
        let mut data = String::new();
        let mut event_id = String::new();
        let mut frame_bytes = 0;
        let mut check = tokio::time::interval(Duration::from_secs(2));
        loop {
            let chunk = tokio::select! {
                _=tx.closed()=>return Ok(()),
                _=check.tick()=>{if !self.current_binding(id,topic,binding).await || !self.source_record(&binding.source).await.is_ok_and(|(_, enabled, _, _)| enabled) {return Ok(())}
                    if self.current_source(id,binding,&topic.subscription,source).await.is_some() {return Ok(())}
                    if activity.elapsed()>Duration::from_secs(45){return Err("data_unavailable".into())}continue;},
                chunk=response.chunk()=>chunk.map_err(|_|"data_unavailable")?,
            };
            let Some(chunk) = chunk else { return Ok(()) };
            activity = tokio::time::Instant::now();
            if pending.len() + chunk.len() > MAX_BYTES {
                return Err("too_large".into());
            }
            pending.extend_from_slice(&chunk);
            while let Some(pos) = pending.iter().position(|b| *b == b'\n') {
                let bytes: Vec<u8> = pending.drain(..=pos).collect();
                frame_bytes += bytes.len();
                if frame_bytes > MAX_BYTES {
                    return Err("too_large".into());
                }
                let line = std::str::from_utf8(&bytes[..pos])
                    .map_err(|_| "data_unavailable")?
                    .trim_end_matches('\r');
                if line.is_empty() {
                    let kind = if event.is_empty() { "message" } else { &event };
                    if !data.is_empty()
                        && (sub.events.is_empty() && kind == "message"
                            || sub.events.iter().any(|s| s == kind))
                    {
                        let text = data.strip_suffix('\n').unwrap_or(&data);
                        let value = serde_json::from_str(text)
                            .unwrap_or_else(|_| Value::String(text.to_owned()));
                        self.record_event_health(&binding.source, Ok(())).await;
                        emit(tx, topic, kind, &event_id, value).await?;
                        if !event_id.is_empty() {
                            *cursor = Some(event_id.clone());
                        }
                    }
                    event.clear();
                    data.clear();
                    event_id.clear();
                    frame_bytes = 0;
                } else if !line.starts_with(':') {
                    let (field, value) = line.split_once(':').unwrap_or((line, ""));
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "event" => event = value.into(),
                        "data" => {
                            data.push_str(value);
                            data.push('\n');
                        }
                        "id" if valid_event_text(value) => event_id = value.into(),
                        _ => {}
                    }
                }
            }
        }
    }
}
pub(super) async fn emit(
    tx: &mpsc::Sender<DataEnvelope>,
    topic: &RequestedSubscription,
    event: &str,
    id: &str,
    data: Value,
) -> Result<(), String> {
    tx.send(DataEnvelope {
        binding: topic.binding.clone(),
        subscription: topic.subscription.clone(),
        event: event.into(),
        id: id.into(),
        data,
    })
    .await
    .map_err(|_| "data_unavailable".into())
}
