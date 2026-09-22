use crate::error::{ProxyError, ProxyResult};
use crate::models::{openai, responses};
use crate::translate::pipeline::{sanitize_prompt, strip_model_suffix, TranslationPolicy};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn generate_id(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let count = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{:x}{:x}", prefix, now, count)
}

pub fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Translates an incoming OpenAI Responses API request into an upstream OpenAI Chat Completions request.
pub fn translate_responses_request(
    req: responses::ResponsesRequest,
    policy: &TranslationPolicy,
) -> ProxyResult<openai::OpenAIRequest> {
    let mut model = policy
        .model_map
        .get(&req.model)
        .cloned()
        .or_else(|| policy.completion_model.clone())
        .unwrap_or_else(|| req.model.clone());

    if policy.strip_model_suffix {
        model = strip_model_suffix(&model);
    }

    let mut messages = Vec::new();

    // 1. If instructions are present, add as system prompt
    if let Some(instructions) = req.instructions {
        let sanitized = sanitize_prompt(instructions, &policy.ignore_terms);
        let trimmed = sanitized.trim();
        if !trimmed.is_empty() {
            messages.push(openai::Message {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(trimmed.to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
    }

    // 2. Translate input items into OpenAI messages
    match req.input {
        responses::ResponsesInput::Text(text) => {
            messages.push(openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text(text)),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        responses::ResponsesInput::Items(items) => {
            for item in items {
                match item {
                    responses::ResponseInputItem::Message { role, content, .. } => {
                        let msg_content = match content {
                            responses::ResponseMessageContent::Text(t) => {
                                openai::MessageContent::Text(t)
                            }
                            responses::ResponseMessageContent::Parts(parts) => {
                                let mut conv_parts = Vec::new();
                                for p in parts {
                                    if let Some(txt) = p.text {
                                        conv_parts.push(openai::ContentPart::Text { text: txt });
                                    } else if let Some(img) = p.image_url {
                                        let url = if let Some(s) = img.as_str() {
                                            s.to_string()
                                        } else if let Some(u) =
                                            img.get("url").and_then(|v| v.as_str())
                                        {
                                            u.to_string()
                                        } else {
                                            img.to_string()
                                        };
                                        conv_parts.push(openai::ContentPart::ImageUrl {
                                            image_url: openai::ImageUrl { url },
                                        });
                                    }
                                }
                                openai::MessageContent::Parts(conv_parts)
                            }
                        };
                        messages.push(openai::Message {
                            role,
                            content: Some(msg_content),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    responses::ResponseInputItem::FunctionCall {
                        id,
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
                        let resolved_id = call_id.or(id).unwrap_or_else(|| generate_id("call"));
                        messages.push(openai::Message {
                            role: "assistant".to_string(),
                            content: None,
                            reasoning_content: None,
                            tool_calls: Some(vec![openai::ToolCall {
                                id: resolved_id,
                                call_type: "function".to_string(),
                                function: openai::FunctionCall { name, arguments },
                            }]),
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    responses::ResponseInputItem::FunctionCallOutput {
                        call_id, output, ..
                    } => {
                        let content_str = match output {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        messages.push(openai::Message {
                            role: "tool".to_string(),
                            content: Some(openai::MessageContent::Text(content_str)),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: Some(call_id),
                            name: None,
                        });
                    }
                    responses::ResponseInputItem::Raw(val) => {
                        if let Some(role) = val.get("role").and_then(|r| r.as_str()) {
                            let content_text = val
                                .get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            messages.push(openai::Message {
                                role: role.to_string(),
                                content: Some(openai::MessageContent::Text(content_text)),
                                reasoning_content: None,
                                tool_calls: None,
                                tool_call_id: None,
                                name: None,
                            });
                        }
                    }
                }
            }
        }
    }

    // 3. Translate tools
    //
    // The Responses API carries several tool shapes that Chat Completions has no
    // equivalent for. Codex sends `custom` (freeform `apply_patch`), `namespace`
    // (a group of nested functions) and `web_search` alongside ordinary
    // functions. Emitting those verbatim as `{"type": <kind>, "function": {...}}`
    // is not valid Chat Completions and the gateway rejects the whole request
    // with `11133 Invalid request parameters`, so each shape is mapped onto a
    // plain function tool (or dropped when it has no callable equivalent).
    let tools = req.tools.and_then(|tools| {
        let mapped = normalize_response_tools(tools);
        if mapped.is_empty() {
            None
        } else {
            Some(mapped)
        }
    });
    let tool_choice = normalize_tool_choice(req.tool_choice, &tools);

    let max_tokens = req.max_output_tokens.or(req.max_tokens);
    let stream = req.stream;
    let stream_options = stream.and_then(|s| {
        s.then_some(openai::StreamOptions {
            include_usage: true,
        })
    });

    Ok(openai::OpenAIRequest {
        model,
        messages,
        max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop: None,
        stream,
        stream_options,
        tools,
        tool_choice,
        extra: serde_json::Map::new(),
    })
}

/// Convert Responses API tools into the Chat Completions function shape.
///
/// Chat Completions only understands `{"type":"function","function":{...}}`.
/// Responses clients send richer kinds, so each is either flattened or dropped:
///
/// * `function` — passed through, defaulting missing parameters to an open object.
/// * `namespace` — a group whose nested `function` tools are flattened to the top
///   level, since Chat Completions has no grouping concept.
/// * `custom` — a freeform tool (Codex `apply_patch`) whose input is a raw string
///   rather than JSON. Modelled as a single required string parameter so the
///   model can still call it.
/// * anything else (`web_search`, `local_shell`, …) — dropped: there is no
///   upstream equivalent, and forwarding the unknown type makes the gateway
///   reject the entire request.
fn normalize_response_tools(tools: Vec<responses::ResponseTool>) -> Vec<openai::Tool> {
    let mut out = Vec::new();

    for tool in tools {
        match tool.tool_type.as_str() {
            // A nested namespace is a container, not a callable tool.
            "namespace" => {
                if let Some(nested) = tool.tools {
                    out.extend(normalize_response_tools(nested));
                }
            }
            "custom" => {
                let Some(name) = tool.name.filter(|n| !n.is_empty()) else {
                    continue;
                };
                // `format` carries a grammar/lark definition that Chat Completions
                // cannot express; expose the freeform payload as one string input.
                let description = tool.description.map(|d| {
                    format!("{d}\n\nProvide the tool input as a single string in `input`.")
                });
                out.push(openai::Tool {
                    tool_type: "function".to_string(),
                    function: openai::Function {
                        name,
                        description,
                        parameters: json!({
                            "type": "object",
                            "properties": {
                                "input": {
                                    "type": "string",
                                    "description": "Raw freeform tool input."
                                }
                            },
                            "required": ["input"]
                        }),
                    },
                });
            }
            "function" => {
                if let Some(func) = tool.function {
                    // An empty tool name is rejected upstream; drop rather than fail
                    // the entire turn.
                    if func.name.is_empty() {
                        continue;
                    }
                    out.push(openai::Tool {
                        tool_type: "function".to_string(),
                        function: func,
                    });
                } else if let Some(name) = tool.name.filter(|n| !n.is_empty()) {
                    out.push(openai::Tool {
                        tool_type: "function".to_string(),
                        function: openai::Function {
                            name,
                            description: tool.description,
                            parameters: tool
                                .parameters
                                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                        },
                    });
                }
            }
            _ => {}
        }
    }

    out
}

/// Translate a Responses `tool_choice` into a Chat Completions value.
///
/// String forms (`auto` / `none` / `required`) are shared. The Responses object
/// form `{"type":"function","name":…}` has to become the Chat Completions
/// `{"type":"function","function":{"name":…}}`, because upstream rejects the
/// object form outright ("cannot unmarshal object into Go struct field
/// Request.tool_choice of type string").
fn normalize_tool_choice(
    choice: Option<Value>,
    tools: &Option<Vec<openai::Tool>>,
) -> Option<Value> {
    let choice = choice?;

    match choice {
        Value::String(_) => Some(choice),
        Value::Object(ref obj) => {
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or_default();
            // `namespace` selection refers to nested tools upstream cannot see, and a
            // dangling reference would be rejected; let the model choose freely.
            let name = obj.get("name").and_then(Value::as_str).or_else(|| {
                obj.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            });

            match (kind, name) {
                ("function", Some(name)) if tool_exists(tools, name) => {
                    Some(json!({"type": "function", "function": {"name": name}}))
                }
                // A choice naming a dropped tool (e.g. web_search) cannot be honoured.
                _ => Some(json!("auto")),
            }
        }
        _ => Some(json!("auto")),
    }
}

/// Whether a tool with `name` survived normalization.
fn tool_exists(tools: &Option<Vec<openai::Tool>>, name: &str) -> bool {
    tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t.function.name == name))
}

/// Translates an upstream non-streaming OpenAI Chat Completions response into an OpenAI Responses API response.
pub fn translate_responses_response(
    resp: openai::OpenAIResponse,
    requested_model: &str,
) -> ProxyResult<responses::ResponsesResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| ProxyError::Transform("No choices in response".to_string()))?;

    let response_id = resp.id.unwrap_or_else(|| generate_id("resp"));
    let model = resp.model.unwrap_or_else(|| requested_model.to_string());
    let created_at = resp
        .created
        .map(|c| c as i64)
        .unwrap_or_else(current_timestamp);

    let mut output = Vec::new();
    let mut response_content_items = Vec::new();

    if let Some(ref text) = choice.message.content {
        if !text.is_empty() {
            output.push(responses::OutputItem::Message {
                id: generate_id("msg"),
                status: "completed".to_string(),
                role: choice.message.role.clone(),
                content: vec![responses::OutputContentPart::OutputText { text: text.clone() }],
            });

            response_content_items.push(responses::ResponseContentItem {
                content_type: "output_text".to_string(),
                text: Some(text.clone()),
            });
        }
    }

    if let Some(ref tool_calls) = choice.message.tool_calls {
        for call in tool_calls {
            output.push(responses::OutputItem::FunctionCall {
                id: call.id.clone(),
                call_id: call.id.clone(),
                status: "completed".to_string(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            });
        }
    }

    let response_output = if !response_content_items.is_empty() {
        Some(responses::ResponseOutput {
            role: Some(choice.message.role.clone()),
            content: Some(response_content_items),
        })
    } else {
        None
    };

    let usage = Some(responses::ResponsesUsage {
        total_tokens: resp.usage.total_tokens,
        input_tokens: resp.usage.prompt_tokens,
        output_tokens: resp.usage.completion_tokens,
        input_tokens_details: resp.usage.prompt_tokens_details,
    });

    Ok(responses::ResponsesResponse {
        id: response_id,
        object: "response".to_string(),
        created_at,
        status: "completed".to_string(),
        model,
        output,
        usage,
        response: response_output,
    })
}

/// State tracker for translating an upstream OpenAI Chat SSE stream to Responses API SSE events.
#[derive(Debug)]
pub struct ResponsesStreamState {
    pub response_id: String,
    pub model: Option<String>,
    pub fallback_model: String,
    pub created_at: i64,
    created_emitted: bool,
    text_item_started: bool,
    text_item_id: String,
    accumulated_text: String,
    active_tool_calls: Vec<StreamingToolCall>,
    output_index_counter: usize,
    finalized: bool,
    pending_usage: Option<openai::Usage>,
}

#[derive(Debug, Clone)]
struct StreamingToolCall {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
    started: bool,
    done: bool,
}

pub fn initial_stream_state(fallback_model: String) -> ResponsesStreamState {
    ResponsesStreamState {
        response_id: generate_id("resp"),
        model: None,
        fallback_model,
        created_at: current_timestamp(),
        created_emitted: false,
        text_item_started: false,
        text_item_id: generate_id("msg"),
        accumulated_text: String::new(),
        active_tool_calls: Vec::new(),
        output_index_counter: 0,
        finalized: false,
        pending_usage: None,
    }
}

impl ResponsesStreamState {
    pub fn model(&self) -> &str {
        self.model
            .as_deref()
            .unwrap_or(self.fallback_model.as_str())
    }
}

/// Translates a single incoming OpenAI `StreamChunk` into a series of `ResponsesStreamEvent`s.
pub fn translate_stream_chunk(
    state: &mut ResponsesStreamState,
    chunk: &openai::StreamChunk,
) -> Vec<responses::ResponsesStreamEvent> {
    let mut events = Vec::new();

    if let Some(id) = &chunk.id {
        if state.response_id.starts_with("resp_") && !id.is_empty() {
            // Can retain or track original id if helpful
        }
    }
    if let Some(m) = &chunk.model {
        if state.model.is_none() {
            state.model = Some(m.clone());
        }
    }
    if chunk.usage.is_some() {
        state.pending_usage = chunk.usage.clone();
    }

    // 1. Emit `response.created` on the first chunk
    if !state.created_emitted {
        state.created_emitted = true;
        events.push(responses::ResponsesStreamEvent::Created {
            response: responses::ResponseStreamMetadata {
                id: state.response_id.clone(),
                object: "response".to_string(),
                status: "in_progress".to_string(),
                model: state.model().to_string(),
                output: Vec::new(),
                usage: None,
            },
        });
    }

    let Some(choice) = chunk.choices.first() else {
        return events;
    };

    // 2. Handle text content delta
    if let Some(ref content) = choice.delta.content {
        if !content.is_empty() {
            if !state.text_item_started {
                state.text_item_started = true;
                let output_index = state.output_index_counter;
                state.output_index_counter += 1;

                events.push(responses::ResponsesStreamEvent::OutputItemAdded {
                    response_id: state.response_id.clone(),
                    output_index,
                    item: responses::OutputItem::Message {
                        id: state.text_item_id.clone(),
                        status: "in_progress".to_string(),
                        role: "assistant".to_string(),
                        content: Vec::new(),
                    },
                });

                events.push(responses::ResponsesStreamEvent::ContentPartAdded {
                    response_id: state.response_id.clone(),
                    item_id: state.text_item_id.clone(),
                    output_index,
                    content_index: 0,
                    part: responses::OutputContentPart::OutputText {
                        text: String::new(),
                    },
                });
            }

            state.accumulated_text.push_str(content);

            events.push(responses::ResponsesStreamEvent::OutputTextDelta {
                response_id: state.response_id.clone(),
                item_id: state.text_item_id.clone(),
                output_index: 0,
                content_index: 0,
                delta: content.clone(),
            });
        }
    }

    // 3. Handle tool calls delta
    if let Some(ref tool_calls) = choice.delta.tool_calls {
        for call in tool_calls {
            let index = call.index;
            while state.active_tool_calls.len() <= index {
                let output_index = state.output_index_counter;
                state.output_index_counter += 1;
                state.active_tool_calls.push(StreamingToolCall {
                    id: generate_id("call"),
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    output_index,
                    started: false,
                    done: false,
                });
            }

            let tool_entry = &mut state.active_tool_calls[index];
            if let Some(ref id) = call.id {
                tool_entry.call_id = id.clone();
                tool_entry.id = id.clone();
            }
            if let Some(ref func) = call.function {
                if let Some(ref name) = func.name {
                    tool_entry.name.push_str(name);
                }
            }

            if !tool_entry.started
                && (!tool_entry.name.is_empty() || !tool_entry.call_id.is_empty())
            {
                tool_entry.started = true;
                events.push(responses::ResponsesStreamEvent::OutputItemAdded {
                    response_id: state.response_id.clone(),
                    output_index: tool_entry.output_index,
                    item: responses::OutputItem::FunctionCall {
                        id: tool_entry.id.clone(),
                        call_id: tool_entry.call_id.clone(),
                        status: "in_progress".to_string(),
                        name: tool_entry.name.clone(),
                        arguments: String::new(),
                    },
                });
            }

            if let Some(ref func) = call.function {
                if let Some(ref args) = func.arguments {
                    if !args.is_empty() {
                        tool_entry.arguments.push_str(args);
                        events.push(
                            responses::ResponsesStreamEvent::FunctionCallArgumentsDelta {
                                response_id: state.response_id.clone(),
                                item_id: tool_entry.id.clone(),
                                output_index: tool_entry.output_index,
                                call_id: tool_entry.call_id.clone(),
                                delta: args.clone(),
                            },
                        );
                    }
                }
            }
        }
    }

    // 4. Handle finish_reason
    if choice.finish_reason.is_some() {
        events.extend(close_stream_items(state));
    }

    events
}

fn close_stream_items(state: &mut ResponsesStreamState) -> Vec<responses::ResponsesStreamEvent> {
    let mut events = Vec::new();
    if state.finalized {
        return events;
    }

    // Close text item if active
    if state.text_item_started {
        events.push(responses::ResponsesStreamEvent::OutputTextDone {
            response_id: state.response_id.clone(),
            item_id: state.text_item_id.clone(),
            output_index: 0,
            content_index: 0,
            text: state.accumulated_text.clone(),
        });

        events.push(responses::ResponsesStreamEvent::OutputItemDone {
            response_id: state.response_id.clone(),
            output_index: 0,
            item: responses::OutputItem::Message {
                id: state.text_item_id.clone(),
                status: "completed".to_string(),
                role: "assistant".to_string(),
                content: vec![responses::OutputContentPart::OutputText {
                    text: state.accumulated_text.clone(),
                }],
            },
        });
    }

    // Close tool calls if active
    for tool_entry in &mut state.active_tool_calls {
        if tool_entry.started && !tool_entry.done {
            tool_entry.done = true;
            events.push(responses::ResponsesStreamEvent::FunctionCallArgumentsDone {
                response_id: state.response_id.clone(),
                item_id: tool_entry.id.clone(),
                output_index: tool_entry.output_index,
                call_id: tool_entry.call_id.clone(),
                arguments: tool_entry.arguments.clone(),
            });

            events.push(responses::ResponsesStreamEvent::OutputItemDone {
                response_id: state.response_id.clone(),
                output_index: tool_entry.output_index,
                item: responses::OutputItem::FunctionCall {
                    id: tool_entry.id.clone(),
                    call_id: tool_entry.call_id.clone(),
                    status: "completed".to_string(),
                    name: tool_entry.name.clone(),
                    arguments: tool_entry.arguments.clone(),
                },
            });
        }
    }

    // Assemble completed output items
    let mut output = Vec::new();
    let mut response_content_items = Vec::new();

    if state.text_item_started {
        output.push(responses::OutputItem::Message {
            id: state.text_item_id.clone(),
            status: "completed".to_string(),
            role: "assistant".to_string(),
            content: vec![responses::OutputContentPart::OutputText {
                text: state.accumulated_text.clone(),
            }],
        });
        response_content_items.push(responses::ResponseContentItem {
            content_type: "output_text".to_string(),
            text: Some(state.accumulated_text.clone()),
        });
    }

    for tool_entry in &state.active_tool_calls {
        if tool_entry.started {
            output.push(responses::OutputItem::FunctionCall {
                id: tool_entry.id.clone(),
                call_id: tool_entry.call_id.clone(),
                status: "completed".to_string(),
                name: tool_entry.name.clone(),
                arguments: tool_entry.arguments.clone(),
            });
        }
    }

    let response_output = if !response_content_items.is_empty() {
        Some(responses::ResponseOutput {
            role: Some("assistant".to_string()),
            content: Some(response_content_items),
        })
    } else {
        None
    };

    let usage = state
        .pending_usage
        .as_ref()
        .map(|u| responses::ResponsesUsage {
            total_tokens: u.total_tokens,
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            input_tokens_details: u.prompt_tokens_details.clone(),
        });

    events.push(responses::ResponsesStreamEvent::Completed {
        response: responses::ResponsesResponse {
            id: state.response_id.clone(),
            object: "response".to_string(),
            created_at: state.created_at,
            status: "completed".to_string(),
            model: state.model().to_string(),
            output,
            usage,
            response: response_output,
        },
    });

    state.finalized = true;
    events
}

/// Closes any unfinalized stream items and emits `response.completed` upon stream completion.
pub fn translate_stream_done(
    state: &mut ResponsesStreamState,
) -> Vec<responses::ResponsesStreamEvent> {
    if !state.finalized {
        close_stream_items(state)
    } else {
        Vec::new()
    }
}

/// Translates a stream error into `response.failed`.
pub fn translate_stream_error(
    state: &ResponsesStreamState,
    error_message: String,
) -> Vec<responses::ResponsesStreamEvent> {
    vec![responses::ResponsesStreamEvent::Failed {
        response_id: state.response_id.clone(),
        error: responses::StreamErrorDetail {
            message: error_message,
            error_type: "server_error".to_string(),
            code: None,
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_policy() -> TranslationPolicy {
        TranslationPolicy {
            reasoning_model: None,
            completion_model: None,
            model_map: BTreeMap::new(),
            ignore_terms: vec!["ignore-me".to_string()],
            strip_model_suffix: false,
            sanitize_fingerprints: true,
        }
    }

    #[test]
    fn test_translate_request_string_input_and_instructions() {
        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Text("Hello AI".to_string()),
            instructions: Some("You are helpful. ignore-me".to_string()),
            tools: None,
            tool_choice: None,
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_output_tokens: Some(100),
            max_tokens: None,
            stream: Some(true),
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
        };

        let policy = test_policy();
        let openai_req = translate_responses_request(req, &policy).unwrap();

        assert_eq!(openai_req.model, "gpt-4o");
        assert_eq!(openai_req.messages.len(), 2);
        assert_eq!(openai_req.messages[0].role, "system");
        if let Some(openai::MessageContent::Text(ref txt)) = openai_req.messages[0].content {
            assert_eq!(txt, "You are helpful.");
        } else {
            panic!("Expected text content");
        }

        assert_eq!(openai_req.messages[1].role, "user");
        if let Some(openai::MessageContent::Text(ref txt)) = openai_req.messages[1].content {
            assert_eq!(txt, "Hello AI");
        } else {
            panic!("Expected text content");
        }

        assert_eq!(openai_req.temperature, Some(0.7));
        assert_eq!(openai_req.max_tokens, Some(100));
        assert_eq!(openai_req.stream, Some(true));
    }

    #[test]
    fn test_translate_request_with_tool_call_and_output() {
        let items = vec![
            responses::ResponseInputItem::Message {
                item_type: Some("message".to_string()),
                id: None,
                role: "user".to_string(),
                content: responses::ResponseMessageContent::Text(
                    "What is the weather?".to_string(),
                ),
            },
            responses::ResponseInputItem::FunctionCall {
                item_type: "function_call".to_string(),
                id: Some("call_123".to_string()),
                call_id: Some("call_123".to_string()),
                name: "get_weather".to_string(),
                arguments: "{\"location\":\"Paris\"}".to_string(),
            },
            responses::ResponseInputItem::FunctionCallOutput {
                item_type: "function_call_output".to_string(),
                call_id: "call_123".to_string(),
                output: Value::String("22°C, Sunny".to_string()),
            },
        ];

        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Items(items),
            instructions: None,
            tools: Some(vec![responses::ResponseTool {
                tool_type: "function".to_string(),
                name: Some("get_weather".to_string()),
                description: Some("Get current weather".to_string()),
                parameters: Some(json!({"type": "object"})),
                function: None,
                tools: None,
            }]),
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: None,
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
        };

        let policy = test_policy();
        let openai_req = translate_responses_request(req, &policy).unwrap();

        assert_eq!(openai_req.messages.len(), 3);
        assert_eq!(openai_req.messages[0].role, "user");
        assert_eq!(openai_req.messages[1].role, "assistant");
        assert!(openai_req.messages[1].tool_calls.is_some());
        assert_eq!(openai_req.messages[2].role, "tool");
        assert_eq!(
            openai_req.messages[2].tool_call_id,
            Some("call_123".to_string())
        );

        let tools = openai_req.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "get_weather");
    }

    /// Build a minimal request carrying `tools`, for the shape-normalization tests.
    fn req_with_tools(tools: Vec<responses::ResponseTool>) -> responses::ResponsesRequest {
        responses::ResponsesRequest {
            model: "hy3".to_string(),
            input: responses::ResponsesInput::Text("hi".to_string()),
            instructions: None,
            tools: Some(tools),
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: Some(true),
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
        }
    }

    fn function_tool(name: &str) -> responses::ResponseTool {
        responses::ResponseTool {
            tool_type: "function".to_string(),
            name: Some(name.to_string()),
            description: None,
            parameters: None,
            function: None,
            tools: None,
        }
    }

    /// Codex sends `custom` and `namespace` tools; emitting them verbatim used to
    /// make the gateway reject the whole turn with 11133.
    #[test]
    fn codex_custom_and_namespace_tools_become_functions() {
        let tools = vec![
            responses::ResponseTool {
                tool_type: "custom".to_string(),
                name: Some("apply_patch".to_string()),
                description: Some("Edit files.".to_string()),
                parameters: None,
                function: None,
                tools: None,
            },
            responses::ResponseTool {
                tool_type: "namespace".to_string(),
                name: Some("multi_agent_v1".to_string()),
                description: None,
                parameters: None,
                function: None,
                tools: Some(vec![
                    function_tool("close_agent"),
                    function_tool("spawn_agent"),
                ]),
            },
        ];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        let tools = out.tools.expect("tools should survive");

        assert_eq!(
            tools
                .iter()
                .map(|t| t.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["apply_patch", "close_agent", "spawn_agent"]
        );
        // Every emitted tool must be a plain function; anything else is rejected upstream.
        assert!(tools.iter().all(|t| t.tool_type == "function"));
        // The freeform tool must expose its payload as a required string.
        let apply_patch = &tools[0].function.parameters;
        assert_eq!(apply_patch["required"], json!(["input"]));
        assert_eq!(apply_patch["properties"]["input"]["type"], "string");
    }

    /// Tools with no Chat Completions equivalent are dropped rather than forwarded.
    #[test]
    fn unsupported_tool_kinds_are_dropped() {
        let tools = vec![
            function_tool("exec_command"),
            responses::ResponseTool {
                tool_type: "web_search".to_string(),
                name: None,
                description: None,
                parameters: None,
                function: None,
                tools: None,
            },
        ];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        let tools = out.tools.expect("kept tools should survive");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "exec_command");
    }

    /// A tool list that normalizes to nothing must omit `tools` entirely.
    #[test]
    fn only_unsupported_tools_yields_no_tools_field() {
        let tools = vec![responses::ResponseTool {
            tool_type: "web_search".to_string(),
            name: None,
            description: None,
            parameters: None,
            function: None,
            tools: None,
        }];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        assert!(out.tools.is_none());
    }

    /// The object form of `tool_choice` must be rewritten for Chat Completions.
    #[test]
    fn object_tool_choice_is_rewritten_to_chat_shape() {
        let mut req = req_with_tools(vec![function_tool("exec_command")]);
        req.tool_choice = Some(json!({"type": "function", "name": "exec_command"}));

        let out = translate_responses_request(req, &test_policy()).unwrap();
        assert_eq!(
            out.tool_choice,
            Some(json!({"type": "function", "function": {"name": "exec_command"}}))
        );
    }

    /// Choosing a dropped tool cannot be honoured, so fall back to `auto`.
    #[test]
    fn tool_choice_naming_dropped_tool_falls_back_to_auto() {
        let mut req = req_with_tools(vec![
            function_tool("exec_command"),
            responses::ResponseTool {
                tool_type: "web_search".to_string(),
                name: None,
                description: None,
                parameters: None,
                function: None,
                tools: None,
            },
        ]);
        req.tool_choice = Some(json!({"type": "web_search"}));

        let out = translate_responses_request(req, &test_policy()).unwrap();
        assert_eq!(out.tool_choice, Some(json!("auto")));
    }

    /// String forms are shared with Chat Completions and pass through unchanged.
    #[test]
    fn string_tool_choice_passes_through() {
        for value in ["auto", "none", "required"] {
            let mut req = req_with_tools(vec![function_tool("exec_command")]);
            req.tool_choice = Some(json!(value));
            let out = translate_responses_request(req, &test_policy()).unwrap();
            assert_eq!(out.tool_choice, Some(json!(value)));
        }
    }

    /// A `function` entry may arrive with a top-level name instead of `function`.
    #[test]
    fn function_tool_with_top_level_name_keeps_parameters() {
        let tools = vec![responses::ResponseTool {
            tool_type: "function".to_string(),
            name: Some("get_weather".to_string()),
            description: Some("weather".to_string()),
            parameters: Some(json!({"type": "object", "properties": {"city": {"type": "string"}}})),
            function: None,
            tools: None,
        }];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(
            tools[0].function.parameters["properties"]["city"]["type"],
            "string"
        );
    }

    /// A `function` tool without parameters still gets a valid empty schema.
    #[test]
    fn function_tool_without_parameters_gets_empty_schema() {
        let out = translate_responses_request(
            req_with_tools(vec![function_tool("ping")]),
            &test_policy(),
        )
        .unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(
            tools[0].function.parameters,
            json!({"type": "object", "properties": {}})
        );
    }

    /// An unnamed function tool is dropped instead of sending an empty name upstream.
    #[test]
    fn function_tool_with_empty_name_is_dropped() {
        let out = translate_responses_request(
            req_with_tools(vec![function_tool(""), function_tool("keep_me")]),
            &test_policy(),
        )
        .unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "keep_me");
    }

    #[test]
    fn test_translate_non_streaming_response() {
        let openai_resp = openai::OpenAIResponse {
            id: Some("chatcmpl-test".to_string()),
            object: Some("chat.completion".to_string()),
            created: Some(1712345678),
            model: Some("gpt-4o-2024-05-13".to_string()),
            choices: vec![openai::Choice {
                index: 0,
                message: openai::ChoiceMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello! How can I assist you?".to_string()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: openai::Usage {
                prompt_tokens: 10,
                completion_tokens: 8,
                total_tokens: 18,
                prompt_tokens_details: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                ..Default::default()
            },
            system_fingerprint: None,
        };

        let resp = translate_responses_response(openai_resp, "gpt-4o").unwrap();
        assert_eq!(resp.id, "chatcmpl-test");
        assert_eq!(resp.model, "gpt-4o-2024-05-13");
        assert_eq!(resp.output.len(), 1);
        match &resp.output[0] {
            responses::OutputItem::Message { content, role, .. } => {
                assert_eq!(role, "assistant");
                assert_eq!(content.len(), 1);
                match &content[0] {
                    responses::OutputContentPart::OutputText { text } => {
                        assert_eq!(text, "Hello! How can I assist you?");
                    }
                }
            }
            _ => panic!("Expected Message output item"),
        }

        assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 18);
        assert!(resp.response.is_some());
    }

    #[test]
    fn test_stream_lifecycle() {
        let mut state = initial_stream_state("gpt-4o".to_string());

        let chunk1 = openai::StreamChunk {
            id: Some("chunk-1".to_string()),
            object: Some("chat.completion.chunk".to_string()),
            created: Some(1712345678),
            model: Some("gpt-4o".to_string()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: Some("assistant".to_string()),
                    content: Some("Hello".to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events1 = translate_stream_chunk(&mut state, &chunk1);
        // Should have Created, OutputItemAdded, ContentPartAdded, OutputTextDelta
        assert_eq!(events1.len(), 4);
        assert_eq!(events1[0].event_type(), "response.created");
        assert_eq!(events1[1].event_type(), "response.output_item.added");
        assert_eq!(events1[2].event_type(), "response.content_part.added");
        assert_eq!(events1[3].event_type(), "response.output_text.delta");

        let chunk2 = openai::StreamChunk {
            id: Some("chunk-2".to_string()),
            object: Some("chat.completion.chunk".to_string()),
            created: Some(1712345679),
            model: Some("gpt-4o".to_string()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: None,
                    content: Some(" world!".to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(openai::Usage {
                prompt_tokens: 5,
                completion_tokens: 3,
                total_tokens: 8,
                prompt_tokens_details: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                ..Default::default()
            }),
        };

        let events2 = translate_stream_chunk(&mut state, &chunk2);
        // OutputTextDelta, OutputTextDone, OutputItemDone, Completed
        assert_eq!(events2.len(), 4);
        assert_eq!(events2[0].event_type(), "response.output_text.delta");
        assert_eq!(events2[1].event_type(), "response.output_text.done");
        assert_eq!(events2[2].event_type(), "response.output_item.done");
        assert_eq!(events2[3].event_type(), "response.completed");

        let done_events = translate_stream_done(&mut state);
        assert!(done_events.is_empty(), "Already finalized");
    }
}
