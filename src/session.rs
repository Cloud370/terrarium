use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::config::{state_dir, ResolvedProfile};
use crate::kernel::FACTS_CAP;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkingRoot {
    pub display_path: String,
    pub canonical_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionHeader {
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    pub id: String,
    #[serde(rename = "workingRoot")]
    pub working_root: WorkingRoot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: String,
    pub seq: u64,
    /// Wall-clock append time, epoch milliseconds. Operator-facing forensics (per-step LLM
    /// latency, turn duration); never projected into model-visible context. Absent in
    /// journals written before the field existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,
    pub data: Value,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct Journal {
    pub id: String,
    file: File,
    pub header: SessionHeader,
    pub events: Vec<Event>,
}

impl Journal {
    pub fn create(root: &Path) -> Result<Self, String> {
        if !root.is_absolute() {
            return Err("working root must be absolute".into());
        }
        let display = root.to_path_buf();
        let canonical = display
            .canonicalize()
            .map_err(|e| format!("working root is invalid: {e}"))?;
        if !canonical.is_dir() {
            return Err(format!(
                "working root is not a directory: {}",
                display.display()
            ));
        }
        let id = format!(
            "ses_{}_{:x}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let dir = state_dir()?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create session directory: {e}"))?;
        let path = dir.join(format!("{id}.jsonl"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("cannot create session {id}: {e}"))?;
        file.try_lock_exclusive()
            .map_err(|e| format!("cannot lock session {id}: {e}"))?;
        let header = SessionHeader {
            kind: "session".into(),
            version: 1,
            id: id.clone(),
            working_root: WorkingRoot {
                display_path: display.to_string_lossy().into_owned(),
                canonical_path: canonical.to_string_lossy().into_owned(),
            },
        };
        write_line(&mut file, &header)?;
        file.sync_all()
            .map_err(|e| format!("cannot sync session header: {e}"))?;
        Ok(Self {
            id,
            file,
            header,
            events: Vec::new(),
        })
    }

    pub fn open(id: &str) -> Result<Self, String> {
        let path = state_dir()?.join(format!("{id}.jsonl"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| format!("cannot open session {id}: {e}"))?;
        file.try_lock_exclusive()
            .map_err(|e| format!("cannot lock session {id}: {e}"))?;
        let mut reader = BufReader::new(file.try_clone().map_err(|e| e.to_string())?);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .map_err(|e| format!("session {id}: cannot read journal: {e}"))?;
            if n == 0 {
                break;
            }
            if !line.ends_with('\n') {
                let truncate_at = reader.stream_position().map_err(|e| e.to_string())? - n as u64;
                file.set_len(truncate_at).map_err(|e| {
                    format!("session {id}: cannot remove incomplete final line: {e}")
                })?;
                break;
            }
            lines.push(line);
        }
        if lines.is_empty() {
            return Err(format!("session {id} is empty"));
        }
        let header: SessionHeader = serde_json::from_str(lines[0].trim_end())
            .map_err(|e| format!("session {id}: malformed header: {e}"))?;
        if header.kind != "session" || header.version != 1 || header.id != id {
            return Err(format!("session {id}: invalid header"));
        }
        let root = Path::new(&header.working_root.display_path);
        if !root.is_absolute() {
            return Err(format!("session {id}: stored display path is not absolute"));
        }
        let canonical = root
            .canonicalize()
            .map_err(|e| format!("session {id}: working root unavailable: {e}"))?;
        if canonical.to_string_lossy() != header.working_root.canonical_path || !canonical.is_dir()
        {
            return Err(format!(
                "session {id}: working root changed or is not a directory"
            ));
        }
        let mut events = Vec::new();
        for (index, line) in lines.iter().enumerate().skip(1) {
            let event: Event = serde_json::from_str(line.trim_end())
                .map_err(|e| format!("session {id}: malformed event at line {}: {e}", index + 1))?;
            let expected = events.len() as u64 + 1;
            if event.seq != expected {
                return Err(format!(
                    "session {id}: sequence {} is not contiguous, expected {}",
                    event.seq, expected
                ));
            }
            validate_event(&event, &events)?;
            events.push(event);
        }
        if events.is_empty() {
            return Err(format!("session {id} is incomplete: no turn/start event"));
        }
        let mut file = file;
        file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        Ok(Self {
            id: id.into(),
            file,
            header,
            events,
        })
    }

    pub fn append(&mut self, kind: &str, data: Value) -> Result<u64, String> {
        let seq = self.events.len() as u64 + 1;
        let event = Event {
            kind: kind.into(),
            seq,
            ts: Some(now_ms()),
            data,
        };
        validate_event(&event, &self.events)?;
        write_line(&mut self.file, &event)?;
        self.file
            .sync_all()
            .map_err(|e| format!("session {}: cannot sync event {seq}: {e}", self.id))?;
        self.events.push(event);
        Ok(seq)
    }

    pub fn open_turn(&self) -> Option<&Event> {
        self.events.iter().rev().find(|e| {
            e.kind == "turn/start"
                && !self
                    .events
                    .iter()
                    .any(|x| x.kind == "turn/end" && x.seq > e.seq)
        })
    }
}

fn write_line<T: Serialize>(file: &mut File, value: &T) -> Result<(), String> {
    let text =
        serde_json::to_string(value).map_err(|e| format!("cannot serialize journal event: {e}"))?;
    file.write_all(text.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|e| format!("cannot write journal: {e}"))
}

fn object<'a>(value: &'a Value, seq: u64, label: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("event {seq} {label} must be an object"))
}

fn exact_keys(
    value: &Map<String, Value>,
    required: &[&str],
    optional: &[&str],
    seq: u64,
    label: &str,
) -> Result<(), String> {
    for key in value.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return Err(format!(
                "event {seq} {label} contains unknown field {key:?}"
            ));
        }
    }
    for key in required {
        if !value.contains_key(*key) {
            return Err(format!("event {seq} {label} is missing field {key:?}"));
        }
    }
    Ok(())
}

fn string_field<'a>(
    value: &'a Map<String, Value>,
    key: &str,
    seq: u64,
    label: &str,
) -> Result<&'a str, String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("event {seq} {label}.{key} must be a string"))
}

fn positive_u64(
    value: &Map<String, Value>,
    key: &str,
    seq: u64,
    label: &str,
) -> Result<u64, String> {
    let result = value
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("event {seq} {label}.{key} must be a positive integer"))?;
    if result == 0 {
        return Err(format!("event {seq} {label}.{key} must be positive"));
    }
    Ok(result)
}

fn validate_profile(value: &Value, seq: u64) -> Result<(), String> {
    let profile = object(value, seq, "profile")?;
    exact_keys(
        profile,
        &["name", "protocol", "baseUrl", "model"],
        &[
            "apiKeyEnv",
            "maxOutputTokens",
            "reasoningEffort",
            "requestTimeoutMs",
            "idleTimeoutMs",
            "contextWindow",
        ],
        seq,
        "profile",
    )?;
    string_field(profile, "name", seq, "profile")?;
    let protocol = string_field(profile, "protocol", seq, "profile")?;
    if !crate::llm::protocol_supported(protocol) {
        return Err(format!(
            "event {seq} profile.protocol is unsupported: {protocol:?}"
        ));
    }
    string_field(profile, "baseUrl", seq, "profile")?;
    string_field(profile, "model", seq, "profile")?;
    if let Some(value) = profile.get("apiKeyEnv") {
        string_field(profile, "apiKeyEnv", seq, "profile")?;
        if value.as_str().is_some_and(str::is_empty) {
            return Err(format!("event {seq} profile.apiKeyEnv must not be empty"));
        }
    }
    for field in [
        "maxOutputTokens",
        "requestTimeoutMs",
        "idleTimeoutMs",
        "contextWindow",
    ] {
        if profile
            .get(field)
            .is_some_and(|value| value.as_u64().is_none_or(|number| number == 0))
        {
            return Err(format!("event {seq} profile.{field} must be positive"));
        }
    }
    if let Some(effort) = profile.get("reasoningEffort") {
        if !matches!(effort.as_str(), Some("low" | "medium" | "high")) {
            return Err(format!("event {seq} profile.reasoningEffort is invalid"));
        }
    }
    Ok(())
}

fn validate_disposition(value: &Value, seq: u64) -> Result<(), String> {
    let disposition = object(value, seq, "run disposition")?;
    let to = string_field(disposition, "to", seq, "run disposition")?;
    match to {
        "model" => {
            exact_keys(disposition, &["to", "facts"], &[], seq, "run disposition")?;
            if !disposition["facts"].is_object() {
                return Err(format!(
                    "event {seq} run disposition facts must be an object"
                ));
            }
        }
        "user" => {
            exact_keys(disposition, &["to", "message"], &[], seq, "run disposition")?;
            string_field(disposition, "message", seq, "run disposition")?;
        }
        _ => {
            return Err(format!(
                "event {seq} has unsupported run disposition target {to:?}"
            ))
        }
    }
    Ok(())
}

fn validate_usage(value: &Value, seq: u64) -> Result<(), String> {
    const KEYS: &[&str] = &[
        "inputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
        "reasoningTokens",
    ];
    let usage = object(value, seq, "model/result usage")?;
    exact_keys(usage, KEYS, &[], seq, "model/result usage")?;
    for &key in KEYS {
        if usage.get(key).and_then(Value::as_u64).is_none() {
            return Err(format!(
                "event {seq} model/result usage.{key} must be a non-negative integer"
            ));
        }
    }
    Ok(())
}

// Derived from the transport's caps so the writer and the validator can
// never disagree: the journal rejects exactly what the transport refuses
// to emit.
const REASONING_TEXT_LIMIT: usize = crate::llm::REASONING_TEXT_CAP;
const REASONING_REPLAY_LIMIT: usize = crate::llm::REASONING_REPLAY_CAP;

fn validate_reasoning(value: &Value, seq: u64) -> Result<(), String> {
    let reasoning = object(value, seq, "model/result reasoning")?;
    exact_keys(
        reasoning,
        &["text", "replay"],
        &[],
        seq,
        "model/result reasoning",
    )?;
    let text = string_field(reasoning, "text", seq, "model/result reasoning")?;
    if text.len() > REASONING_TEXT_LIMIT {
        return Err(format!(
            "event {seq} model/result reasoning.text exceeds the {REASONING_TEXT_LIMIT}-byte limit"
        ));
    }
    let replay = reasoning.get("replay").expect("validated key");
    if !replay.is_null() && !replay.is_object() {
        return Err(format!(
            "event {seq} model/result reasoning.replay must be an object or null"
        ));
    }
    if serde_json::to_vec(replay)
        .map(|bytes| bytes.len() > REASONING_REPLAY_LIMIT)
        .unwrap_or(true)
    {
        return Err(format!(
            "event {seq} model/result reasoning.replay exceeds the {REASONING_REPLAY_LIMIT}-byte limit"
        ));
    }
    Ok(())
}

fn validate_limits(value: &Value, seq: u64) -> Result<(), String> {
    let limits = object(value, seq, "limits")?;
    exact_keys(
        limits,
        &["maxSteps", "defaultRunTimeoutMs", "maxRunTimeoutMs"],
        &[],
        seq,
        "limits",
    )?;
    positive_u64(limits, "maxSteps", seq, "limits")?;
    let default_timeout = positive_u64(limits, "defaultRunTimeoutMs", seq, "limits")?;
    let max_timeout = positive_u64(limits, "maxRunTimeoutMs", seq, "limits")?;
    if default_timeout > max_timeout {
        return Err(format!(
            "event {seq} limits.defaultRunTimeoutMs exceeds maxRunTimeoutMs"
        ));
    }
    Ok(())
}

fn has_open(prior: &[Event]) -> bool {
    prior
        .iter()
        .rfind(|event| event.kind == "turn/start")
        .is_some_and(|start| {
            !prior
                .iter()
                .any(|event| event.kind == "turn/end" && event.seq > start.seq)
        })
}

fn previous_request(prior: &[Event], seq: u64) -> Option<&Event> {
    prior
        .iter()
        .find(|event| event.kind == "model/request" && event.seq == seq)
}

fn validate_event(event: &Event, prior: &[Event]) -> Result<(), String> {
    let open = has_open(prior);
    if event.kind != "turn/start" && !open {
        return Err(format!("event {} occurs without an open turn", event.seq));
    }
    if event.kind == "turn/start" && open {
        return Err(format!(
            "event {} starts a turn while another turn is open",
            event.seq
        ));
    }
    match event.kind.as_str() {
        "turn/start" => {
            let data = object(&event.data, event.seq, "turn/start data")?;
            exact_keys(
                data,
                &["message", "systemPrompt", "profile", "limits"],
                &[],
                event.seq,
                "turn/start data",
            )?;
            string_field(data, "message", event.seq, "turn/start data")?;
            string_field(data, "systemPrompt", event.seq, "turn/start data")?;
            validate_profile(data.get("profile").unwrap(), event.seq)?;
            validate_limits(data.get("limits").unwrap(), event.seq)?;
        }
        "model/request" => {
            let data = object(&event.data, event.seq, "model/request data")?;
            exact_keys(
                data,
                &["step", "attempt"],
                &[],
                event.seq,
                "model/request data",
            )?;
            let step = positive_u64(data, "step", event.seq, "model/request data")?;
            let attempt = positive_u64(data, "attempt", event.seq, "model/request data")?;
            if attempt > 2 {
                return Err(format!("event {} attempt must be 1 or 2", event.seq));
            }
            let turn_seq = prior
                .iter()
                .rfind(|event| event.kind == "turn/start")
                .map(|event| event.seq)
                .unwrap_or(0);
            let requests = prior.iter().filter(|event| {
                event.kind == "model/request"
                    && event.seq > turn_seq
                    && event.data["step"].as_u64() == Some(step)
            });
            if attempt == 1 {
                if requests.count() != 0 {
                    return Err(format!(
                        "event {} repeats attempt 1 for step {step}",
                        event.seq
                    ));
                }
                let previous_step = prior
                    .iter()
                    .filter(|event| event.kind == "model/request" && event.seq > turn_seq)
                    .filter_map(|event| event.data["step"].as_u64())
                    .max()
                    .unwrap_or(0);
                if step != previous_step + 1 {
                    return Err(format!("event {} step {} is out of order", event.seq, step));
                }
            } else {
                if prior.iter().any(|event| {
                    event.kind == "model/request"
                        && event.seq > turn_seq
                        && event.data["step"].as_u64() == Some(step)
                        && event.data["attempt"].as_u64() == Some(2)
                }) {
                    return Err(format!(
                        "event {} repeats attempt 2 for step {step}",
                        event.seq
                    ));
                }
                let first = prior.iter().find(|event| {
                    event.kind == "model/request"
                        && event.seq > turn_seq
                        && event.data["step"].as_u64() == Some(step)
                        && event.data["attempt"].as_u64() == Some(1)
                });
                let Some(first) = first else {
                    return Err(format!("event {} attempt 2 has no attempt 1", event.seq));
                };
                let Some(result) = prior.iter().find(|event| {
                    event.kind == "model/result"
                        && event.data["requestSeq"].as_u64() == Some(first.seq)
                }) else {
                    return Err(format!("event {} attempt 2 follows no result", event.seq));
                };
                if result.data["ok"].as_bool() != Some(false)
                    || result.data["error"]["retryable"].as_bool() != Some(true)
                {
                    return Err(format!(
                        "event {} attempt 2 follows a non-retryable result",
                        event.seq
                    ));
                }
            }
        }
        "model/result" => {
            let data = object(&event.data, event.seq, "model/result data")?;
            exact_keys(
                data,
                &["requestSeq", "ok"],
                &["content", "action", "error", "usage", "reasoning"],
                event.seq,
                "model/result data",
            )?;
            let request_seq = data
                .get("requestSeq")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    format!("event {} requestSeq must be a positive integer", event.seq)
                })?;
            let request = previous_request(prior, request_seq).ok_or_else(|| {
                format!(
                    "event {} references missing request {request_seq}",
                    event.seq
                )
            })?;
            let current_turn_seq = prior
                .iter()
                .rfind(|item| item.kind == "turn/start")
                .map(|item| item.seq)
                .unwrap_or(0);
            if request.seq <= current_turn_seq {
                return Err(format!(
                    "event {} references a request from another turn",
                    event.seq
                ));
            }
            if prior.iter().any(|item| {
                item.kind == "model/result" && item.data["requestSeq"].as_u64() == Some(request_seq)
            }) {
                return Err(format!(
                    "event {} duplicates result for request {request_seq}",
                    event.seq
                ));
            }
            let ok = data
                .get("ok")
                .and_then(Value::as_bool)
                .ok_or_else(|| format!("event {} ok must be a boolean", event.seq))?;
            if ok {
                exact_keys(
                    data,
                    &["requestSeq", "ok", "content", "action"],
                    &["usage", "reasoning"],
                    event.seq,
                    "model/result data",
                )?;
                string_field(data, "content", event.seq, "model/result data")?;
                let action = object(
                    data.get("action").unwrap(),
                    event.seq,
                    "model/result action",
                )?;
                let kind = string_field(action, "kind", event.seq, "model/result action")?;
                match kind {
                    "run" => {
                        exact_keys(
                            action,
                            &["kind", "source", "timeoutMs", "access"],
                            &[],
                            event.seq,
                            "run action",
                        )?;
                        string_field(action, "source", event.seq, "run action")?;
                        positive_u64(action, "timeoutMs", event.seq, "run action")?;
                        let access = object(
                            action.get("access").unwrap(),
                            event.seq,
                            "run action access",
                        )?;
                        exact_keys(
                            access,
                            &["writes", "reason"],
                            &[],
                            event.seq,
                            "run action access",
                        )?;
                        let writes =
                            access
                                .get("writes")
                                .and_then(Value::as_array)
                                .ok_or_else(|| {
                                    format!(
                                    "event {} run action access.writes must be an array of strings",
                                    event.seq
                                )
                                })?;
                        if writes.len() > crate::auth::MAX_WRITE_TARGETS {
                            return Err(format!(
                                "event {} run action access.writes exceeds the 32-target limit",
                                event.seq
                            ));
                        }
                        for entry in writes {
                            if entry.as_str().is_none() {
                                return Err(format!(
                                    "event {} run action access.writes must contain only strings",
                                    event.seq
                                ));
                            }
                        }
                        let reason =
                            string_field(access, "reason", event.seq, "run action access")?;
                        if reason.chars().count() > crate::auth::ACCESS_REASON_CHARS {
                            return Err(format!(
                                "event {} run action access.reason exceeds the 200-character limit",
                                event.seq
                            ));
                        }
                    }
                    "observation" => {
                        exact_keys(
                            action,
                            &["kind", "message"],
                            &[],
                            event.seq,
                            "observation action",
                        )?;
                        string_field(action, "message", event.seq, "observation action")?;
                    }
                    _ => {
                        return Err(format!(
                            "event {} has unsupported action kind {kind:?}",
                            event.seq
                        ))
                    }
                }
                if let Some(usage) = data.get("usage") {
                    validate_usage(usage, event.seq)?;
                }
                if let Some(reasoning) = data.get("reasoning") {
                    validate_reasoning(reasoning, event.seq)?;
                }
            } else {
                exact_keys(
                    data,
                    &["requestSeq", "ok", "error"],
                    &[],
                    event.seq,
                    "model/result data",
                )?;
                let error = object(data.get("error").unwrap(), event.seq, "model/result error")?;
                exact_keys(
                    error,
                    &["kind", "message", "retryable"],
                    &[],
                    event.seq,
                    "model/result error",
                )?;
                let kind = string_field(error, "kind", event.seq, "model/result error")?;
                if !matches!(
                    kind,
                    "configuration"
                        | "transport"
                        | "http"
                        | "protocol"
                        | "timeout"
                        | "cancelled"
                        | "interrupted"
                ) {
                    return Err(format!(
                        "event {} has unsupported model error kind {kind:?}",
                        event.seq
                    ));
                }
                string_field(error, "message", event.seq, "model/result error")?;
                if error["retryable"].as_bool().is_none() {
                    return Err(format!("event {} retryable must be a boolean", event.seq));
                }
                if request.data["attempt"].as_u64() == Some(2)
                    && error["retryable"].as_bool() == Some(true)
                {
                    return Err(format!(
                        "event {} is final attempt but remains retryable",
                        event.seq
                    ));
                }
            }
        }
        "run/start" => {
            let data = object(&event.data, event.seq, "run/start data")?;
            exact_keys(data, &["modelResultSeq"], &[], event.seq, "run/start data")?;
            let result_seq = positive_u64(data, "modelResultSeq", event.seq, "run/start data")?;
            let result = prior
                .iter()
                .find(|item| item.seq == result_seq && item.kind == "model/result")
                .ok_or_else(|| {
                    format!(
                        "event {} references missing model result {result_seq}",
                        event.seq
                    )
                })?;
            let current_turn_seq = prior
                .iter()
                .rfind(|item| item.kind == "turn/start")
                .map(|item| item.seq)
                .unwrap_or(0);
            if result.seq <= current_turn_seq {
                return Err(format!(
                    "event {} references a model result from another turn",
                    event.seq
                ));
            }
            if result.data["ok"].as_bool() != Some(true) || result.data["action"]["kind"] != "run" {
                return Err(format!(
                    "event {} references a model result without a run action",
                    event.seq
                ));
            }
            if prior.iter().any(|item| {
                item.kind == "run/start" && item.data["modelResultSeq"].as_u64() == Some(result_seq)
            }) {
                return Err(format!(
                    "event {} duplicates run start for model result {result_seq}",
                    event.seq
                ));
            }
            if let Some(access) = prior.iter().find(|item| {
                item.kind == "run/access"
                    && item.data["modelResultSeq"].as_u64() == Some(result_seq)
            }) {
                if matches!(
                    access.data["decision"].as_str(),
                    Some("deny" | "cancel" | "unavailable" | "invalid")
                ) {
                    return Err(format!(
                        "event {} starts a run whose access request was not authorized",
                        event.seq
                    ));
                }
            }
        }
        "run/access" => {
            let data = object(&event.data, event.seq, "run/access data")?;
            exact_keys(
                data,
                &["decision", "writes", "reason", "modelResultSeq"],
                &["observation"],
                event.seq,
                "run/access data",
            )?;
            let result_seq = positive_u64(data, "modelResultSeq", event.seq, "run/access data")?;
            let result = prior
                .iter()
                .find(|item| item.seq == result_seq && item.kind == "model/result")
                .ok_or_else(|| {
                    format!(
                        "event {} references missing model result {result_seq}",
                        event.seq
                    )
                })?;
            let current_turn_seq = prior
                .iter()
                .rfind(|item| item.kind == "turn/start")
                .map(|item| item.seq)
                .unwrap_or(0);
            if result.seq <= current_turn_seq {
                return Err(format!(
                    "event {} references a model result from another turn",
                    event.seq
                ));
            }
            if result.data["ok"].as_bool() != Some(true) || result.data["action"]["kind"] != "run" {
                return Err(format!(
                    "event {} references a model result without a run action",
                    event.seq
                ));
            }
            if prior.iter().any(|item| {
                item.kind == "run/access"
                    && item.data["modelResultSeq"].as_u64() == Some(result_seq)
            }) {
                return Err(format!(
                    "event {} duplicates access decision for model result {result_seq}",
                    event.seq
                ));
            }
            if prior.iter().any(|item| {
                item.kind == "run/start" && item.data["modelResultSeq"].as_u64() == Some(result_seq)
            }) {
                return Err(format!(
                    "event {} decides access after the run already started",
                    event.seq
                ));
            }
            let decision = string_field(data, "decision", event.seq, "run/access data")?;
            let blocking = matches!(decision, "deny" | "cancel" | "unavailable" | "invalid");
            if !blocking && !matches!(decision, "allow" | "covered" | "declared") {
                return Err(format!(
                    "event {} has unsupported access decision {decision:?}",
                    event.seq
                ));
            }
            let writes = data
                .get("writes")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    format!(
                        "event {} run/access writes must be an array of strings",
                        event.seq
                    )
                })?;
            if writes.len() > crate::auth::MAX_WRITE_TARGETS {
                return Err(format!(
                    "event {} run/access writes exceeds the 32-target limit",
                    event.seq
                ));
            }
            for entry in writes {
                if entry.as_str().is_none() {
                    return Err(format!(
                        "event {} run/access writes must contain only strings",
                        event.seq
                    ));
                }
            }
            let reason = string_field(data, "reason", event.seq, "run/access data")?;
            if reason.chars().count() > crate::auth::ACCESS_REASON_CHARS {
                return Err(format!(
                    "event {} run/access reason exceeds the 200-character limit",
                    event.seq
                ));
            }
            if blocking && data.get("observation").and_then(Value::as_str).is_none() {
                return Err(format!(
                    "event {} blocking access decision needs an observation",
                    event.seq
                ));
            }
            if let Some(observation) = data.get("observation") {
                if observation.as_str().is_none() {
                    return Err(format!(
                        "event {} run/access observation must be a string",
                        event.seq
                    ));
                }
            }
        }
        "run/result" => {
            let data = object(&event.data, event.seq, "run/result data")?;
            let status = string_field(data, "status", event.seq, "run/result data")?;
            let run_seq = data["runSeq"]
                .as_u64()
                .ok_or_else(|| format!("event {} runSeq must be a positive integer", event.seq))?;
            if run_seq == 0 {
                return Err(format!("event {} runSeq must be positive", event.seq));
            }
            let current_turn_seq = prior
                .iter()
                .rfind(|item| item.kind == "turn/start")
                .map(|item| item.seq)
                .unwrap_or(0);
            if run_seq <= current_turn_seq {
                return Err(format!(
                    "event {} references a run from another turn",
                    event.seq
                ));
            }
            if !prior
                .iter()
                .any(|item| item.kind == "run/start" && item.seq == run_seq)
            {
                return Err(format!(
                    "event {} references missing run {run_seq}",
                    event.seq
                ));
            }
            if prior.iter().any(|item| {
                item.kind == "run/result" && item.data["runSeq"].as_u64() == Some(run_seq)
            }) {
                return Err(format!(
                    "event {} duplicates result for run {run_seq}",
                    event.seq
                ));
            }
            match status {
                "completed" => {
                    exact_keys(
                        data,
                        &["runSeq", "status", "outcome"],
                        &["disposition", "observation"],
                        event.seq,
                        "run/result data",
                    )?;
                    let outcome = object(data.get("outcome").unwrap(), event.seq, "run outcome")?;
                    exact_keys(
                        outcome,
                        &[
                            "ok",
                            "value",
                            "stdout",
                            "error",
                            "termination",
                            "timedOut",
                            "elapsedMs",
                        ],
                        &["answer", "writes", "writesTruncated"],
                        event.seq,
                        "run outcome",
                    )?;
                    if outcome["ok"].as_bool().is_none()
                        || outcome["stdout"].as_str().is_none()
                        || outcome["timedOut"].as_bool().is_none()
                        || outcome["elapsedMs"].as_u64().is_none()
                    {
                        return Err(format!(
                            "event {} run outcome has invalid field types",
                            event.seq
                        ));
                    }
                    if let Some(writes) = outcome.get("writes") {
                        let writes = writes.as_array().ok_or_else(|| {
                            format!("event {} run outcome.writes must be an array", event.seq)
                        })?;
                        if writes.len() > 64 {
                            return Err(format!(
                                "event {} run outcome.writes exceeds the 64-receipt limit",
                                event.seq
                            ));
                        }
                        for receipt in writes {
                            let receipt = object(receipt, event.seq, "write receipt")?;
                            exact_keys(
                                receipt,
                                &[
                                    "path",
                                    "created",
                                    "changed",
                                    "bytesBefore",
                                    "bytesAfter",
                                    "firstChangedLine",
                                ],
                                &[],
                                event.seq,
                                "write receipt",
                            )?;
                            string_field(receipt, "path", event.seq, "write receipt")?;
                            if receipt["created"].as_bool().is_none()
                                || receipt["changed"].as_bool().is_none()
                                || receipt["bytesAfter"].as_u64().is_none()
                                || !receipt["bytesBefore"].is_null()
                                    && receipt["bytesBefore"].as_u64().is_none()
                                || !receipt["firstChangedLine"].is_null()
                                    && receipt["firstChangedLine"].as_u64().is_none()
                            {
                                return Err(format!(
                                    "event {} write receipt has invalid field types",
                                    event.seq
                                ));
                            }
                        }
                    }
                    if outcome
                        .get("writesTruncated")
                        .is_some_and(|value| value.as_bool().is_none())
                    {
                        return Err(format!(
                            "event {} run outcome.writesTruncated must be a boolean",
                            event.seq
                        ));
                    }
                    if let Some(answer) = outcome.get("answer") {
                        if answer.as_str().is_none() && !answer.is_null() {
                            return Err(format!(
                                "event {} legacy run answer must be a string or null",
                                event.seq
                            ));
                        }
                    }
                    if let Some(disposition) = data.get("disposition") {
                        validate_disposition(disposition, event.seq)?;
                        let to = disposition["to"].as_str().unwrap_or_default();
                        if to == "model" {
                            let observation = data.get("observation").ok_or_else(|| {
                                format!(
                                    "event {} model disposition needs an observation",
                                    event.seq
                                )
                            })?;
                            let bytes = serde_json::to_vec(&disposition["facts"]).map_err(|e| {
                                format!("event {} cannot serialize run facts: {e}", event.seq)
                            })?;
                            if bytes.len() > FACTS_CAP {
                                return Err(format!(
                                    "event {} run disposition facts exceed the {FACTS_CAP}-byte limit",
                                    event.seq
                                ));
                            }
                            if observation.as_str().is_none() {
                                return Err(format!(
                                    "event {} run observation must be a string",
                                    event.seq
                                ));
                            }
                        } else if data.get("observation").is_some() {
                            return Err(format!(
                                "event {} user disposition must not have an observation",
                                event.seq
                            ));
                        }
                    } else if data.get("observation").is_none()
                        && outcome.get("answer").and_then(Value::as_str).is_none()
                    {
                        return Err(format!(
                            "event {} completed run needs disposition or observation",
                            event.seq
                        ));
                    }
                    if let Some(observation) = data.get("observation") {
                        if observation.as_str().is_none() {
                            return Err(format!(
                                "event {} run observation must be a string",
                                event.seq
                            ));
                        }
                    }
                    if let Some(error) = outcome.get("error") {
                        if !error.is_null() {
                            let error = object(error, event.seq, "run outcome error")?;
                            exact_keys(
                                error,
                                &["kind", "message"],
                                &[],
                                event.seq,
                                "run outcome error",
                            )?;
                            string_field(error, "kind", event.seq, "run outcome error")?;
                            string_field(error, "message", event.seq, "run outcome error")?;
                        }
                    }
                }
                "outcome_unknown" => {
                    exact_keys(
                        data,
                        &["runSeq", "status", "observation"],
                        &[],
                        event.seq,
                        "run/result data",
                    )?;
                    string_field(data, "observation", event.seq, "run/result data")?;
                }
                _ => {
                    return Err(format!(
                        "event {} has unsupported run status {status:?}",
                        event.seq
                    ))
                }
            }
        }
        "turn/end" => {
            let data = object(&event.data, event.seq, "turn/end data")?;
            exact_keys(
                data,
                &["reason"],
                &["answerRunSeq", "handoffRunSeq"],
                event.seq,
                "turn/end data",
            )?;
            let reason = string_field(data, "reason", event.seq, "turn/end data")?;
            if !matches!(
                reason,
                "answered" | "handed_off" | "step_limit" | "failed" | "cancelled"
            ) {
                return Err(format!(
                    "event {} has unsupported turn end reason {reason:?}",
                    event.seq
                ));
            }
            if reason == "handed_off" {
                let run_seq = data["handoffRunSeq"].as_u64().ok_or_else(|| {
                    format!("event {} handed_off turn needs handoffRunSeq", event.seq)
                })?;
                let run = prior
                    .iter()
                    .find(|item| {
                        item.kind == "run/result"
                            && item.data["runSeq"].as_u64() == Some(run_seq)
                            && item.data["status"] == "completed"
                            && item.data["disposition"]["to"] == "user"
                    })
                    .ok_or_else(|| {
                        format!(
                            "event {} references missing completed user handoff run {run_seq}",
                            event.seq
                        )
                    })?;
                if run.data["disposition"]["message"].as_str().is_none() {
                    return Err(format!("event {} handoff has no message", event.seq));
                }
            } else if reason == "answered" {
                let run_seq = data["answerRunSeq"].as_u64().ok_or_else(|| {
                    format!("event {} answered turn needs answerRunSeq", event.seq)
                })?;
                let run = prior
                    .iter()
                    .find(|item| {
                        item.kind == "run/result"
                            && item.data["runSeq"].as_u64() == Some(run_seq)
                            && item.data["status"] == "completed"
                    })
                    .ok_or_else(|| {
                        format!(
                            "event {} references missing completed answer run {run_seq}",
                            event.seq
                        )
                    })?;
                if run.data["outcome"]["answer"].as_str().is_none() {
                    return Err(format!("event {} answer run has no answer", event.seq));
                }
            } else if data.contains_key("answerRunSeq") || data.contains_key("handoffRunSeq") {
                return Err(format!(
                    "event {} non-terminal-answer turn cannot contain answer references",
                    event.seq
                ));
            }
        }
        _ => {
            return Err(format!(
                "unknown event type {:?} at sequence {}",
                event.kind, event.seq
            ))
        }
    }
    Ok(())
}

pub fn turn_data(
    message: &str,
    system_prompt: &str,
    profile: &ResolvedProfile,
    max_steps: u64,
    default_timeout: u64,
    max_timeout: u64,
) -> Value {
    serde_json::json!({
        "message": message,
        "systemPrompt": system_prompt,
        "profile": profile,
        "limits": {
            "maxSteps": max_steps,
            "defaultRunTimeoutMs": default_timeout,
            "maxRunTimeoutMs": max_timeout
        }
    })
}

/// Projects the journaled conversation into protocol-neutral messages. Failed
/// attempts are excluded so a retry rebuilds byte-identical input. Assistant
/// reasoning captured by the transport is restored here so the active protocol
/// codec can replay it in whatever shape it requires.
pub fn project(events: &[Event], before_seq: u64) -> Vec<crate::llm::NeutralMessage> {
    use crate::llm::{NeutralMessage, ReasoningBlob, Role};
    let start = events
        .iter()
        .rfind(|event| event.kind == "turn/start" && event.seq < before_seq)
        .expect("turn start before request");
    let mut messages = vec![NeutralMessage {
        role: Role::System,
        content: start.data["systemPrompt"]
            .as_str()
            .unwrap_or_default()
            .into(),
        reasoning: None,
    }];
    let reasoning_of = |event: &Event| {
        let data = &event.data["reasoning"];
        (!data.is_null()).then(|| ReasoningBlob {
            text: data["text"].as_str().unwrap_or_default().into(),
            replay: data["replay"].clone(),
        })
    };
    for event in events.iter().filter(|event| event.seq < before_seq) {
        match event.kind.as_str() {
            "turn/start" => messages.push(NeutralMessage {
                role: Role::User,
                content: event.data["message"].as_str().unwrap_or_default().into(),
                reasoning: None,
            }),
            "model/result" if event.data["ok"].as_bool() == Some(true) => {
                if let Some(content) = event.data["content"].as_str() {
                    messages.push(NeutralMessage {
                        role: Role::Assistant,
                        content: content.into(),
                        reasoning: reasoning_of(event),
                    });
                }
                if let Some(message) = event.data["action"]["message"].as_str() {
                    messages.push(NeutralMessage {
                        role: Role::User,
                        content: message.into(),
                        reasoning: None,
                    });
                }
            }
            "run/access" => {
                if let Some(observation) = event.data["observation"].as_str() {
                    messages.push(NeutralMessage {
                        role: Role::User,
                        content: observation.into(),
                        reasoning: None,
                    });
                }
            }
            "run/result" => {
                if let Some(observation) = event.data["observation"].as_str() {
                    messages.push(NeutralMessage {
                        role: Role::User,
                        content: observation.into(),
                        reasoning: None,
                    });
                }
            }
            _ => {}
        }
    }
    messages
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    static STATE_LOCK: Mutex<()> = Mutex::new(());

    fn test_profile() -> ResolvedProfile {
        crate::llm::test_profile(
            "openai-chat-completions",
            "https://example.test",
            "test-model",
        )
    }

    fn test_state_home() -> std::path::PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "terrarium-session-tests-{}-{stamp}",
            std::process::id()
        ))
    }

    fn test_journal() -> (Journal, std::path::PathBuf) {
        let state = test_state_home();
        let root = state.join("root");
        fs::create_dir_all(&root).unwrap();
        std::env::set_var("XDG_STATE_HOME", &state);
        (Journal::create(&root).unwrap(), state)
    }

    fn start(journal: &mut Journal) {
        journal
            .append(
                "turn/start",
                turn_data("test", "system", &test_profile(), 2, 100, 300),
            )
            .unwrap();
    }

    #[test]
    fn appended_events_stamp_non_decreasing_wall_clock_ts() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        let stamps: Vec<u64> = journal
            .events
            .iter()
            .map(|event| event.ts.expect("append stamps ts"))
            .collect();
        assert_eq!(stamps.len(), 2);
        assert!(stamps.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(stamps[0] >= 1_500_000_000_000); // a plausible epoch-ms
        drop(journal);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn legacy_events_without_ts_still_deserialize() {
        let legacy: Event =
            serde_json::from_str(r#"{"type":"turn/start","seq":1,"data":{}}"#).unwrap();
        assert!(legacy.ts.is_none());
        let stamped: Event =
            serde_json::from_str(r#"{"type":"turn/start","seq":1,"ts":1700000000123,"data":{}}"#)
                .unwrap();
        assert_eq!(stamped.ts, Some(1700000000123));
        let bad =
            serde_json::from_str::<Event>(r#"{"type":"turn/start","seq":1,"ts":"x","data":{}}"#);
        assert!(bad.is_err());
    }

    #[test]
    fn projection_keeps_model_facts_but_not_user_handoff_messages() {
        let events = vec![
            Event {
                kind: "turn/start".into(),
                seq: 1,
                ts: None,
                data: serde_json::json!({"message":"inspect","systemPrompt":"system","profile":{"name":"p","protocol":"openai-chat-completions","baseUrl":"https://x","model":"m"},"limits":{"maxSteps":2,"defaultRunTimeoutMs":1,"maxRunTimeoutMs":2}}),
            },
            Event {
                kind: "model/result".into(),
                seq: 2,
                ts: None,
                data: serde_json::json!({"requestSeq":1,"ok":true,"content":"```run\nreturn {}\n```","action":{"kind":"run","source":"return {}\n","timeoutMs":1,"access":{"writes":[],"reason":""}}}),
            },
            Event {
                kind: "run/result".into(),
                seq: 3,
                ts: None,
                data: serde_json::json!({"runSeq":2,"status":"completed","outcome":{"ok":true,"value":null,"stdout":"","error":null,"termination":"returned","timedOut":false,"elapsedMs":1},"disposition":{"to":"model","facts":{"count":2}},"observation":"{\"turn\":1,\"step\":1,\"to\":\"model\",\"facts\":{\"count\":2}}"}),
            },
            Event {
                kind: "run/result".into(),
                seq: 4,
                ts: None,
                data: serde_json::json!({"runSeq":3,"status":"completed","outcome":{"ok":true,"value":null,"stdout":"","error":null,"termination":"returned","timedOut":false,"elapsedMs":1},"disposition":{"to":"user","message":"private handoff"}}),
            },
        ];
        let projection = project(&events, 5);
        assert!(projection
            .iter()
            .any(|message| message.role == crate::llm::Role::User
                && message.content
                    == "{\"turn\":1,\"step\":1,\"to\":\"model\",\"facts\":{\"count\":2}}"));
        assert!(!projection
            .iter()
            .any(|message| message.content == "private handoff"));
    }

    #[test]
    fn completed_successful_run_requires_disposition_or_protocol_observation() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        journal
            .append(
                "model/result",
                serde_json::json!({"requestSeq":2,"ok":true,"content":"```run\nreturn {}\n```","action":{"kind":"run","source":"return {}\n","timeoutMs":1,"access":{"writes":[],"reason":""}}}),
            )
            .unwrap();
        journal
            .append("run/start", serde_json::json!({"modelResultSeq":3}))
            .unwrap();
        let error = journal
            .append(
                "run/result",
                serde_json::json!({"runSeq":4,"status":"completed","outcome":{"ok":true,"value":null,"stdout":"","error":null,"termination":"returned","timedOut":false,"elapsedMs":1}}),
            )
            .unwrap_err();
        assert!(error.contains("disposition"), "{error}");
        drop(journal);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn oversized_model_facts_are_rejected_by_journal_validation() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        journal
            .append(
                "model/result",
                serde_json::json!({"requestSeq":2,"ok":true,"content":"```run\nreturn tagged\n```","action":{"kind":"run","source":"return tagged\n","timeoutMs":1,"access":{"writes":[],"reason":""}}}),
            )
            .unwrap();
        journal
            .append("run/start", serde_json::json!({"modelResultSeq":3}))
            .unwrap();
        let error = journal
            .append(
                "run/result",
                serde_json::json!({
                    "runSeq":4,
                    "status":"completed",
                    "outcome":{"ok":true,"value":null,"stdout":"","error":null,"termination":"returned","timedOut":false,"elapsedMs":1},
                    "disposition":{"to":"model","facts":{"text":"x".repeat(16 * 1024 + 1)}},
                    "observation":"{}"
                }),
            )
            .unwrap_err();
        assert!(error.contains("facts"), "{error}");
        assert!(error.contains("limit"), "{error}");
        drop(journal);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn projection_omits_failed_model_attempts_and_uses_latest_prompt() {
        let events = vec![
            Event {
                kind: "turn/start".into(),
                seq: 1,
                ts: None,
                data: serde_json::json!({"message":"old","systemPrompt":"old-system","profile":{"name":"p","protocol":"openai-chat-completions","baseUrl":"https://x","model":"m"},"limits":{"maxSteps":2,"defaultRunTimeoutMs":1,"maxRunTimeoutMs":2}}),
            },
            Event {
                kind: "turn/end".into(),
                seq: 2,
                ts: None,
                data: serde_json::json!({"reason":"failed"}),
            },
            Event {
                kind: "turn/start".into(),
                seq: 3,
                ts: None,
                data: serde_json::json!({"message":"new","systemPrompt":"new-system","profile":{"name":"p","protocol":"openai-chat-completions","baseUrl":"https://x","model":"m"},"limits":{"maxSteps":2,"defaultRunTimeoutMs":1,"maxRunTimeoutMs":2}}),
            },
            Event {
                kind: "model/request".into(),
                seq: 4,
                ts: None,
                data: serde_json::json!({"step":1,"attempt":1}),
            },
            Event {
                kind: "model/result".into(),
                seq: 5,
                ts: None,
                data: serde_json::json!({"requestSeq":4,"ok":false,"error":{"kind":"transport","message":"x","retryable":true}}),
            },
            Event {
                kind: "model/request".into(),
                seq: 6,
                ts: None,
                data: serde_json::json!({"step":1,"attempt":2}),
            },
        ];
        let projection = project(&events, 6);
        assert_eq!(projection[0].content, "new-system");
        assert_eq!(projection.len(), 3);
    }

    #[test]
    fn successful_results_may_carry_usage_and_reasoning() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        journal
            .append(
                "model/result",
                serde_json::json!({
                    "requestSeq": 2,
                    "ok": true,
                    "content": "```run\nreturn {}\n```",
                    "action": {"kind":"run","source":"return {}\n","timeoutMs":1,"access": {"writes": [], "reason": ""}},
                    "usage": {"inputTokens":10,"outputTokens":5,"cacheReadTokens":4,"cacheWriteTokens":0,"reasoningTokens":2},
                    "reasoning": {"text":"pondered","replay":{"protocol":"openai-chat-completions","field":"reasoning_content"}}
                }),
            )
            .unwrap();
        journal
            .append("model/request", serde_json::json!({"step":2,"attempt":1}))
            .unwrap();
        let projection = project(&journal.events, 5);
        let assistant = projection
            .iter()
            .find(|message| message.role == crate::llm::Role::Assistant)
            .expect("assistant message");
        let reasoning = assistant.reasoning.as_ref().expect("reasoning restored");
        assert_eq!(reasoning.text, "pondered");
        assert_eq!(
            reasoning.replay,
            serde_json::json!({"protocol":"openai-chat-completions","field":"reasoning_content"})
        );
        drop(journal);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn malformed_usage_and_reasoning_are_rejected() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        for data in [
            serde_json::json!({
                "requestSeq": 2, "ok": true, "content": "hi",
                "action": {"kind":"observation","message":"m"},
                "usage": {"inputTokens":1}
            }),
            serde_json::json!({
                "requestSeq": 2, "ok": true, "content": "hi",
                "action": {"kind":"observation","message":"m"},
                "reasoning": {"text":"t"}
            }),
            serde_json::json!({
                "requestSeq": 2, "ok": true, "content": "hi",
                "action": {"kind":"observation","message":"m"},
                "reasoning": {"text":"t","replay":"not-an-object"}
            }),
        ] {
            let result = validate_event(
                &Event {
                    kind: "model/result".into(),
                    seq: 3,
                    ts: None,
                    data,
                },
                &journal.events,
            );
            assert!(result.is_err());
        }
        // The timeout kind itself is legal; only the duplicate-result rule
        // rejects the loop above because they share requestSeq.
        journal
            .append("model/request", serde_json::json!({"step":2,"attempt":1}))
            .unwrap();
        assert!(validate_event(
            &Event {
                kind: "model/result".into(),
                seq: 5,
                ts: None,
                data: serde_json::json!({
                    "requestSeq": 3, "ok": false,
                    "error": {"kind":"timeout","message":"request exceeded the budget","retryable":true}
                }),
            },
            &journal.events,
        )
        .is_ok());
        drop(journal);
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn open_removes_only_an_incomplete_final_line() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        let id = journal.id.clone();
        drop(journal);
        let path = state.join("terrarium/sessions").join(format!("{id}.jsonl"));
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"type":"model/request"}"#).unwrap();
        drop(file);
        let reopened = Journal::open(&id).unwrap();
        assert_eq!(reopened.events.len(), 1);
        // Windows range locks are mandatory: the journal must be dropped before
        // another handle may read the file.
        drop(reopened);
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn malformed_complete_line_is_rejected() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        let id = journal.id.clone();
        drop(journal);
        let path = state.join("terrarium/sessions").join(format!("{id}.jsonl"));
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"not-json\n").unwrap();
        drop(file);
        let error = match Journal::open(&id) {
            Ok(_) => panic!("malformed complete line was accepted"),
            Err(error) => error,
        };
        assert!(error.contains("malformed event"));
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn header_only_session_is_incomplete() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (journal, state) = test_journal();
        let id = journal.id.clone();
        drop(journal);
        let error = match Journal::open(&id) {
            Ok(_) => panic!("header-only session was accepted"),
            Err(error) => error,
        };
        assert!(error.contains("no turn/start event"));
        fs::remove_dir_all(state).unwrap();
    }

    #[test]
    fn duplicate_attempt_two_is_rejected() {
        let _guard = STATE_LOCK.lock().unwrap();
        let (mut journal, state) = test_journal();
        start(&mut journal);
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":1}))
            .unwrap();
        journal
            .append(
                "model/result",
                serde_json::json!({"requestSeq":2,"ok":false,"error":{"kind":"transport","message":"x","retryable":true}}),
            )
            .unwrap();
        journal
            .append("model/request", serde_json::json!({"step":1,"attempt":2}))
            .unwrap();
        let error = journal
            .append("model/request", serde_json::json!({"step":1,"attempt":2}))
            .unwrap_err();
        assert!(error.contains("repeats attempt 2"));
        let id = journal.id.clone();
        drop(journal);
        fs::remove_dir_all(state).unwrap();
        let _ = id;
    }
}
