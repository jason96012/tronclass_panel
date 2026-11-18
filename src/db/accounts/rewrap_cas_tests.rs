//! Synthetic SQLite rows only; no account credentials or external services.
use super::*;
use sqlx::sqlite::SqlitePoolOptions;

#[tokio::test]
async fn rewrap_compare_and_swap_does_not_overwrite_newer_credentials() {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::query("CREATE TABLE accounts (id TEXT PRIMARY KEY, password TEXT NOT NULL, session_cookie TEXT NOT NULL)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO accounts (id, password, session_cookie) VALUES (?, ?, ?)")
        .bind("mock").bind("old-password").bind("old-cookie").execute(&pool).await.unwrap();
    sqlx::query("UPDATE accounts SET session_cookie = ? WHERE id = ?")
        .bind("new-cookie").bind("mock").execute(&pool).await.unwrap();
    let result = sqlx::query(REWRAP_COMPARE_AND_SWAP_SQL)
        .bind("rewrapped-password").bind("rewrapped-cookie").bind("mock")
        .bind("old-password").bind("old-cookie").execute(&pool).await.unwrap();
    assert_eq!(result.rows_affected(), 0);
    let row: (String, String) = sqlx::query_as("SELECT password, session_cookie FROM accounts WHERE id = ?")
        .bind("mock").fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("old-password".to_string(), "new-cookie".to_string()));
}

#[tokio::test]
async fn rewrap_compare_and_swap_updates_unchanged_snapshot() {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::query("CREATE TABLE accounts (id TEXT PRIMARY KEY, password TEXT NOT NULL, session_cookie TEXT NOT NULL)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO accounts (id, password, session_cookie) VALUES (?, ?, ?)")
        .bind("mock").bind("old-password").bind("old-cookie").execute(&pool).await.unwrap();
    let result = sqlx::query(REWRAP_COMPARE_AND_SWAP_SQL)
        .bind("rewrapped-password").bind("rewrapped-cookie").bind("mock")
        .bind("old-password").bind("old-cookie").execute(&pool).await.unwrap();
    assert_eq!(result.rows_affected(), 1);
    let row: (String, String) = sqlx::query_as("SELECT password, session_cookie FROM accounts WHERE id = ?")
        .bind("mock").fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("rewrapped-password".to_string(), "rewrapped-cookie".to_string()));
}

#[tokio::test]
async fn rewrap_compare_and_swap_does_not_overwrite_newer_password() {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::query("CREATE TABLE accounts (id TEXT PRIMARY KEY, password TEXT NOT NULL, session_cookie TEXT NOT NULL)")
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO accounts (id, password, session_cookie) VALUES (?, ?, ?)")
        .bind("mock").bind("new-password").bind("old-cookie").execute(&pool).await.unwrap();
    let result = sqlx::query(REWRAP_COMPARE_AND_SWAP_SQL)
        .bind("rewrapped-password").bind("rewrapped-cookie").bind("mock")
        .bind("old-password").bind("old-cookie").execute(&pool).await.unwrap();
    assert_eq!(result.rows_affected(), 0);
    let row: (String, String) = sqlx::query_as("SELECT password, session_cookie FROM accounts WHERE id = ?")
        .bind("mock").fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("new-password".to_string(), "old-cookie".to_string()));
}
