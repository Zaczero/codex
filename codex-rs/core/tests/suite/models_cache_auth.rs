//! Catalogs must follow auth and provider changes without leaking default billing tiers.

use std::sync::Arc;

use anyhow::Result;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::CodexAuth;
use codex_login::login_with_api_key;
use codex_models_manager::bundled_models_response;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_unauthorized_retry_keeps_original_account_after_global_switch() -> Result<()> {
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let server = wiremock::MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let mut original: codex_login::AuthDotJson = serde_json::from_value(serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJ1c2VyLTEyMyJ9.c2ln",
            "access_token": "original-token",
            "refresh_token": "original-refresh",
            "account_id": "original-workspace"
        },
        "last_refresh": chrono::Utc::now()
    }))?;
    codex_login::auth::save_named_account(
        home.path(),
        "Original",
        &original,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let auth = config
        .auth_config()
        .load_auth(/*enable_codex_api_key_env*/ false)
        .await?
        .expect("stored original account");
    let original_scope = auth.account_scope().expect("original account scope");
    let codex = test_codex()
        .with_home(home.clone())
        .with_auth(auth)
        .build_with_auto_env(&server)
        .await?;
    original.tokens.as_mut().unwrap().access_token = "refreshed-original-token".into();
    let replacement: codex_login::AuthDotJson = serde_json::from_value(serde_json::json!({
        "auth_mode": "apikey", "OPENAI_API_KEY": "selected-next-token"
    }))?;
    let callback_home = home.clone();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer original-token"))
        .respond_with(move |_: &wiremock::Request| {
            codex_login::save_auth(
                callback_home.path(),
                &original,
                AuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::default(),
            )
            .unwrap();
            codex_login::auth::save_named_account(
                callback_home.path(),
                "Next",
                &replacement,
                AuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::default(),
            )
            .unwrap();
            ResponseTemplate::new(/*s*/ 401)
        })
        .expect(1)
        .mount(&server)
        .await;
    for token in ["refreshed-original-token", "selected-next-token"] {
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .and(header("authorization", format!("Bearer {token}")))
            .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_raw(
                sse(vec![ev_response_created(token), serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {"type": "reasoning", "id": format!("rs_{token}"), "summary": [], "encrypted_content": format!("encrypted-{token}")}
                }), ev_completed(token)]),
                "text/event-stream",
            ))
            .expect(if token == "selected-next-token" { 2 } else { 1 })
            .mount(&server)
            .await;
    }
    codex.submit_turn("First request").await?;
    codex.submit_turn("Next request").await?;
    let next_auth = config
        .auth_config()
        .load_auth(/*enable_codex_api_key_env*/ false)
        .await?
        .expect("selected account");
    let resumed = test_codex()
        .with_auth(next_auth)
        .restart(&server, &codex)
        .await?;
    resumed.submit_turn("After restart").await?;
    let requests = server.received_requests().await.unwrap();
    let request_accounts: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .map(|request| request.headers["authorization"].to_str().unwrap())
        .collect();
    assert_eq!(
        request_accounts,
        vec![
            "Bearer original-token",
            "Bearer refreshed-original-token",
            "Bearer selected-next-token",
            "Bearer selected-next-token",
        ]
    );
    let bodies = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/responses")
        .map(|request| {
            let body = if request
                .headers
                .get("content-encoding")
                .is_some_and(|value| value == "zstd")
            {
                zstd::stream::decode_all(request.body.as_slice()).unwrap()
            } else {
                request.body.clone()
            };
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(bodies[0]["prompt_cache_key"], bodies[1]["prompt_cache_key"]);
    assert_ne!(bodies[1]["prompt_cache_key"], bodies[2]["prompt_cache_key"]);
    assert_eq!(bodies[2]["prompt_cache_key"], bodies[3]["prompt_cache_key"]);
    for body in &bodies[2..] {
        assert!(
            !body["input"]
                .to_string()
                .contains("encrypted-refreshed-original-token")
        );
        assert!(body["input"].to_string().contains("First request"));
    }
    assert!(
        bodies[3]["input"]
            .to_string()
            .contains("encrypted-selected-next-token")
    );
    let rollout = std::fs::read_to_string(codex.session_configured.rollout_path.as_ref().unwrap())?;
    let producer = rollout.lines().filter_map(|line| serde_json::from_str::<codex_history::RolloutItem>(line).ok()).find_map(|item| match item {
        codex_history::RolloutItem::ResponseItem(item) if matches!(&item.item, codex_protocol::models::ResponseItem::Reasoning { encrypted_content: Some(value), .. } if value == "encrypted-refreshed-original-token") => item.metadata.and_then(|metadata| metadata.account_scope),
        _ => None,
    });
    assert_eq!(producer, Some(original_scope));
    assert_eq!(
        codex_login::load_auth_dot_json(
            home.path(),
            AuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::default()
        )?
        .unwrap()
        .openai_api_key,
        Some("selected-next-token".into()),
    );
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auth_and_provider_switches_do_not_reuse_chatgpt_catalog() -> Result<()> {
    let server = wiremock::MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    let mut model = bundled_models_response()?
        .models
        .into_iter()
        .find(|model| model.slug == "gpt-5.4")
        .unwrap();
    model.visibility = ModelVisibility::List;
    model.default_service_tier = Some("priority".into());
    let models_mock = responses::mount_models_once(
        &server,
        ModelsResponse {
            models: vec![model.clone()],
        },
    )
    .await;
    let chatgpt = test_codex()
        .with_home(home.clone())
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model("gpt-5.4")
        .build_with_auto_env(&server)
        .await?;
    assert!(home.path().join("models_cache.json").exists());
    assert_eq!(
        chatgpt
            .thread_manager
            .get_models_manager()
            .get_remote_models()
            .await,
        vec![model.clone()]
    );
    login_with_api_key(
        home.path(),
        "api-key",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    assert!(chatgpt.thread_manager.auth_manager().reload().await);
    let manager = chatgpt.thread_manager.get_models_manager();
    let bundled = bundled_models_response()?.models;
    assert_eq!(manager.get_remote_models().await, bundled);
    assert_eq!(
        manager
            .raw_model_catalog(
                RefreshStrategy::Offline,
                codex_core::test_support::default_http_client_factory()
            )
            .await
            .models,
        bundled
    );
    drop(chatgpt);

    let api = test_codex()
        .with_home(home.clone())
        .with_auth(CodexAuth::from_api_key("api-key"))
        .with_model("gpt-5.4")
        .build_with_auto_env(&server)
        .await?;
    let ordinary = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("ordinary"),
            ev_completed("ordinary"),
        ]),
    )
    .await;
    api.submit_turn("hello").await?;
    assert_eq!(
        ordinary.single_request().body_json().get("service_tier"),
        None
    );

    let explicit = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("explicit"),
            ev_completed("explicit"),
        ]),
    )
    .await;
    api.submit_turn_with_service_tier("hello again", Some("priority"))
        .await?;
    assert_eq!(
        explicit.single_request().body_json()["service_tier"],
        "priority"
    );
    assert_eq!(models_mock.requests().len(), 1);
    drop(api);

    let other_server = wiremock::MockServer::start().await;
    model.default_service_tier = None;
    model.display_name = "Second provider model".into();
    let other_models = responses::mount_models_once(
        &other_server,
        ModelsResponse {
            models: vec![model.clone()],
        },
    )
    .await;
    let next_auth = CodexAuth::create_dummy_chatgpt_auth_for_testing();
    let mut next_tokens = next_auth.get_token_data()?;
    next_tokens.id_token = codex_login::token_data::parse_chatgpt_jwt_claims(
        "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJ1c2VyLTEyMyJ9.c2ln",
    )?;
    codex_login::save_auth(
        home.path(),
        &codex_login::AuthDotJson {
            auth_mode: Some(codex_protocol::auth::AuthMode::Chatgpt),
            openai_api_key: None,
            tokens: Some(next_tokens),
            last_refresh: Some(chrono::Utc::now()),
            agent_identity: None,
            personal_access_token: None,
            bedrock_api_key: None,
            bedrock_access_keys: None,
        },
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    let other = test_codex()
        .with_home(home)
        .with_auth(next_auth)
        .with_model("gpt-5.4")
        .with_config(|config| {
            config.model_provider_id = "second".into();
            config.model_provider.name = "Second".into();
        })
        .build_with_auto_env(&other_server)
        .await?;
    assert_eq!(
        other
            .thread_manager
            .get_models_manager()
            .get_remote_models()
            .await,
        vec![model]
    );
    assert_eq!(other_models.requests().len(), 1);
    Ok(())
}
