//! The agent registry on SQLite and on Postgres does the same things: one
//! script of calls, and what each answers, on both.
//!
//! Postgres runs when `JAMJET_TEST_POSTGRES_URL` is set, and is required
//! with `JAMJET_REQUIRE_POSTGRES_TESTS=1`.

use jamjet_agents::card::{AgentCapabilities, AuthSpec, AutonomyLevel, Skill};
use jamjet_agents::{
    AgentCard, AgentFilter, AgentRegistry, AgentStatus, PgAgentRegistry, SqliteAgentRegistry,
};
use jamjet_testkit::pg::TestDatabase;
use std::time::Duration;
use uuid::Uuid;

fn card(uri: &str, skill: &str, protocol: &str) -> AgentCard {
    AgentCard {
        id: Uuid::new_v4().to_string(),
        uri: uri.into(),
        name: format!("agent at {uri}"),
        description: "a test agent".into(),
        version: "1.0.0".into(),
        capabilities: AgentCapabilities {
            skills: vec![Skill {
                name: skill.into(),
                description: format!("does {skill}"),
                input_schema: "{}".into(),
                output_schema: "{}".into(),
            }],
            protocols: vec![protocol.into()],
            tools_provided: vec![],
            tools_consumed: vec![],
        },
        autonomy: AutonomyLevel::Guided,
        constraints: None,
        auth: AuthSpec::None,
        latency_class: None,
        cost_class: None,
        reasoning_modes: vec!["react".into()],
        labels: [("team".to_string(), "docs".to_string())].into(),
    }
}

/// What the script saw, in a form both stores can be compared by: URIs for
/// agents, never ids or times, which differ by store.
#[derive(Debug, PartialEq)]
struct Seen {
    second_register_refused: bool,
    card_round_trips: bool,
    status_after_register: AgentStatus,
    heartbeat_before: bool,
    get_unknown: bool,
    by_uri: Option<String>,
    by_unknown_uri: bool,
    update_unknown_refused: bool,
    heartbeat_unknown_ok: bool,
    status_after_update: AgentStatus,
    heartbeat_after: bool,
    heartbeat_moves_updated_at: bool,
    all_newest_first: Vec<String>,
    active: Vec<String>,
    with_skill_search: Vec<String>,
    on_a2a: Vec<String>,
    paused_on_a2a: Vec<String>,
}

fn uris(agents: Vec<jamjet_agents::Agent>) -> Vec<String> {
    agents.into_iter().map(|a| a.card.uri).collect()
}

async fn script(reg: &dyn AgentRegistry) -> Seen {
    let pause = || tokio::time::sleep(Duration::from_millis(5));
    let first = card("jamjet://acme/search", "search", "mcp_client");
    let a = reg.register(first.clone()).await.unwrap();
    pause().await;
    let b = reg
        .register(card("jamjet://acme/writer", "write", "a2a"))
        .await
        .unwrap();
    pause().await;
    let c = reg
        .register(card("jamjet://acme/editor", "edit", "a2a"))
        .await
        .unwrap();
    let second_register_refused = reg
        .register(card("jamjet://acme/search", "other", "a2a"))
        .await
        .is_err();

    let got = reg.get(a).await.unwrap().unwrap();
    let card_round_trips = got.id == a
        && serde_json::to_value(&got.card).unwrap() == serde_json::to_value(&first).unwrap()
        && got.registered_at <= got.updated_at;
    let unknown = Uuid::new_v4();

    reg.update_status(b, AgentStatus::Active).await.unwrap();
    reg.update_status(c, AgentStatus::Paused).await.unwrap();
    let before = reg.get(b).await.unwrap().unwrap();
    pause().await;
    reg.heartbeat(b).await.unwrap();
    let after = reg.get(b).await.unwrap().unwrap();

    let find = |status: Option<AgentStatus>, skill: Option<&str>, protocol: Option<&str>| {
        reg.find(AgentFilter {
            skill: skill.map(str::to_string),
            protocol: protocol.map(str::to_string),
            status,
        })
    };
    Seen {
        second_register_refused,
        card_round_trips,
        status_after_register: got.status,
        heartbeat_before: got.last_heartbeat.is_some(),
        get_unknown: reg.get(unknown).await.unwrap().is_some(),
        by_uri: reg
            .get_by_uri("jamjet://acme/search")
            .await
            .unwrap()
            .filter(|found| found.id == a)
            .map(|found| found.card.uri),
        by_unknown_uri: reg
            .get_by_uri("jamjet://acme/nobody")
            .await
            .unwrap()
            .is_some(),
        update_unknown_refused: reg
            .update_status(unknown, AgentStatus::Active)
            .await
            .is_err(),
        heartbeat_unknown_ok: reg.heartbeat(unknown).await.is_ok(),
        status_after_update: after.status.clone(),
        heartbeat_after: after.last_heartbeat.is_some(),
        heartbeat_moves_updated_at: after.updated_at > before.updated_at,
        all_newest_first: uris(find(None, None, None).await.unwrap()),
        active: uris(find(Some(AgentStatus::Active), None, None).await.unwrap()),
        with_skill_search: uris(find(None, Some("search"), None).await.unwrap()),
        on_a2a: uris(find(None, None, Some("a2a")).await.unwrap()),
        paused_on_a2a: uris(
            find(Some(AgentStatus::Paused), None, Some("a2a"))
                .await
                .unwrap(),
        ),
    }
}

fn expected() -> Seen {
    let s = |v: &[&str]| v.iter().map(|u| format!("jamjet://acme/{u}")).collect();
    Seen {
        second_register_refused: true,
        card_round_trips: true,
        status_after_register: AgentStatus::Registered,
        heartbeat_before: false,
        get_unknown: false,
        by_uri: Some("jamjet://acme/search".into()),
        by_unknown_uri: false,
        update_unknown_refused: true,
        heartbeat_unknown_ok: true,
        status_after_update: AgentStatus::Active,
        heartbeat_after: true,
        heartbeat_moves_updated_at: true,
        all_newest_first: s(&["editor", "writer", "search"]),
        active: s(&["writer"]),
        with_skill_search: s(&["search"]),
        on_a2a: s(&["editor", "writer"]),
        paused_on_a2a: s(&["editor"]),
    }
}

#[tokio::test]
async fn registry_conformance_sqlite_and_postgres() {
    let dir = std::env::temp_dir().join(format!("jamjet_agents_{}", Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let url = format!("sqlite://{}", dir.join("state.db").display());
    // The state migrations make the agents table.
    let state = jamjet_state::SqliteBackend::open(&url).await.unwrap();
    let sqlite = SqliteAgentRegistry::connect(&url).await.unwrap();
    let on_sqlite = script(&sqlite).await;
    drop((sqlite, state));
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(on_sqlite, expected(), "sqlite");

    let Some(db) = TestDatabase::fresh().await else {
        return jamjet_testkit::skip("registry_conformance_sqlite_and_postgres on postgres");
    };
    let postgres = PgAgentRegistry::new(db.pool().await);
    let on_postgres = script(&postgres).await;
    assert_eq!(on_postgres, on_sqlite, "postgres against sqlite");
}
