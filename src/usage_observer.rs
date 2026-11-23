//! Bounded, passive observation of upstream usage.  No response bytes are
//! changed: callers forward each original chunk after calling `observe`.
use std::{collections::BTreeMap, time::Instant};

use serde_json::Value;

const MAX_SSE_EVENT: usize = 256 * 1024;
const MAX_JSON_BODY: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ObservedUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_input_tokens: Option<i64>,
    pub cache_creation_input_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub total_nano_aiu: Option<i64>,
    pub first_token_ms: Option<i64>,
    pub protocol_error: bool,
    pub terminal: Option<StreamTerminal>,
    pub monthly_quota_exceeded: bool,
    /// Optional provider-reported nested token subcounts; may overlap totals.
    pub token_details: BTreeMap<String, i64>,
}

/// A protocol terminal event. Incomplete is distinct from success.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamTerminal {
    Completed,
    Incomplete,
}

#[derive(Clone, Copy)]
enum Protocol {
    Chat,
    Responses,
    Messages,
    Embeddings,
    Other,
}

pub struct UsageObserver {
    protocol: Protocol,
    is_sse: bool,
    started: Instant,
    usage: ObservedUsage,
    body: Vec<u8>,
    body_oversize: bool,
    line: Vec<u8>,
    line_oversize: bool,
    event_data: Vec<u8>,
    event_name: Vec<u8>,
    event_oversize: bool,
    last_was_cr: bool,
    saw_chat_payload: bool,
}

impl UsageObserver {
    pub fn new(endpoint: &str, is_sse: bool, started: Instant) -> Self {
        let protocol = match endpoint {
            "chat_completions" | "/chat/completions" | "/v1/chat/completions" => Protocol::Chat,
            "responses" | "/responses" | "/v1/responses" => Protocol::Responses,
            "messages" | "/v1/messages" => Protocol::Messages,
            "embeddings" | "/embeddings" | "/v1/embeddings" => Protocol::Embeddings,
            _ => Protocol::Other,
        };
        Self {
            protocol,
            is_sse,
            started,
            usage: ObservedUsage::default(),
            body: Vec::new(),
            body_oversize: false,
            line: Vec::new(),
            line_oversize: false,
            event_data: Vec::new(),
            event_name: Vec::new(),
            event_oversize: false,
            last_was_cr: false,
            saw_chat_payload: false,
        }
    }

    /// Inspect a chunk without storing the whole stream or modifying it.
    pub fn observe(&mut self, chunk: &[u8]) {
        if !self.is_sse {
            if !self.body_oversize {
                if self.body.len().saturating_add(chunk.len()) <= MAX_JSON_BODY {
                    self.body.extend_from_slice(chunk);
                } else {
                    self.body.clear();
                    self.body_oversize = true;
                }
            }
            return;
        }

        for &byte in chunk {
            if self.last_was_cr {
                self.last_was_cr = false;
                if byte == b'\n' {
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                self.process_line();
                self.last_was_cr = byte == b'\r';
            } else if !self.line_oversize {
                if self.line.len() < MAX_SSE_EVENT {
                    self.line.push(byte);
                } else {
                    self.line.clear();
                    self.line_oversize = true;
                    self.event_oversize = true;
                }
            }
        }
    }

    pub fn protocol_error(&self) -> bool {
        self.usage.protocol_error
    }

    /// Return the explicit protocol terminal state, if observed.
    pub fn terminal(&self) -> Option<StreamTerminal> {
        self.usage.terminal
    }

    pub fn monthly_quota_exceeded(&self) -> bool {
        self.usage.monthly_quota_exceeded
    }

    /// HTTP EOF alone does not establish generation completion for these protocols.
    pub fn expects_terminal(&self) -> bool {
        self.is_sse
            && matches!(
                self.protocol,
                Protocol::Chat | Protocol::Responses | Protocol::Messages
            )
    }

    /// Complete a non-streaming response or return observations from complete
    /// SSE events seen so far. Incomplete trailing SSE frames are ignored.
    pub fn finish(mut self) -> Option<ObservedUsage> {
        if !self.is_sse
            && !self.body_oversize
            && let Ok(value) = serde_json::from_slice::<Value>(&self.body)
        {
            self.observe_value(&value, None);
        }
        if self.usage.total_tokens.is_none()
            && let (Some(input), Some(output)) = (self.usage.input_tokens, self.usage.output_tokens)
        {
            self.usage.total_tokens = input
                .checked_add(output)
                .and_then(|n| n.checked_add(self.usage.cache_read_input_tokens.unwrap_or(0)))
                .and_then(|n| n.checked_add(self.usage.cache_creation_input_tokens.unwrap_or(0)));
        }
        let has_usage = [
            self.usage.input_tokens,
            self.usage.output_tokens,
            self.usage.cache_read_input_tokens,
            self.usage.cache_creation_input_tokens,
            self.usage.total_tokens,
            self.usage.total_nano_aiu,
        ]
        .into_iter()
        .flatten()
        .any(|value| value > 0)
            || self.usage.total_nano_aiu.is_some();
        (has_usage || self.usage.protocol_error).then_some(self.usage)
    }

    fn process_line(&mut self) {
        if self.line_oversize {
            self.line_oversize = false;
            return;
        }
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            self.dispatch_event();
            return;
        }
        if self.event_oversize || line[0] == b':' {
            return;
        }
        let (field, mut value) = match line.iter().position(|&b| b == b':') {
            Some(index) => (&line[..index], &line[index + 1..]),
            None => (&line[..], &[][..]),
        };
        if value.first() == Some(&b' ') {
            value = &value[1..];
        }
        match field {
            b"data" => {
                if self
                    .event_data
                    .len()
                    .saturating_add(value.len())
                    .saturating_add(1)
                    > MAX_SSE_EVENT
                {
                    self.event_oversize = true;
                    self.event_data.clear();
                } else {
                    self.event_data.extend_from_slice(value);
                    self.event_data.push(b'\n');
                }
            }
            b"event" => {
                self.event_name.clear();
                self.event_name.extend_from_slice(value);
            }
            _ => {}
        }
    }

    fn dispatch_event(&mut self) {
        if !self.event_oversize {
            let event_name = std::str::from_utf8(&self.event_name)
                .ok()
                .map(str::to_owned);
            if matches!(event_name.as_deref(), Some("error" | "response.failed")) {
                self.usage.protocol_error = true;
            }
            if matches!(self.protocol, Protocol::Chat)
                && self.saw_chat_payload
                && std::str::from_utf8(&self.event_data).is_ok_and(|data| data.trim() == "[DONE]")
            {
                self.usage.terminal = Some(StreamTerminal::Completed);
            }
            if let Ok(value) = serde_json::from_slice::<Value>(&self.event_data) {
                self.observe_value(&value, event_name.as_deref());
            }
        }
        self.event_data.clear();
        self.event_name.clear();
        self.event_oversize = false;
    }

    fn observe_value(&mut self, value: &Value, event_name: Option<&str>) {
        let event_type = value.get("type").and_then(Value::as_str).or(event_name);
        if self.is_sse
            && matches!(self.protocol, Protocol::Chat)
            && (value.get("choices").is_some_and(Value::is_array)
                || value.get("usage").is_some_and(Value::is_object)
                || value.get("copilot_usage").is_some_and(Value::is_object))
        {
            self.saw_chat_payload = true;
        }
        let current_error = matches!(event_type, Some("error" | "response.failed"))
            || value.get("error").is_some_and(|error| !error.is_null())
            || (matches!(self.protocol, Protocol::Responses)
                && value.pointer("/response/status").and_then(Value::as_str) == Some("failed"));
        if current_error {
            self.usage.protocol_error = true;
            let message = value
                .pointer("/error/message")
                .or_else(|| value.pointer("/response/error/message"))
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if message
                .to_ascii_lowercase()
                .contains("you have exceeded your monthly quota")
            {
                self.usage.monthly_quota_exceeded = true;
            }
        }
        if self.is_sse {
            match (self.protocol, event_type) {
                (Protocol::Responses, Some("response.incomplete"))
                    if value.get("response").is_some_and(Value::is_object) =>
                {
                    self.usage.terminal = Some(StreamTerminal::Incomplete);
                }
                (Protocol::Responses, Some("response.completed"))
                    if value.get("response").is_some_and(Value::is_object)
                        && value.pointer("/response/status").and_then(Value::as_str)
                            == Some("incomplete") =>
                {
                    self.usage.terminal = Some(StreamTerminal::Incomplete);
                }
                (Protocol::Responses, Some("response.completed"))
                    if value.get("response").is_some_and(Value::is_object)
                        && self.usage.terminal.is_none() =>
                {
                    self.usage.terminal = Some(StreamTerminal::Completed);
                }
                (Protocol::Messages, Some("message_stop"))
                    if value.get("type").and_then(Value::as_str) == Some("message_stop")
                        && self.usage.terminal.is_none() =>
                {
                    self.usage.terminal = Some(StreamTerminal::Completed);
                }
                _ => {}
            }
        }
        if self.is_sse
            && self.usage.first_token_ms.is_none()
            && first_token(self.protocol, value, event_type)
        {
            self.usage.first_token_ms =
                Some(i64::try_from(self.started.elapsed().as_millis()).unwrap_or(i64::MAX));
        }
        let container = match self.protocol {
            Protocol::Responses if self.is_sse => value.get("response").unwrap_or(&Value::Null),
            Protocol::Messages if event_type == Some("message_start") => {
                value.get("message").unwrap_or(&Value::Null)
            }
            _ => value,
        };
        if let Some(usage) = container.get("usage").filter(|v| v.is_object()) {
            crate::usage_details::update(&mut self.usage.token_details, usage);
            let before = (
                self.usage.input_tokens,
                self.usage.output_tokens,
                self.usage.cache_read_input_tokens,
                self.usage.cache_creation_input_tokens,
            );
            match self.protocol {
                Protocol::Chat => self.openai_usage(usage),
                Protocol::Responses => self.responses_usage(usage),
                Protocol::Messages => self.anthropic_usage(usage),
                Protocol::Embeddings => {
                    assign(&mut self.usage.input_tokens, number(usage, "prompt_tokens"));
                    assign(&mut self.usage.total_tokens, number(usage, "total_tokens"));
                }
                Protocol::Other => {}
            }
            let after = (
                self.usage.input_tokens,
                self.usage.output_tokens,
                self.usage.cache_read_input_tokens,
                self.usage.cache_creation_input_tokens,
            );
            // An older cumulative total cannot describe later updated counters.
            if before != after && number(usage, "total_tokens").is_none() {
                self.usage.total_tokens = None;
            }
        }
        let aiu = value
            .get("copilot_usage")
            .and_then(|u| number(u, "total_nano_aiu"))
            .or_else(|| {
                container
                    .get("copilot_usage")
                    .and_then(|u| number(u, "total_nano_aiu"))
            });
        assign(&mut self.usage.total_nano_aiu, aiu);
    }

    fn openai_usage(&mut self, usage: &Value) {
        let details = usage.get("prompt_tokens_details").unwrap_or(&Value::Null);
        let read = number(details, "cached_tokens");
        let write = number(details, "cache_creation_input_tokens");
        let has_split = usage.get("prompt_cache_hit_tokens").is_some()
            || usage.get("prompt_cache_miss_tokens").is_some();
        if has_split {
            assign(
                &mut self.usage.cache_read_input_tokens,
                number(usage, "prompt_cache_hit_tokens"),
            );
            assign(
                &mut self.usage.input_tokens,
                number(usage, "prompt_cache_miss_tokens"),
            );
        } else {
            assign(&mut self.usage.cache_read_input_tokens, read);
            assign(&mut self.usage.cache_creation_input_tokens, write);
            let input = number(usage, "prompt_tokens").map(|n| {
                n.saturating_sub(self.usage.cache_read_input_tokens.unwrap_or(0))
                    .saturating_sub(self.usage.cache_creation_input_tokens.unwrap_or(0))
                    .max(0)
            });
            assign(&mut self.usage.input_tokens, input);
        }
        assign(
            &mut self.usage.output_tokens,
            number(usage, "completion_tokens"),
        );
        assign(&mut self.usage.total_tokens, number(usage, "total_tokens"));
    }

    fn responses_usage(&mut self, usage: &Value) {
        let details = usage.get("input_tokens_details").unwrap_or(&Value::Null);
        let read = number(details, "cached_tokens");
        let write = number(details, "cache_write_tokens");
        assign(&mut self.usage.cache_read_input_tokens, read);
        assign(&mut self.usage.cache_creation_input_tokens, write);
        let input = number(usage, "input_tokens").map(|n| {
            n.saturating_sub(self.usage.cache_read_input_tokens.unwrap_or(0))
                .saturating_sub(self.usage.cache_creation_input_tokens.unwrap_or(0))
                .max(0)
        });
        assign(&mut self.usage.input_tokens, input);
        assign(
            &mut self.usage.output_tokens,
            number(usage, "output_tokens"),
        );
        assign(&mut self.usage.total_tokens, number(usage, "total_tokens"));
    }

    fn anthropic_usage(&mut self, usage: &Value) {
        assign(&mut self.usage.input_tokens, number(usage, "input_tokens"));
        assign(
            &mut self.usage.output_tokens,
            number(usage, "output_tokens"),
        );
        assign(
            &mut self.usage.cache_read_input_tokens,
            number(usage, "cache_read_input_tokens"),
        );
        assign(
            &mut self.usage.cache_creation_input_tokens,
            number(usage, "cache_creation_input_tokens"),
        );
        assign(&mut self.usage.total_tokens, number(usage, "total_tokens"));
    }
}

fn assign(target: &mut Option<i64>, value: Option<i64>) {
    if value.is_some() {
        *target = value;
    }
}

fn number(value: &Value, key: &str) -> Option<i64> {
    let value = value.get(key)?;
    if let Some(number) = value.as_i64() {
        return (number >= 0).then_some(number);
    }
    if let Some(number) = value.as_u64() {
        return i64::try_from(number).ok();
    }
    let number = value.as_f64()?;
    if number.is_finite() && number >= 0.0 && number < i64::MAX as f64 {
        Some(number.floor() as i64)
    } else {
        None
    }
}

fn nonempty(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

fn first_token(protocol: Protocol, value: &Value, event_type: Option<&str>) -> bool {
    match protocol {
        Protocol::Chat => value
            .get("choices")
            .and_then(Value::as_array)
            .is_some_and(|choices| {
                choices.iter().any(|choice| {
                    let delta = &choice["delta"];
                    [
                        "content",
                        "reasoning_text",
                        "reasoning_content",
                        "reasoning",
                    ]
                    .iter()
                    .any(|key| nonempty(delta.get(key)))
                        || delta
                            .get("tool_calls")
                            .and_then(Value::as_array)
                            .is_some_and(|calls| {
                                calls
                                    .iter()
                                    .any(|call| nonempty(call.pointer("/function/arguments")))
                            })
                })
            }),
        Protocol::Responses => match event_type {
            Some("response.output_item.added") => {
                value.pointer("/item/type").and_then(Value::as_str) == Some("reasoning")
            }
            Some("response.reasoning_summary_part.added") => true,
            Some(
                "response.output_text.delta"
                | "response.reasoning_summary_text.delta"
                | "response.function_call_arguments.delta"
                | "response.custom_tool_call_input.delta",
            ) => nonempty(value.get("delta")),
            _ => false,
        },
        Protocol::Messages => match event_type {
            Some("content_block_delta") => {
                match value.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => nonempty(value.pointer("/delta/text")),
                    Some("thinking_delta") => nonempty(value.pointer("/delta/thinking")),
                    Some("input_json_delta") => nonempty(value.pointer("/delta/partial_json")),
                    _ => false,
                }
            }
            Some("content_block_start") => {
                match value.pointer("/content_block/type").and_then(Value::as_str) {
                    Some("thinking") => true,
                    Some("text") => nonempty(value.pointer("/content_block/text")),
                    Some("tool_use" | "server_tool_use") => value
                        .pointer("/content_block/input")
                        .and_then(Value::as_object)
                        .is_some_and(|o| !o.is_empty()),
                    _ => false,
                }
            }
            _ => false,
        },
        Protocol::Embeddings | Protocol::Other => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_json_preserves_cache_split_and_exact_nano_aiu() {
        let mut observer = UsageObserver::new("chat_completions", false, Instant::now());
        observer.observe(br#"{"usage":{"prompt_tokens":100,"completion_tokens":20,"total_tokens":120,"prompt_tokens_details":{"cached_tokens":5,"cache_creation_input_tokens":5}},"copilot_usage":{"total_nano_aiu":9007199254740993}}"#);
        let actual = observer.finish().unwrap();
        assert_eq!(actual.input_tokens, Some(90));
        assert_eq!(actual.output_tokens, Some(20));
        assert_eq!(actual.cache_read_input_tokens, Some(5));
        assert_eq!(actual.cache_creation_input_tokens, Some(5));
        assert_eq!(actual.total_tokens, Some(120));
        assert_eq!(actual.total_nano_aiu, Some(9007199254740993));
        assert_eq!(actual.first_token_ms, None);
    }

    #[test]
    fn responses_sse_stream_is_incremental_and_preserves_partial_error() {
        let mut observer = UsageObserver::new("responses", true, Instant::now());
        let bytes = b"event: response.output_text.delta\r\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hey\"}\r\n\r\nevent: response.failed\r\ndata: {\"type\":\"response.failed\",\"response\":{\"usage\":{\"input_tokens\":10,\"input_tokens_details\":{\"cached_tokens\":3,\"cache_write_tokens\":2},\"output_tokens\":4,\"total_tokens\":14}},\"copilot_usage\":{\"total_nano_aiu\":9}}\r\n\r\n";
        for chunk in bytes.chunks(7) {
            observer.observe(chunk);
        }
        assert!(observer.protocol_error());
        let result = observer.finish().unwrap();
        assert_eq!(result.input_tokens, Some(5));
        assert_eq!(result.output_tokens, Some(4));
        assert_eq!(result.cache_read_input_tokens, Some(3));
        assert_eq!(result.cache_creation_input_tokens, Some(2));
        assert_eq!(result.total_tokens, Some(14));
        assert_eq!(result.total_nano_aiu, Some(9));
        assert!(result.first_token_ms.is_some());
        assert!(result.protocol_error);
    }

    #[test]
    fn anthropic_cumulative_updates_merge_without_double_counting() {
        let mut observer = UsageObserver::new("messages", true, Instant::now());
        observer.observe(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":3,\"cache_creation_input_tokens\":2,\"output_tokens\":0}}}\n\n");
        observer.observe(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":4},\"copilot_usage\":{\"total_nano_aiu\":17}}\n\n");
        // A client may disconnect here; previously complete events still count.
        let result = observer.finish().unwrap();
        assert_eq!(result.input_tokens, Some(10));
        assert_eq!(result.output_tokens, Some(4));
        assert_eq!(result.cache_read_input_tokens, Some(3));
        assert_eq!(result.cache_creation_input_tokens, Some(2));
        assert_eq!(result.total_tokens, Some(19));
        assert_eq!(result.total_nano_aiu, Some(17));
        assert_eq!(result.first_token_ms, None);
    }

    #[test]
    fn skips_oversize_event_then_resumes_without_buffering_stream() {
        let mut observer = UsageObserver::new("chat_completions", true, Instant::now());
        observer.observe(b"data: ");
        observer.observe(&vec![b'x'; MAX_SSE_EVENT + 1]);
        observer
            .observe(b"\n\ndata: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n");
        assert!(observer.line.len() <= MAX_SSE_EVENT);
        assert!(observer.event_data.len() <= MAX_SSE_EVENT);
        let result = observer.finish().unwrap();
        assert_eq!(result.input_tokens, Some(3));
        assert_eq!(result.output_tokens, Some(2));
        assert_eq!(result.total_tokens, Some(5));
    }

    #[test]
    fn incomplete_or_oversized_json_and_sse_do_not_invent_usage() {
        let mut observer = UsageObserver::new("chat_completions", true, Instant::now());
        observer.observe(b"data: {\"usage\":{\"prompt_tokens\":99}}\n");
        assert_eq!(observer.finish(), None);
        let mut observer = UsageObserver::new("chat_completions", false, Instant::now());
        observer.observe(&vec![b' '; MAX_JSON_BODY + 1]);
        observer.observe(br#"{"usage":{"prompt_tokens":99}}"#);
        assert_eq!(observer.finish(), None);
    }

    #[test]
    fn embeddings_and_split_openai_are_recorded() {
        let mut observer = UsageObserver::new("embeddings", false, Instant::now());
        observer.observe(br#"{"usage":{"prompt_tokens":7,"total_tokens":7}}"#);
        let result = observer.finish().unwrap();
        assert_eq!(result.input_tokens, Some(7));
        assert_eq!(result.output_tokens, None);
        assert_eq!(result.total_tokens, Some(7));
        let mut observer = UsageObserver::new("chat_completions", false, Instant::now());
        observer.observe(br#"{"usage":{"prompt_cache_miss_tokens":8,"prompt_cache_hit_tokens":4,"completion_tokens":2}}"#);
        let result = observer.finish().unwrap();
        assert_eq!(result.input_tokens, Some(8));
        assert_eq!(result.cache_read_input_tokens, Some(4));
        assert_eq!(result.total_tokens, Some(14));
    }

    #[test]
    fn ignores_invalid_numbers_and_reports_error_without_usage() {
        let mut observer = UsageObserver::new("messages", true, Instant::now());
        observer.observe(b"event: error\ndata: {\"type\":\"error\"}\n\n");
        assert!(observer.protocol_error());
        let observed = observer.finish().unwrap();
        assert!(observed.protocol_error);
        assert_eq!(observed.input_tokens, None);
        let mut observer = UsageObserver::new("responses", false, Instant::now());
        observer.observe(br#"{"usage":{"input_tokens":-3,"output_tokens":"42"}}"#);
        assert_eq!(observer.finish(), None);
        let mut observer = UsageObserver::new("chat_completions", false, Instant::now());
        observer.observe(br#"{"copilot_usage":{"total_nano_aiu":0}}"#);
        assert_eq!(observer.finish().unwrap().total_nano_aiu, Some(0));
    }

    #[test]
    fn quota_error_detection_does_not_match_normal_model_text() {
        let mut error = UsageObserver::new("responses", true, Instant::now());
        error.observe(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"You have exceeded your monthly quota\"}}\n\n");
        assert!(error.protocol_error());
        assert!(error.monthly_quota_exceeded());
        let mut answer = UsageObserver::new("chat_completions", true, Instant::now());
        answer.observe(b"data: {\"choices\":[{\"delta\":{\"content\":\"You have exceeded your monthly quota\"}}]}\n\n");
        assert!(!answer.monthly_quota_exceeded());
    }

    #[test]
    fn later_partial_update_replaces_stale_total_and_cache_is_clamped() {
        let mut observer = UsageObserver::new("chat_completions", true, Instant::now());
        observer.observe(b"data: {\"usage\":{\"prompt_tokens\":4,\"completion_tokens\":0,\"total_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":9}}}\n\n");
        observer.observe(b"data: {\"usage\":{\"completion_tokens\":3}}\n\n");
        let usage = observer.finish().unwrap();
        assert_eq!(usage.input_tokens, Some(0));
        assert_eq!(usage.output_tokens, Some(3));
        assert_eq!(usage.cache_read_input_tokens, Some(9));
        assert_eq!(usage.total_tokens, Some(12));
    }

    #[test]
    fn cumulative_updates_keep_prior_cache_when_details_are_omitted() {
        let mut chat = UsageObserver::new("chat_completions", true, Instant::now());
        chat.observe(b"data: {\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":2,\"prompt_tokens_details\":{\"cached_tokens\":10,\"cache_creation_input_tokens\":5}}}\n\n");
        chat.observe(b"data: {\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":4}}\n\n");
        let chat = chat.finish().unwrap();
        assert_eq!(chat.input_tokens, Some(105));
        assert_eq!(chat.cache_read_input_tokens, Some(10));
        assert_eq!(chat.cache_creation_input_tokens, Some(5));
        assert_eq!(chat.total_tokens, Some(124));
        let mut responses = UsageObserver::new("responses", true, Instant::now());
        responses.observe(b"data: {\"response\":{\"usage\":{\"input_tokens\":100,\"output_tokens\":2,\"input_tokens_details\":{\"cached_tokens\":10,\"cache_write_tokens\":5}}}}\n\n");
        responses.observe(
            b"data: {\"response\":{\"usage\":{\"input_tokens\":120,\"output_tokens\":4}}}\n\n",
        );
        let responses = responses.finish().unwrap();
        assert_eq!(responses.input_tokens, Some(105));
        assert_eq!(responses.cache_read_input_tokens, Some(10));
        assert_eq!(responses.cache_creation_input_tokens, Some(5));
        assert_eq!(responses.total_tokens, Some(124));
    }
}
