use sqlx::Connection;
use sqlx::Row;
use sqlx::SqliteConnection;

use super::Harness;
use super::SessionMove;
use super::rename_session_records;

async fn database() -> SqliteConnection {
    let mut connection = SqliteConnection::connect("sqlite::memory:").await.unwrap();
    sqlx::raw_sql(
        "CREATE TABLE sessions (session_key TEXT PRIMARY KEY, conversation_id TEXT, \
         thread_id TEXT, rollout_path TEXT, created_at TEXT, updated_at TEXT);",
    )
    .execute(&mut connection)
    .await
    .unwrap();
    connection
}

#[tokio::test]
async fn rename_preserves_thread_rollout_and_timestamps() {
    let mut connection = database().await;
    let old_key = Harness::session_key("project:旧名:main");
    let new_key = Harness::session_key("project:新名:main");
    sqlx::query("INSERT INTO sessions VALUES (?, ?, 'thread-1', '/log/one.jsonl', 'created', 'updated')")
        .bind(&old_key).bind("project:旧名:main").execute(&mut connection).await.unwrap();
    rename_session_records(&mut connection, &[SessionMove {
        old_key, new_key: new_key.clone(), conversation_id: "project:新名:main".into(), thread_id: "thread-1".into(),
    }]).await.unwrap();
    let row = sqlx::query("SELECT * FROM sessions").fetch_one(&mut connection).await.unwrap();
    let actual: Vec<String> = ["session_key", "conversation_id", "thread_id", "rollout_path", "created_at", "updated_at"]
        .iter().map(|column| row.try_get(*column).unwrap()).collect();
    assert_eq!(actual, vec![new_key.clone(), "project:新名:main".into(), "thread-1".into(), "/log/one.jsonl".into(), "created".into(), "updated".into()]);
}

#[tokio::test]
async fn collision_does_not_replace_existing_conversation() {
    let mut connection = database().await;
    for (key, conversation) in [("old", "project:a:main"), ("new", "project:b:main")] {
        sqlx::query("INSERT INTO sessions VALUES (?, ?, 'thread', '/log', 'created', 'updated')")
            .bind(key).bind(conversation).execute(&mut connection).await.unwrap();
    }
    let result = rename_session_records(&mut connection, &[SessionMove {
        old_key: "old".into(), new_key: "new".into(), conversation_id: "project:b:main".into(), thread_id: "thread".into(),
    }]).await;
    assert!(result.is_err());
    let rows = sqlx::query("SELECT conversation_id FROM sessions ORDER BY session_key").fetch_all(&mut connection).await.unwrap();
    assert_eq!(rows.iter().map(|row| row.try_get::<String, _>("conversation_id").unwrap()).collect::<Vec<_>>(), vec!["project:b:main", "project:a:main"]);
}
