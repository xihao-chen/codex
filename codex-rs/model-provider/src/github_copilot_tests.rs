use super::*;
use pretty_assertions::assert_eq;

#[test]
fn user_turn_and_tool_continuation_have_distinct_initiators() {
    let user = RequestBody::Json(serde_json::json!({
        "input": [{ "role": "user", "content": "hello" }]
    }));
    let tool = RequestBody::Json(serde_json::json!({
        "input": [{ "type": "function_call_output", "call_id": "call-1", "output": "ok" }]
    }));
    assert!(is_user_turn(Some(&user)));
    assert!(!is_user_turn(Some(&tool)));
}

#[test]
fn catalog_cache_round_trips_without_a_keyring() -> Result<(), Box<dyn std::error::Error>> {
    let home = tempfile::tempdir()?;
    let snapshot = CatalogSnapshot {
        identity: "github-copilot:test-account".to_string(),
        fetched_at: 12345,
        models: vec![model_info_from_slug("test-model")],
    };
    store_catalog(home.path(), &snapshot)?;
    assert_eq!(
        serde_json::to_value(cached_catalog(home.path())?)?,
        serde_json::to_value(Some(snapshot))?
    );
    Ok(())
}

#[test]
fn only_available_responses_models_with_tools_are_listed() {
    let response: CatalogResponse = serde_json::from_value(serde_json::json!({
        "data": [
            {
                "id": "responses-model",
                "name": "Responses Model",
                "model_picker_enabled": true,
                "supported_endpoints": ["/responses", "/chat/completions"],
                "capabilities": {
                    "limits": {
                        "max_context_window_tokens": 100000,
                        "max_prompt_tokens": 90000,
                        "max_output_tokens": 10000
                    },
                    "supports": {
                        "tool_calls": true,
                        "vision": false,
                        "reasoning_effort": ["low", "medium"]
                    }
                }
            },
            {
                "id": "chat-only",
                "name": "Chat Only",
                "model_picker_enabled": true,
                "supported_endpoints": ["/chat/completions"],
                "capabilities": {
                    "limits": { "max_prompt_tokens": 100, "max_output_tokens": 50 },
                    "supports": { "tool_calls": true }
                }
            },
            {
                "id": "disabled",
                "name": "Disabled",
                "model_picker_enabled": true,
                "policy": { "state": "disabled" },
                "supported_endpoints": ["/responses"],
                "capabilities": {
                    "limits": { "max_prompt_tokens": 100, "max_output_tokens": 50 },
                    "supports": { "tool_calls": true }
                }
            }
        ]
    }))
    .unwrap();
    let models = convert_catalog(response);
    assert_eq!(models.len(), 1);
    let model = &models[0];
    assert_eq!(model.slug, "responses-model");
    assert_eq!(model.visibility, ModelVisibility::List);
    assert_eq!(model.context_window, Some(90_000));
    assert_eq!(
        model
            .supported_reasoning_levels
            .iter()
            .map(|preset| preset.effort.clone())
            .collect::<Vec<_>>(),
        vec![ReasoningEffort::Low, ReasoningEffort::Medium]
    );
}
