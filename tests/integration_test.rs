use deadpool_postgres::Manager;
use rust_pg_proxy::cache::QueryCache;
use rust_pg_proxy::proxy::{ProxyHandlerFactory, ReadOnlyProxy};
use pgwire::tokio::process_socket;
use std::env;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_postgres::{Config as PgConfig, NoTls};

/// Helper function to start a test proxy server
/// Returns the port number on which the proxy is listening
async fn start_test_proxy() -> Result<u16, Box<dyn std::error::Error>> {
    // Read upstream configuration from environment variables
    let upstream_host = env::var("UPSTREAM_HOST")
        .unwrap_or_else(|_| "localhost".to_string());
    let upstream_port = env::var("UPSTREAM_PORT")
        .unwrap_or_else(|_| "5432".to_string())
        .parse::<u16>()?;
    let upstream_user = env::var("UPSTREAM_USER")
        .unwrap_or_else(|_| "postgres".to_string());
    let upstream_password = env::var("UPSTREAM_PASSWORD")
        .unwrap_or_else(|_| "".to_string());
    let upstream_database = env::var("UPSTREAM_DATABASE")
        .unwrap_or_else(|_| "postgres".to_string());

    // Configure tokio-postgres connection
    let mut pg_config = PgConfig::new();
    pg_config
        .host(&upstream_host)
        .port(upstream_port)
        .user(&upstream_user)
        .password(&upstream_password)
        .dbname(&upstream_database);

    // Create deadpool connection pool
    let pool = deadpool_postgres::Pool::builder(Manager::new(pg_config, NoTls))
        .max_size(5)
        .build()?;

    // Test upstream connectivity
    let test_client = pool.get().await?;
    test_client.query("SELECT 1", &[]).await?;

    // Create query cache (15 minute TTL, 10000 max entries)
    let cache = Arc::new(QueryCache::new(10000, 900));

    // Create the read-only proxy
    let proxy = Arc::new(ReadOnlyProxy::new(pool, cache));
    let handler_factory = ProxyHandlerFactory::new(proxy.clone());

    // Bind to a random available port
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;
    let port = local_addr.port();

    // Spawn the server loop
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((socket, _addr)) => {
                    let handler = ProxyHandlerFactory::new(proxy.clone());
                    tokio::spawn(async move {
                        let _ = process_socket(socket, None, handler).await;
                    });
                }
                Err(_) => break,
            }
        }
    });

    // Give the server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    Ok(port)
}

/// Helper to check if test environment is configured
fn check_test_env() -> bool {
    env::var("UPSTREAM_HOST").is_ok()
}

#[tokio::test]
#[ignore]
async fn test_select_basic() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test SELECT 1
    let rows = client.query("SELECT 1", &[]).await.expect("SELECT 1 failed");
    assert_eq!(rows.len(), 1);
    let value: i32 = rows[0].get(0);
    assert_eq!(value, 1);
}

#[tokio::test]
#[ignore]
async fn test_select_version() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test SELECT version()
    let rows = client
        .query("SELECT version()", &[])
        .await
        .expect("SELECT version() failed");
    assert_eq!(rows.len(), 1);
    let version: String = rows[0].get(0);
    assert!(!version.is_empty());
    assert!(version.to_lowercase().contains("postgresql"));
}

#[tokio::test]
#[ignore]
async fn test_select_current_database() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test SELECT current_database()
    let rows = client
        .query("SELECT current_database()", &[])
        .await
        .expect("SELECT current_database() failed");
    assert_eq!(rows.len(), 1);
    let db: String = rows[0].get(0);
    assert!(!db.is_empty());
}

#[tokio::test]
#[ignore]
async fn test_select_from_information_schema() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test SELECT from information_schema.tables
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables LIMIT 5",
            &[],
        )
        .await
        .expect("SELECT from information_schema failed");

    // Should get some results (at least system tables)
    assert!(!rows.is_empty());
    assert!(rows.len() <= 5);
}

#[tokio::test]
#[ignore]
async fn test_show_command() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test SHOW server_version
    let rows = client
        .query("SHOW server_version", &[])
        .await
        .expect("SHOW server_version failed");
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
#[ignore]
async fn test_create_table_blocked() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test CREATE TABLE - should be blocked with SQLSTATE 25006
    let result = client
        .query("CREATE TABLE test_blocked (id int)", &[])
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let db_err = err.as_db_error().expect("Expected database error");
    assert_eq!(db_err.code().code(), "25006");
}

#[tokio::test]
#[ignore]
async fn test_insert_blocked() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test INSERT - should be blocked with SQLSTATE 25006
    let result = client
        .query("INSERT INTO nonexistent (id) VALUES (1)", &[])
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let db_err = err.as_db_error().expect("Expected database error");
    assert_eq!(db_err.code().code(), "25006");
}

#[tokio::test]
#[ignore]
async fn test_drop_table_blocked() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test DROP TABLE - should be blocked with SQLSTATE 25006
    let result = client
        .query("DROP TABLE IF EXISTS nonexistent", &[])
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let db_err = err.as_db_error().expect("Expected database error");
    assert_eq!(db_err.code().code(), "25006");
}

#[tokio::test]
#[ignore]
async fn test_caching_same_query_twice() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Execute the same query twice - should use cache on second call
    let query = "SELECT table_name FROM information_schema.tables LIMIT 3";

    let rows1 = client.query(query, &[]).await.expect("First query failed");
    let rows2 = client.query(query, &[]).await.expect("Second query failed");

    // Both should return the same data
    assert_eq!(rows1.len(), rows2.len());

    for (row1, row2) in rows1.iter().zip(rows2.iter()) {
        let name1: String = row1.get(0);
        let name2: String = row2.get(0);
        assert_eq!(name1, name2);
    }
}

#[tokio::test]
#[ignore]
async fn test_cache_reset() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Execute a cacheable query to populate cache
    let _ = client
        .query("SELECT 12345", &[])
        .await
        .expect("Query failed");

    // Execute cache reset
    let result = client
        .query("SELECT rustproxy_cache_reset()", &[])
        .await;

    // Should succeed
    assert!(result.is_ok());
    let rows = result.unwrap();
    assert_eq!(rows.len(), 1);
}

#[tokio::test]
#[ignore]
async fn test_multi_statement_blocking() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test multi-statement with mixed read and write - should be blocked
    let result = client
        .query("SELECT 1; DROP TABLE users", &[])
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let db_err = err.as_db_error().expect("Expected database error");
    assert_eq!(db_err.code().code(), "25006");
}

#[tokio::test]
#[ignore]
async fn test_update_blocked() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test UPDATE - should be blocked with SQLSTATE 25006
    let result = client
        .query("UPDATE nonexistent SET id = 1", &[])
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let db_err = err.as_db_error().expect("Expected database error");
    assert_eq!(db_err.code().code(), "25006");
}

#[tokio::test]
#[ignore]
async fn test_delete_blocked() {
    if !check_test_env() {
        eprintln!("Skipping test: UPSTREAM_HOST not set");
        return;
    }

    let port = start_test_proxy().await.expect("Failed to start proxy");
    let connection_string = format!("host=127.0.0.1 port={} user=test dbname=test", port);

    let (client, connection) = tokio_postgres::connect(&connection_string, NoTls)
        .await
        .expect("Failed to connect to proxy");

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("Connection error: {}", e);
        }
    });

    // Test DELETE - should be blocked with SQLSTATE 25006
    let result = client
        .query("DELETE FROM nonexistent", &[])
        .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    let db_err = err.as_db_error().expect("Expected database error");
    assert_eq!(db_err.code().code(), "25006");
}
