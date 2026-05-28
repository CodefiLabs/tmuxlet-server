use super::{ApiBackend, BackendError, DispatchResult};
use crate::env::Env;

/// Split a base_url into (scheme, host, port, path).
pub fn split_url(base_url: &str) -> (String, String, u16, String) {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("http", base_url));
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(default_port)),
        None => (authority.to_string(), default_port),
    };
    (scheme.to_string(), host, port, path.to_string())
}

/// Merge order: caller payload -> shallow-merge extra_body (overrides caller) ->
/// pin model last (neither caller nor extra_body can override). `stream` is
/// dropped (V1 always requests non-streamed upstream).
pub fn build_body(
    caller: &serde_json::Value,
    extra: &serde_json::Value,
    model: &str,
) -> serde_json::Value {
    let mut body = caller.clone();
    if let Some(obj) = body.as_object_mut() {
        if let Some(ex) = extra.as_object() {
            for (k, v) in ex {
                obj.insert(k.clone(), v.clone());
            }
        }
        obj.insert("model".into(), serde_json::json!(model));
        obj.remove("stream");
    }
    body
}

pub fn extract_content(name: &str, resp: &str) -> Result<String, BackendError> {
    let v: serde_json::Value =
        serde_json::from_str(resp).map_err(|e| BackendError::Parse(name.into(), e.to_string()))?;
    v.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| BackendError::Parse(name.into(), "no choices[0].message.content".into()))
}

/// Forward to an OpenAI-compatible HTTP upstream. STUB — implemented in Task 8
/// (see docs/superpowers/plans/2026-05-28-tmuxlet-server.md).
pub fn dispatch(
    _b: &ApiBackend,
    _raw_messages: &serde_json::Value,
    _env: &Env,
) -> Result<DispatchResult, BackendError> {
    todo!("Task 8: split_url + build_body -> http_client::post_json -> extract_content")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_extra_body_then_pins_model() {
        let caller = serde_json::json!({"model":"caller-wins?","messages":[{"role":"user","content":"hi"}],"think":false});
        let extra = serde_json::json!({"think":true,"foo":1});
        let body = build_body(&caller, &extra, "pinned-model");
        assert_eq!(body["model"], serde_json::json!("pinned-model"));
        assert_eq!(body["think"], serde_json::json!(true));
        assert_eq!(body["foo"], serde_json::json!(1));
        assert!(body["messages"].is_array());
    }

    #[test]
    fn splits_base_url() {
        assert_eq!(
            split_url("http://127.0.0.1:11434/v1"),
            ("http".into(), "127.0.0.1".into(), 11434, "/v1".into())
        );
        assert_eq!(
            split_url("https://openrouter.ai/api/v1"),
            (
                "https".into(),
                "openrouter.ai".into(),
                443,
                "/api/v1".into()
            )
        );
    }

    #[test]
    fn extracts_content_from_completion() {
        let r = r#"{"choices":[{"message":{"content":"hello"}}]}"#;
        assert_eq!(extract_content("api", r).unwrap(), "hello");
    }
}
