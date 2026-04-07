mod common;

use std::collections::HashMap;

use ferrex_core::{RecallRequest, StoreRequest};
use ferrex_store::MemoryType;

fn golden_memories() -> Vec<(&'static str, StoreRequest)> {
    vec![
        (
            "e1",
            StoreRequest {
                content: Some(
                    "Deployed API server v2.3 to production, tokio runtime config updated".into(),
                ),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["api-server".into(), "tokio".into()],
                ..default_store()
            },
        ),
        (
            "e2",
            StoreRequest {
                content: Some(
                    "Fixed memory leak in connection pool during load testing".into(),
                ),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["connection-pool".into()],
                ..default_store()
            },
        ),
        (
            "e3",
            StoreRequest {
                content: Some("Migrated database from PostgreSQL 14 to PostgreSQL 16".into()),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["postgresql".into(), "database".into()],
                ..default_store()
            },
        ),
        (
            "e4",
            StoreRequest {
                content: Some("Reviewed pull request for auth middleware refactor".into()),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["auth-middleware".into()],
                ..default_store()
            },
        ),
        (
            "e5",
            StoreRequest {
                content: Some(
                    "CI pipeline broke due to flaky test in integration suite".into(),
                ),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["ci-pipeline".into()],
                ..default_store()
            },
        ),
        (
            "e6",
            StoreRequest {
                content: Some(
                    "Onboarded new team member, walked through the Tokio runtime architecture"
                        .into(),
                ),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["tokio".into()],
                ..default_store()
            },
        ),
        (
            "e7",
            StoreRequest {
                content: Some(
                    "Rolled back deployment after 500 errors spiked in monitoring".into(),
                ),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["api-server".into(), "monitoring".into()],
                ..default_store()
            },
        ),
        (
            "e8",
            StoreRequest {
                content: Some(
                    "Upgraded serde from 1.0.190 to 1.0.200 across all crates".into(),
                ),
                memory_type: Some(MemoryType::Episodic),
                entities: vec!["serde".into()],
                ..default_store()
            },
        ),
        (
            "s1",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("api-server".into()),
                predicate: Some("uses".into()),
                object: Some("tokio runtime".into()),
                entities: vec!["api-server".into(), "tokio".into()],
                ..default_store()
            },
        ),
        (
            "s2",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("api-server".into()),
                predicate: Some("written-in".into()),
                object: Some("Rust".into()),
                entities: vec!["api-server".into()],
                ..default_store()
            },
        ),
        (
            "s3",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("api-server".into()),
                predicate: Some("depends-on".into()),
                object: Some("PostgreSQL".into()),
                entities: vec!["api-server".into(), "postgresql".into()],
                ..default_store()
            },
        ),
        (
            "s4",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("auth-middleware".into()),
                predicate: Some("uses".into()),
                object: Some("JWT tokens".into()),
                entities: vec!["auth-middleware".into()],
                ..default_store()
            },
        ),
        (
            "s5",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("ci-pipeline".into()),
                predicate: Some("runs-on".into()),
                object: Some("GitHub Actions".into()),
                entities: vec!["ci-pipeline".into()],
                ..default_store()
            },
        ),
        (
            "s6",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("PostgreSQL".into()),
                predicate: Some("has-version".into()),
                object: Some("16".into()),
                entities: vec!["postgresql".into()],
                ..default_store()
            },
        ),
        (
            "s7",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("monitoring".into()),
                predicate: Some("uses".into()),
                object: Some("Grafana dashboards".into()),
                entities: vec!["monitoring".into()],
                ..default_store()
            },
        ),
        (
            "s8",
            StoreRequest {
                content: None,
                memory_type: Some(MemoryType::Semantic),
                subject: Some("connection-pool".into()),
                predicate: Some("depends-on".into()),
                object: Some("deadpool".into()),
                entities: vec!["connection-pool".into()],
                ..default_store()
            },
        ),
        (
            "p1",
            StoreRequest {
                content: Some("To deploy: 1) run cargo build --release 2) docker push 3) kubectl apply 4) verify health endpoint".into()),
                memory_type: Some(MemoryType::Procedural),
                entities: vec!["api-server".into(), "deployment".into()],
                ..default_store()
            },
        ),
        (
            "p2",
            StoreRequest {
                content: Some("Database migration workflow: create migration file, test locally with pg_dump, apply via flyway, verify with integration tests".into()),
                memory_type: Some(MemoryType::Procedural),
                entities: vec!["postgresql".into(), "database".into()],
                ..default_store()
            },
        ),
        (
            "p3",
            StoreRequest {
                content: Some("Debugging production issues: check Grafana dashboards first, then grep structured logs, attach debugger as last resort".into()),
                memory_type: Some(MemoryType::Procedural),
                entities: vec!["monitoring".into()],
                ..default_store()
            },
        ),
        (
            "p4",
            StoreRequest {
                content: Some("Adding a new API endpoint: define schema in OpenAPI spec, generate types, implement handler, add integration test, update docs".into()),
                memory_type: Some(MemoryType::Procedural),
                entities: vec!["api-server".into()],
                ..default_store()
            },
        ),
    ]
}

fn default_store() -> StoreRequest {
    StoreRequest {
        content: None,
        memory_type: None,
        subject: None,
        predicate: None,
        object: None,
        confidence: None,
        source: None,
        context: None,
        entities: vec![],
        namespace: None,
        supersedes: None,
    }
}

fn golden_queries() -> Vec<(&'static str, Vec<&'static str>)> {
    vec![
        ("tokio runtime", vec!["e1", "e6", "s1"]),
        ("what does the API server depend on", vec!["s1", "s2", "s3"]),
        ("PostgreSQL version", vec!["s6", "e3", "s3"]),
        ("how to deploy the API", vec!["p1", "e1", "e7"]),
        ("what language is the api server built with", vec!["s2"]),
        ("debugging production problems", vec!["p3"]),
        ("connection pool issues", vec!["e2", "s8"]),
        ("continuous integration problems", vec!["e5", "s5"]),
        ("authentication and authorization", vec!["e4", "s4"]),
        ("monitoring and observability setup", vec!["s7", "p3", "e7"]),
    ]
}

#[tokio::test]
async fn test_golden_set_recall_at_3() {
    let ctx = common::TestContext::with_config(|mut c| {
        // let all 20 memories through without dedup or conflict checks
        c.deduplication.threshold = 1.01;
        c.conflict = ferrex_core::ConflictConfig {
            object_fuzzy_duplicate: 1.0,
            object_fuzzy_update: 1.0,
        };
        c
    })
    .await;

    let mut label_to_id: HashMap<&str, String> = HashMap::new();
    for (label, req) in golden_memories() {
        let resp = ctx
            .service
            .store(req)
            .await
            .unwrap_or_else(|e| panic!("store failed for {label}: {e}"));
        label_to_id.insert(label, resp.id);
    }

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let queries = golden_queries();
    let mut total_recall = 0.0;

    for (query_text, expected_labels) in &queries {
        let req = RecallRequest {
            query: query_text.to_string(),
            types: None,
            entities: None,
            namespace: Some(ctx.namespace.clone()),
            limit: Some(3),
            include_stale: None,
            include_invalidated: None,
            time_range: None,
            validate_ids: None,
        };
        let results = ctx.service.recall(req).await.expect("recall failed");
        let result_ids: Vec<&str> = results.iter().map(|r| r.memory.id.as_str()).collect();

        let expected_ids: Vec<&str> = expected_labels
            .iter()
            .filter_map(|label| label_to_id.get(label).map(String::as_str))
            .collect();

        let hits = expected_ids
            .iter()
            .filter(|id| result_ids.contains(id))
            .count();

        let recall_at_3 = if expected_ids.is_empty() {
            1.0
        } else {
            hits as f64 / expected_ids.len() as f64
        };

        eprintln!(
            "query={query_text:50} recall@3={recall_at_3:.3} hits={hits}/{} scores={:?}",
            expected_ids.len(),
            results
                .iter()
                .map(|r| format!("{:.3}", r.relevance_score))
                .collect::<Vec<_>>(),
        );

        total_recall += recall_at_3;
    }

    let avg_recall = total_recall / queries.len() as f64;
    eprintln!("avg_recall@3 = {avg_recall:.3}");

    assert!(
        avg_recall >= 0.7,
        "average recall@3 = {avg_recall:.3}, expected >= 0.7"
    );
}

#[tokio::test]
async fn test_golden_set_cache_returns_identical_results() {
    let ctx = common::TestContext::with_config(|mut c| {
        c.deduplication.threshold = 1.01;
        c.conflict = ferrex_core::ConflictConfig {
            object_fuzzy_duplicate: 1.0,
            object_fuzzy_update: 1.0,
        };
        c
    })
    .await;

    for (_label, req) in golden_memories() {
        ctx.service.store(req).await.expect("store failed");
    }
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let queries = golden_queries();

    // Cold run
    let mut cold_results: Vec<Vec<String>> = Vec::new();
    for (query_text, _) in &queries {
        let req = RecallRequest {
            query: query_text.to_string(),
            types: None,
            entities: None,
            namespace: Some(ctx.namespace.clone()),
            limit: Some(3),
            include_stale: None,
            include_invalidated: None,
            time_range: None,
            validate_ids: None,
        };
        let results = ctx.service.recall(req).await.expect("recall failed");
        cold_results.push(results.iter().map(|r| r.memory.id.clone()).collect());
    }

    // Warm run (should hit cache)
    let mut warm_results: Vec<Vec<String>> = Vec::new();
    for (query_text, _) in &queries {
        let req = RecallRequest {
            query: query_text.to_string(),
            types: None,
            entities: None,
            namespace: Some(ctx.namespace.clone()),
            limit: Some(3),
            include_stale: None,
            include_invalidated: None,
            time_range: None,
            validate_ids: None,
        };
        let results = ctx.service.recall(req).await.expect("recall failed");
        warm_results.push(results.iter().map(|r| r.memory.id.clone()).collect());
    }

    assert_eq!(
        cold_results, warm_results,
        "cache must return identical results"
    );
}
