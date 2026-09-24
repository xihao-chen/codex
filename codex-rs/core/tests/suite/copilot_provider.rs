use anyhow::Context;
use anyhow::Result;
use codex_model_provider_info::GITHUB_COPILOT_BASE_URL;
use codex_model_provider_info::ModelProviderInfo;
use core_test_support::test_codex::test_codex;
use wiremock::MockServer;

#[tokio::test]
async fn copilot_without_a_verified_catalog_fails_thread_start() -> Result<()> {
    let server = MockServer::start().await;
    let mut builder = test_codex().with_config(|config| {
        config.model = None;
        config.model_catalog = None;
        config.model_provider = ModelProviderInfo {
            name: "GitHub Copilot".to_string(),
            base_url: Some(GITHUB_COPILOT_BASE_URL.to_string()),
            ..Default::default()
        };
    });
    let error = builder
        .build_with_auto_env(&server)
        .await
        .err()
        .context("thread startup should fail without a validated Copilot catalog")?;
    assert!(
        error.to_string().contains("no validated Responses models"),
        "{error}"
    );
    Ok(())
}
