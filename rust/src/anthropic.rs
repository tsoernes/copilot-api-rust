//! Explicit, fallible translations between Anthropic Messages and Chat Completions.
//! Native Messages forwarding should be preferred where available.
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing or invalid {key}"))
}

fn content_parts(content: &Value) -> Result<Value, String> {
    if content.is_string() {
        return Ok(content.clone());
    }
    let blocks = content
        .as_array()
        .ok_or("content must be a string or array")?;
    let mut out = Vec::new();
    for block in blocks {
        match string(block, "type")? {
            "text" => out.push(json!({"type":"text","text": string(block, "text")?})),
            "image" => {
                let source = block.get("source").ok_or("image has no source")?;
                let url = match string(source, "type")? {
                    "base64" => format!(
                        "data:{};base64,{}",
                        string(source, "media_type")?,
                        string(source, "data")?
                    ),
                    "url" => string(source, "url")?.to_owned(),
                    other => return Err(format!("unsupported image source {other}")),
                };
                out.push(json!({"type":"image_url","image_url":{"url":url}}));
            }
            other => return Err(format!("unsupported content block {other}")),
        }
    }
    Ok(json!(out))
}

/// Translate the common text, image, and function-tool Messages subset to Chat.
/// Unsupported content is rejected rather than silently discarded.
pub fn request_to_chat(payload: Value) -> Result<Value, String> {
    let mut out = Map::new();
    let supported = [
        "model",
        "max_tokens",
        "stop_sequences",
        "stream",
        "temperature",
        "top_p",
        "metadata",
        "system",
        "messages",
        "tools",
        "tool_choice",
    ];
    for (name, value) in payload.as_object().ok_or("request must be an object")? {
        if !supported.contains(&name.as_str()) && !value.is_null() {
            return Err(format!("unsupported request field {name}"));
        }
    }
    out.insert("model".into(), json!(string(&payload, "model")?));
    for (from, to) in [
        ("max_tokens", "max_completion_tokens"),
        ("stop_sequences", "stop"),
        ("stream", "stream"),
        ("temperature", "temperature"),
        ("top_p", "top_p"),
    ] {
        if let Some(value) = payload.get(from) {
            out.insert(to.into(), value.clone());
        }
    }
    if let Some(user) = payload.pointer("/metadata/user_id") {
        out.insert("user".into(), user.clone());
    }
    let mut messages = Vec::new();
    if let Some(system) = payload.get("system") {
        let system = if let Some(text) = system.as_str() {
            text.to_owned()
        } else {
            system
                .as_array()
                .ok_or("invalid system")?
                .iter()
                .map(|b| {
                    if string(b, "type")? != "text" {
                        return Err("unsupported system block".into());
                    }
                    Ok(string(b, "text")?.to_owned())
                })
                .collect::<Result<Vec<String>, String>>()?
                .join("\n\n")
        };
        messages.push(json!({"role":"system", "content":system}));
    }
    for message in payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("messages must be an array")?
    {
        let role = string(message, "role")?;
        let content = message.get("content").ok_or("message missing content")?;
        if role != "user" && role != "assistant" {
            return Err(format!("unsupported role {role}"));
        }
        if let Some(blocks) = content.as_array() {
            if role == "user" {
                let mut ordinary = Vec::new();
                for block in blocks {
                    if string(block, "type")? == "tool_result" {
                        let content = block.get("content").cloned().unwrap_or(json!(""));
                        let content = if let Some(parts) = content.as_array() {
                            let mut text = Vec::new();
                            for part in parts {
                                if string(part, "type")? != "text" {
                                    return Err("rich tool results are not supported".into());
                                }
                                text.push(string(part, "text")?);
                            }
                            json!(text.join("\n"))
                        } else if content.is_string() {
                            content
                        } else {
                            return Err("invalid tool result content".into());
                        };
                        messages.push(json!({"role":"tool","tool_call_id":string(block,"tool_use_id")?,"content":content}));
                    } else {
                        ordinary.push(block.clone());
                    }
                }
                if !ordinary.is_empty() {
                    messages
                        .push(json!({"role":"user","content":content_parts(&json!(ordinary))?}));
                }
            } else {
                let mut ordinary = Vec::new();
                let mut calls = Vec::new();
                let mut thought = Vec::new();
                let mut signature = None;
                for block in blocks {
                    match string(block,"type")? {
                        "tool_use" => calls.push(json!({"id":string(block,"id")?,"type":"function","function":{"name":string(block,"name")?,"arguments":serde_json::to_string(block.get("input").ok_or("tool use missing input")?).map_err(|e| e.to_string())?}})),
                        "thinking" => {
                            let text = string(block, "thinking")?;
                            if !text.is_empty() { thought.push(text.to_owned()); }
                            if let Some(s) = block.get("signature").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                                signature = Some(s.to_owned());
                            }
                        },
                        _ => ordinary.push(block.clone()),
                    }
                }
                let parts = content_parts(&json!(ordinary))?;
                let mut m = Map::new();
                m.insert("role".into(), json!("assistant"));
                let text = parts
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.get("text").and_then(Value::as_str))
                    .collect::<Option<Vec<_>>>();
                m.insert(
                    "content".into(),
                    if let Some(text) = text {
                        json!(text.join(""))
                    } else {
                        parts
                    },
                );
                if !calls.is_empty() {
                    m.insert("tool_calls".into(), json!(calls));
                }
                if !thought.is_empty() {
                    m.insert("reasoning_text".into(), json!(thought.join("\n\n")));
                }
                if let Some(s) = signature {
                    m.insert("reasoning_opaque".into(), json!(s));
                }
                messages.push(Value::Object(m));
            }
        } else {
            messages.push(json!({"role":role,"content":content_parts(content)?}));
        }
    }
    out.insert("messages".into(), json!(messages));
    if let Some(tools) = payload.get("tools") {
        let mut converted = Vec::new();
        for tool in tools.as_array().ok_or("tools must be an array")? {
            if tool.get("type").is_some() {
                return Err("server tools are not supported".into());
            }
            let mut schema = tool
                .get("input_schema")
                .cloned()
                .ok_or("tool missing input_schema")?;
            if schema.get("type").and_then(Value::as_str) == Some("object")
                && schema.get("properties").is_none()
            {
                schema
                    .as_object_mut()
                    .unwrap()
                    .insert("properties".into(), json!({}));
            }
            let mut function = Map::new();
            function.insert("name".into(), json!(string(tool, "name")?));
            function.insert("parameters".into(), schema);
            for k in ["description", "strict"] {
                if let Some(v) = tool.get(k) {
                    function.insert(k.into(), v.clone());
                }
            }
            converted.push(json!({"type":"function","function":function}));
        }
        if !converted.is_empty()
            && let Some(choice) = payload.get("tool_choice")
        {
            let c = match string(choice, "type")? {
                "auto" => json!("auto"),
                "any" => json!("required"),
                "none" => json!("none"),
                "tool" => json!({"type":"function","function":{"name":string(choice,"name")?}}),
                other => return Err(format!("unsupported tool choice {other}")),
            };
            out.insert("tool_choice".into(), c);
            if let Some(v) = choice.get("disable_parallel_tool_use") {
                out.insert(
                    "parallel_tool_calls".into(),
                    json!(!v.as_bool().ok_or("invalid disable_parallel_tool_use")?),
                );
            }
        }
        out.insert("tools".into(), json!(converted));
    }
    Ok(Value::Object(out))
}

fn usage(v: Option<&Value>) -> Value {
    let get = |p: &str| {
        v.and_then(|v| v.pointer(p))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let prompt = get("/prompt_tokens");
    let cached = get("/prompt_tokens_details/cached_tokens");
    let created = get("/prompt_tokens_details/cache_creation_input_tokens");
    let mut result = json!({"input_tokens":prompt.saturating_sub(cached).saturating_sub(created),"output_tokens":get("/completion_tokens")});
    if v.and_then(|v| v.pointer("/prompt_tokens_details/cached_tokens"))
        .is_some()
    {
        result["cache_read_input_tokens"] = json!(cached);
    }
    if v.and_then(|v| v.pointer("/prompt_tokens_details/cache_creation_input_tokens"))
        .is_some()
    {
        result["cache_creation_input_tokens"] = json!(created);
    }
    result
}
fn stop_reason(reason: &str) -> Result<&'static str, String> {
    match reason {
        "stop" | "content_filter" => Ok("end_turn"),
        "length" => Ok("max_tokens"),
        "tool_calls" => Ok("tool_use"),
        other => Err(format!("unsupported finish reason {other}")),
    }
}
/// Translate a non-streaming Chat response to an Anthropic message.
pub fn response_from_chat(response: Value) -> Result<Value, String> {
    let mut blocks = Vec::new();
    let mut stop = Value::Null;
    for choice in response
        .get("choices")
        .and_then(Value::as_array)
        .ok_or("missing choices")?
    {
        let m = choice.get("message").ok_or("missing message")?;
        let thought = m
            .get("reasoning_text")
            .or_else(|| m.get("reasoning_content"))
            .or_else(|| m.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let signature = m
            .get("reasoning_opaque")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !thought.is_empty() || !signature.is_empty() {
            blocks.push(json!({"type":"thinking","thinking":thought,"signature":signature}));
        }
        if let Some(content) = m.get("content") {
            if let Some(s) = content.as_str() {
                if !s.is_empty() {
                    blocks.push(json!({"type":"text","text":s}));
                }
            } else if let Some(parts) = content.as_array() {
                for part in parts {
                    if string(part, "type")? != "text" {
                        return Err("unsupported response content".into());
                    }
                    blocks.push(json!({"type":"text","text":string(part,"text")?}));
                }
            } else if !content.is_null() {
                return Err("invalid response content".into());
            }
        }
        if let Some(calls) = m.get("tool_calls") {
            for call in calls.as_array().ok_or("invalid tool calls")? {
                if string(call, "type")? != "function" {
                    return Err("unsupported tool call".into());
                }
                let f = call.get("function").ok_or("missing tool function")?;
                let input: Value = serde_json::from_str(string(f, "arguments")?)
                    .map_err(|e| format!("invalid tool arguments: {e}"))?;
                blocks.push(json!({"type":"tool_use","id":string(call,"id")?,"name":string(f,"name")?,"input":input}));
            }
        }
        if let Some(r) = choice.get("finish_reason").and_then(Value::as_str)
            && (stop.is_null() || r == "tool_calls")
        {
            stop = json!(stop_reason(r)?);
        }
    }
    Ok(
        json!({"id":string(&response,"id")?,"type":"message","role":"assistant","model":string(&response,"model")?,"content":blocks,"stop_reason":stop,"stop_sequence":null,"usage":usage(response.get("usage"))}),
    )
}

#[derive(Default)]
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
}
const MAX_BUFFERED_TOOL_BYTES: usize = 1024 * 1024;
const MAX_TOOL_CALLS: usize = 128;
/// Stateful conversion of JSON Chat Completion chunks to Anthropic event JSON.
/// `push` accepts decoded SSE data, and `finish` flushes a pending terminal event.
/// Interleaved tool calls are buffered with fixed limits and emitted at completion.
#[derive(Default)]
pub struct ChatStreamTranslator {
    started: bool,
    finished: bool,
    pending_stop: Option<String>,
    index: usize,
    open: Option<&'static str>,
    tool_calls: BTreeMap<u64, ToolCall>,
    buffered: usize,
    last_usage: Option<Value>,
}
impl ChatStreamTranslator {
    fn close(&mut self, events: &mut Vec<Value>) {
        if self.open.take().is_some() {
            events.push(json!({"type":"content_block_stop","index":self.index}));
            self.index += 1;
        }
    }
    fn open_block(&mut self, kind: &'static str, events: &mut Vec<Value>) {
        if self.open != Some(kind) {
            self.close(events);
            let block = if kind == "text" {
                json!({"type":"text","text":""})
            } else {
                json!({"type":"thinking","thinking":"","signature":""})
            };
            events.push(
                json!({"type":"content_block_start","index":self.index,"content_block":block}),
            );
            self.open = Some(kind);
        }
    }
    fn delta(&mut self, kind: &'static str, field: &str, value: &str, events: &mut Vec<Value>) {
        if value.is_empty() {
            return;
        }
        self.open_block(kind, events);
        let mut delta = Map::new();
        delta.insert("type".into(), json!(format!("{field}_delta")));
        delta.insert(field.into(), json!(value));
        events.push(json!({"type":"content_block_delta","index":self.index,"delta":delta}));
    }
    fn complete(&mut self, events: &mut Vec<Value>) {
        if let Some(reason) = self.pending_stop.take() {
            events.push(json!({"type":"message_delta","delta":{"stop_reason":reason,"stop_sequence":null},"usage":self.last_usage.clone().unwrap_or_else(|| usage(None))}));
            events.push(json!({"type":"message_stop"}));
            self.finished = true;
        }
    }
    pub fn push(&mut self, chunk: &Value) -> Result<Vec<Value>, String> {
        if self.finished {
            return Err("chunk received after completion".into());
        }
        let mut events = Vec::new();
        if let Some(u) = chunk.get("usage").filter(|u| !u.is_null()) {
            self.last_usage = Some(usage(Some(u)));
        }
        let choices = chunk
            .get("choices")
            .and_then(Value::as_array)
            .ok_or("chunk missing choices")?;
        if choices.is_empty() {
            if self.last_usage.is_some() {
                self.complete(&mut events);
            }
            return Ok(events);
        }
        if self.pending_stop.is_some() {
            return Err("content after finish reason".into());
        }
        if choices.len() != 1 {
            return Err("multiple streaming choices are unsupported".into());
        }
        let choice = &choices[0];
        let delta = choice.get("delta").ok_or("chunk missing delta")?;
        if !self.started {
            let mut initial_usage = self.last_usage.clone().unwrap_or_else(|| usage(None));
            initial_usage["output_tokens"] = json!(0);
            events.push(json!({"type":"message_start","message":{"id":string(chunk,"id")?,"type":"message","role":"assistant","content":[],"model":string(chunk,"model")?,"stop_reason":null,"stop_sequence":null,"usage":initial_usage}}));
            self.started = true;
        }
        let thought = delta
            .get("reasoning_text")
            .or_else(|| delta.get("reasoning_content"))
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let content = delta.get("content").and_then(Value::as_str).unwrap_or("");
        let signature = delta
            .get("reasoning_opaque")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !self.tool_calls.is_empty()
            && (!thought.is_empty() || !content.is_empty() || !signature.is_empty())
        {
            return Err("content interleaved after tool calls is unsupported".into());
        }
        self.delta("thinking", "thinking", thought, &mut events);
        if !signature.is_empty() {
            self.open_block("thinking", &mut events);
            events.push(json!({"type":"content_block_delta","index":self.index,"delta":{"type":"signature_delta","signature":signature}}));
        }
        self.delta("text", "text", content, &mut events);
        if let Some(calls) = delta.get("tool_calls") {
            let calls = calls.as_array().ok_or("invalid streaming tool calls")?;
            if !calls.is_empty() {
                self.close(&mut events);
            }
            for call in calls {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .ok_or("tool call missing index")?;
                if !self.tool_calls.contains_key(&index) && self.tool_calls.len() >= MAX_TOOL_CALLS
                {
                    return Err("too many tool calls".into());
                }
                let item = self.tool_calls.entry(index).or_default();
                let id = call.get("id").and_then(Value::as_str).unwrap_or("");
                let name = call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                self.buffered = self
                    .buffered
                    .checked_add(id.len() + name.len() + args.len())
                    .ok_or("tool call buffer overflow")?;
                if self.buffered > MAX_BUFFERED_TOOL_BYTES {
                    return Err("tool call buffer limit exceeded".into());
                }
                item.id.push_str(id);
                item.name.push_str(name);
                item.arguments.push_str(args);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            let reason = stop_reason(reason)?.to_owned();
            self.close(&mut events);
            for call in self.tool_calls.values() {
                if call.id.is_empty() || call.name.is_empty() {
                    return Err("incomplete streamed tool call".into());
                }
                // Reject malformed completed JSON instead of emitting a corrupted tool request.
                let _: Value = serde_json::from_str(&call.arguments)
                    .map_err(|e| format!("invalid streamed tool arguments: {e}"))?;
                events.push(json!({"type":"content_block_start","index":self.index,"content_block":{"type":"tool_use","id":call.id,"name":call.name,"input":{}}}));
                events.push(json!({"type":"content_block_delta","index":self.index,"delta":{"type":"input_json_delta","partial_json":call.arguments}}));
                events.push(json!({"type":"content_block_stop","index":self.index}));
                self.index += 1;
            }
            self.tool_calls.clear();
            self.buffered = 0;
            self.pending_stop = Some(reason);
            if chunk.get("usage").is_some_and(|v| !v.is_null()) {
                self.complete(&mut events);
            }
        }
        Ok(events)
    }
    /// Call on the upstream `[DONE]`; an unfinished stream is an error.
    pub fn finish(&mut self) -> Result<Vec<Value>, String> {
        if self.finished {
            return Ok(Vec::new());
        }
        if self.pending_stop.is_none() {
            return Err("upstream stream ended without a finish reason".into());
        }
        let mut events = Vec::new();
        self.complete(&mut events);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_tools_images_and_results() {
        let result = request_to_chat(json!({"model":"m","system":[{"type":"text","text":"sys"}],"max_tokens":20,"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"t","name":"f","input":{"a":1}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"yes"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAA"}}]}],"tools":[{"name":"f","input_schema":{"type":"object"}}],"tool_choice":{"type":"any","disable_parallel_tool_use":true}})).unwrap();
        assert_eq!(result["max_completion_tokens"], 20);
        assert_eq!(result["messages"][0]["role"], "system");
        assert_eq!(
            result["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert_eq!(result["messages"][2]["role"], "tool");
        assert_eq!(
            result["messages"][3]["content"][0]["image_url"]["url"],
            "data:image/png;base64,AAA"
        );
        assert_eq!(
            result["tools"][0]["function"]["parameters"]["properties"],
            json!({})
        );
        assert_eq!(result["tool_choice"], "required");
        assert_eq!(result["parallel_tool_calls"], false);
    }
    #[test]
    fn rejects_unsupported_request() {
        assert!(
            request_to_chat(
                json!({"model":"m","messages":[{"role":"user","content":[{"type":"document"}]}]})
            )
            .is_err()
        );
        assert!(
            request_to_chat(json!({"model":"m","messages":[],"thinking":{"type":"enabled"}}))
                .is_err()
        );
    }
    #[test]
    fn response_translates_reasoning_tools_and_usage() {
        let r = response_from_chat(json!({"id":"x","model":"m","choices":[{"finish_reason":"tool_calls","message":{"content":"hello","reasoning_text":"think","reasoning_opaque":"sig","tool_calls":[{"id":"t","type":"function","function":{"name":"f","arguments":"{\"x\":1}"}}]}}],"usage":{"prompt_tokens":12,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":5,"cache_creation_input_tokens":2}}})).unwrap();
        assert_eq!(r["stop_reason"], "tool_use");
        assert_eq!(r["content"][0]["signature"], "sig");
        assert_eq!(r["content"][2]["input"]["x"], 1);
        assert_eq!(r["usage"]["input_tokens"], 5);
        assert_eq!(r["usage"]["cache_read_input_tokens"], 5);
    }
    #[test]
    fn stream_text_tools_trailing_usage() {
        let mut s = ChatStreamTranslator::default();
        let a = s.push(&json!({"id":"a","model":"m","choices":[{"delta":{"content":"hi"},"finish_reason":null}]})).unwrap();
        assert_eq!(a[0]["type"], "message_start");
        assert_eq!(a[2]["delta"]["text"], "hi");
        let b = s.push(&json!({"id":"a","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"f","arguments":"{\"x\":"}}]},"finish_reason":null}]})).unwrap();
        assert_eq!(b[0]["type"], "content_block_stop");
        let c = s.push(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"1}"}}]},"finish_reason":"tool_calls"}]})).unwrap();
        assert_eq!(c[0]["content_block"]["id"], "t");
        assert_eq!(c[1]["delta"]["partial_json"], "{\"x\":1}");
        let d = s
            .push(&json!({"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":2}}))
            .unwrap();
        assert_eq!(d[0]["usage"]["input_tokens"], 9);
        assert_eq!(d[1]["type"], "message_stop");
        assert!(s.finish().unwrap().is_empty());
    }
    #[test]
    fn stream_rejects_truncated_and_oversized() {
        assert!(ChatStreamTranslator::default().finish().is_err());
        let mut s = ChatStreamTranslator::default();
        assert!(s.push(&json!({"id":"i","model":"m","choices":[{"delta":{"tool_calls":[{"index":0,"id":"t","function":{"name":"f","arguments":"x".repeat(MAX_BUFFERED_TOOL_BYTES)}}]}}]})).is_err());
    }
}
