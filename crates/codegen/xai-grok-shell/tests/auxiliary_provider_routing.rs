use indexmap::IndexMap;
use xai_grok_sampling_types::ModelProvider;
use xai_grok_shell::agent::config::{
    EndpointsConfig, ModelEntry, resolve_aux_model_sampling_config,
};

fn catalog_with(entry: ModelEntry) -> IndexMap<String, ModelEntry> {
    let mut models = IndexMap::new();
    models.insert("helper".to_owned(), entry);
    models
}

#[test]
fn custom_auxiliary_endpoint_requires_its_own_credential() {
    let endpoints = EndpointsConfig::default();
    let mut entry = ModelEntry::fallback("custom-helper", &endpoints);
    entry.info.provider = ModelProvider::Xai;
    entry.info.base_url = "https://custom.example.test/v1".to_owned();

    let rejected = resolve_aux_model_sampling_config(
        "helper",
        &catalog_with(entry.clone()),
        &endpoints,
        Some("xai-session-secret"),
        false,
        None,
        None,
    );
    assert!(
        rejected.is_none(),
        "an xAI session credential must not be sent to a custom endpoint"
    );

    entry.api_key = Some("custom-endpoint-key".to_owned());
    let resolved = resolve_aux_model_sampling_config(
        "helper",
        &catalog_with(entry),
        &endpoints,
        Some("xai-session-secret"),
        false,
        None,
        None,
    )
    .expect("endpoint-owned credentials should enable the custom helper");
    assert_eq!(resolved.base_url, "https://custom.example.test/v1");
    assert_eq!(resolved.api_key.as_deref(), Some("custom-endpoint-key"));
}

#[test]
fn first_party_auxiliary_endpoint_can_use_xai_session_auth() {
    let endpoints = EndpointsConfig::default();
    let entry = ModelEntry::fallback("first-party-helper", &endpoints);
    let resolved = resolve_aux_model_sampling_config(
        "helper",
        &catalog_with(entry),
        &endpoints,
        Some("xai-session-key"),
        false,
        None,
        None,
    )
    .expect("first-party xAI helper should resolve with xAI session auth");

    assert_eq!(resolved.provider, ModelProvider::Xai);
    assert_eq!(resolved.api_key.as_deref(), Some("xai-session-key"));
}

#[test]
fn custom_codex_endpoint_cannot_inherit_chatgpt_oauth() {
    let endpoints = EndpointsConfig::default();
    let mut entry = ModelEntry::fallback("custom-codex-helper", &endpoints);
    entry.info.provider = ModelProvider::Codex;
    entry.info.base_url = "https://codex-proxy.example.test/v1".to_owned();

    let rejected = resolve_aux_model_sampling_config(
        "helper",
        &catalog_with(entry.clone()),
        &endpoints,
        None,
        false,
        None,
        None,
    );
    assert!(
        rejected.is_none(),
        "ChatGPT OAuth must not be loaded for an arbitrary Codex endpoint"
    );

    entry.api_key = Some("custom-codex-key".to_owned());
    let resolved = resolve_aux_model_sampling_config(
        "helper",
        &catalog_with(entry),
        &endpoints,
        None,
        false,
        None,
        None,
    )
    .expect("endpoint-owned key should enable a custom Codex helper");
    assert_eq!(resolved.base_url, "https://codex-proxy.example.test/v1");
    assert_eq!(resolved.api_key.as_deref(), Some("custom-codex-key"));
}
