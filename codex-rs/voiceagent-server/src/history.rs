use std::collections::VecDeque;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;

use serde_json::Value;
use serde_json::json;

const MAX_HISTORY_ITEMS: usize = 1_000;

pub fn read_history(path: &Path) -> anyhow::Result<Vec<Value>> {
    let file = File::open(path)?;
    let mut items = VecDeque::with_capacity(MAX_HISTORY_ITEMS);
    let mut last_usage = None;
    let mut last_context_window = None;

    for line in BufReader::new(file).lines() {
        let line = line?;
        let Ok(record) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let timestamp = record
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let record_type = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let payload = &record["payload"];
        let payload_type = payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        let item = match (record_type, payload_type) {
            ("event_msg", "task_started") => Some(json!({
                "type": "turn.started",
                "timestamp": timestamp,
                "turnId": payload["turn_id"]
            })),
            ("event_msg", "user_message") => text_value(payload.get("message")).map(|text| {
                json!({"type": "message", "role": "user", "timestamp": timestamp, "text": text})
            }),
            ("event_msg", "agent_message") => text_value(payload.get("message")).map(|text| {
                json!({"type": "message", "role": "agent", "timestamp": timestamp, "text": text})
            }),
            ("response_item", "reasoning") => reasoning_summary(payload).map(|text| {
                json!({"type": "reasoning", "timestamp": timestamp, "text": text})
            }),
            ("response_item", "function_call") => Some(function_call(payload, timestamp)),
            ("response_item", "function_call_output") => Some(json!({
                "type": "tool.output",
                "timestamp": timestamp,
                "callId": payload["call_id"],
                "text": payload.get("output").and_then(Value::as_str).unwrap_or_default()
            })),
            ("event_msg", "token_count") => {
                last_usage = payload.pointer("/info/last_token_usage").cloned();
                last_context_window = payload
                    .pointer("/info/model_context_window")
                    .and_then(Value::as_i64);
                None
            }
            ("event_msg", "task_complete") => Some(json!({
                "type": "turn.completed",
                "timestamp": timestamp,
                "turnId": payload["turn_id"],
                "durationMs": payload["duration_ms"],
                "timeToFirstTokenMs": payload["time_to_first_token_ms"],
                "usage": last_usage.take().unwrap_or(Value::Null),
                "contextWindow": last_context_window.take()
            })),
            _ => None,
        };

        if let Some(item) = item {
            if items.len() == MAX_HISTORY_ITEMS {
                items.pop_front();
            }
            items.push_back(item);
        }
    }

    Ok(items.into())
}

fn function_call(payload: &Value, timestamp: &str) -> Value {
    let name = payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let arguments_text = payload
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = serde_json::from_str::<Value>(arguments_text).unwrap_or(Value::Null);
    let command = arguments
        .get("cmd")
        .and_then(Value::as_str)
        .or_else(|| arguments.get("command").and_then(Value::as_str));
    let display = command
        .map(str::to_string)
        .unwrap_or_else(|| compact_json(&arguments, arguments_text));
    json!({
        "type": "tool.call",
        "timestamp": timestamp,
        "callId": payload["call_id"],
        "name": name,
        "text": display
    })
}

fn compact_json(value: &Value, fallback: &str) -> String {
    if value.is_null() {
        fallback.to_string()
    } else {
        serde_json::to_string(value).unwrap_or_else(|_| fallback.to_string())
    }
}

fn reasoning_summary(payload: &Value) -> Option<String> {
    let summary = payload.get("summary")?.as_array()?;
    let text = summary
        .iter()
        .filter_map(|item| {
            item.get("text")
                .and_then(Value::as_str)
                .or_else(|| item.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn text_value(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|text| !text.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_displayable_rollout_items() {
        let payload = json!({
            "type": "function_call",
            "name": "exec_command",
            "call_id": "call-1",
            "arguments": "{\"cmd\":\"python3 /workspace/.agents/skills/test/run.py\"}"
        });

        let item = function_call(&payload, "2026-08-29T00:00:00Z");
        assert_eq!(item["type"], "tool.call");
        assert_eq!(
            item["text"],
            "python3 /workspace/.agents/skills/test/run.py"
        );
    }
}
