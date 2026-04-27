//! Integration test that exercises the reconciler against a real running
//! PostgreSQL server.
//!
//! Gated behind `#[ignore]` so default `cargo test` doesn't need a database;
//! run with:
//!
//! ```sh
//! COMPUTEZA_POSTGRES_TEST_URL='postgres://postgres:secret@localhost:5432/postgres' \
//!   cargo test -p computeza-reconciler-postgres -- --ignored
//! ```
//!
//! The URL is parsed into the same `ServerEndpoint` + password that the
//! reconciler takes in production — the test is end-to-end realistic.

use computeza_core::{reconciler::Context, NoOpDriver, Reconciler};
use computeza_reconciler_postgres::{
    DatabaseSpec, PostgresReconciler, PostgresSpec, ServerEndpoint,
};
use secrecy::{ExposeSecret, SecretString};

fn endpoint_and_password_from_env() -> Option<(ServerEndpoint, SecretString)> {
    let raw = std::env::var("COMPUTEZA_POSTGRES_TEST_URL").ok()?;
    let url = url::Url::parse(&raw).expect("COMPUTEZA_POSTGRES_TEST_URL is not a valid URL");

    let host = url.host_str().expect("URL must have a host").to_string();
    let port = url.port().unwrap_or(5432);
    let username = url.username().to_string();
    let password = url
        .password()
        .expect("URL must include a password")
        .to_string();

    Some((
        ServerEndpoint {
            host,
            port,
            superuser: username,
            sslmode: None,
        },
        SecretString::from(password),
    ))
}

#[tokio::test]
#[ignore = "requires COMPUTEZA_POSTGRES_TEST_URL pointing at a running PostgreSQL"]
async fn end_to_end_create_then_drop() {
    let (endpoint, password) = endpoint_and_password_from_env()
        .expect("set COMPUTEZA_POSTGRES_TEST_URL to run this test");

    // The spec and the reconciler both carry the endpoint+password today;
    // the reconciler ignores the spec's copy in observe/apply, but a future
    // refactor may have plan() validate that they match. Keep them aligned.
    let spec_endpoint = endpoint.clone();
    let spec_password = SecretString::from(password.expose_secret().to_string());

    let reconciler: PostgresReconciler<NoOpDriver> =
        PostgresReconciler::new(endpoint, password);
    let ctx = Context::default();
    let driver = NoOpDriver;

    // Use a deterministic but unique-per-run database name so concurrent
    // runs against the same server don't collide.
    let db_name = format!(
        "computeza_test_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );

    // --- Create ---
    let spec_create = PostgresSpec {
        endpoint: spec_endpoint,
        superuser_password: spec_password,
        databases: vec![DatabaseSpec {
            name: db_name.clone(),
            owner: None,
            encoding: None,
        }],
        prune: false,
    };

    let observed = reconciler
        .observe(&ctx)
        .await
        .expect("observe must succeed against a real server");
    assert!(
        !observed.last_observe_failed,
        "first observe should succeed, status: {observed:?}"
    );
    assert!(
        observed.server_version.is_some(),
        "server_version should be populated"
    );

    let plan = reconciler.plan(&spec_create, &observed).await.unwrap();
    assert!(
        !plan.is_empty(),
        "plan should contain a Create for {db_name}"
    );

    let outcome = reconciler.apply(&ctx, plan, &driver).await.unwrap();
    assert!(outcome.changed, "apply should report changed=true");

    // --- Verify create landed ---
    let after_create = reconciler.observe(&ctx).await.unwrap();
    assert!(
        after_create.databases.iter().any(|d| d == &db_name),
        "after create, observe should list {db_name}: got {:?}",
        after_create.databases
    );

    // --- Drop (via prune) ---
    let spec_drop = PostgresSpec {
        databases: vec![],
        prune: true,
        ..spec_create
    };
    let plan = reconciler.plan(&spec_drop, &after_create).await.unwrap();
    let outcome = reconciler.apply(&ctx, plan, &driver).await.unwrap();
    assert!(outcome.changed, "drop apply should report changed=true");

    let after_drop = reconciler.observe(&ctx).await.unwrap();
    assert!(
        !after_drop.databases.iter().any(|d| d == &db_name),
        "after drop, observe should NOT list {db_name}: got {:?}",
        after_drop.databases
    );
}
