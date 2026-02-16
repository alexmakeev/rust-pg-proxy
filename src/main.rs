use rust_pg_proxy::cache::QueryCache;
use rust_pg_proxy::config::Config;
use rust_pg_proxy::proxy::{ProxyHandlerFactory, ReadOnlyProxy};
use pgwire::tokio::process_socket;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing with environment-based filtering
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Load environment variables from .env file if present
    dotenvy::dotenv().ok();

    // Load configuration
    let config = Config::from_env().map_err(|e| {
        error!("Failed to load configuration: {}", e);
        format!("Configuration error: {}", e)
    })?;

    info!("Starting PostgreSQL Read-Only Proxy");
    info!("Upstream: {}:{}/{}", config.upstream_host, config.upstream_port, config.upstream_database);
    info!("Listening on: {}:{}", config.listen_host, config.listen_port);
    info!("Cache settings: max_entries={}, ttl={}s", config.max_cache_entries, config.cache_ttl_seconds);
    info!("Connection pool size: {}", config.pool_size);

    // Configure tokio-postgres connection
    let mut pg_config = tokio_postgres::Config::new();
    pg_config
        .host(&config.upstream_host)
        .port(config.upstream_port)
        .user(&config.upstream_user)
        .password(&config.upstream_password)
        .dbname(&config.upstream_database);

    // Create deadpool connection pool
    let pool = deadpool_postgres::Pool::builder(
        deadpool_postgres::Manager::new(pg_config, tokio_postgres::NoTls),
    )
    .max_size(config.pool_size)
    .wait_timeout(Some(std::time::Duration::from_secs(5)))
    .create_timeout(Some(std::time::Duration::from_secs(5)))
    .recycle_timeout(Some(std::time::Duration::from_secs(5)))
    .build()
    .map_err(|e| {
        error!("Failed to create connection pool: {}", e);
        format!("Pool creation error: {}", e)
    })?;

    // Test upstream connectivity
    info!("Testing upstream connection...");
    let test_client = pool.get().await.map_err(|e| {
        error!("Failed to connect to upstream database: {}", e);
        format!("Upstream connection error: {}", e)
    })?;

    test_client.query("SELECT 1", &[]).await.map_err(|e| {
        error!("Failed to execute test query on upstream: {}", e);
        format!("Upstream test query error: {}", e)
    })?;

    info!("Successfully connected to upstream database");

    // Create query cache
    let cache = Arc::new(QueryCache::new(
        config.max_cache_entries,
        config.cache_ttl_seconds,
    ));

    // Create the read-only proxy
    let proxy = Arc::new(ReadOnlyProxy::new(pool, cache));
    let handler_factory = ProxyHandlerFactory::new(proxy);

    // Bind TCP listener
    let listen_addr = format!("{}:{}", config.listen_host, config.listen_port);
    let listener = TcpListener::bind(&listen_addr).await.map_err(|e| {
        error!("Failed to bind to {}: {}", listen_addr, e);
        format!("Bind error: {}", e)
    })?;

    info!("Proxy server listening on {}", listen_addr);

    // Setup graceful shutdown signal handling
    let shutdown_signal = async {
        let ctrl_c = async {
            match signal::ctrl_c().await {
                Ok(_) => {},
                Err(e) => {
                    error!("Failed to install Ctrl+C handler: {}", e);
                }
            }
        };

        #[cfg(unix)]
        let terminate = async {
            match signal::unix::signal(signal::unix::SignalKind::terminate()) {
                Ok(mut stream) => {
                    stream.recv().await;
                }
                Err(e) => {
                    error!("Failed to install SIGTERM handler: {}", e);
                }
            }
        };

        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {
                info!("Received Ctrl+C");
            }
            _ = terminate => {
                info!("Received SIGTERM");
            }
        }
    };

    // Main server loop
    tokio::select! {
        result = async {
            loop {
                match listener.accept().await {
                    Ok((socket, addr)) => {
                        info!("New connection from {}", addr);
                        let handler = ProxyHandlerFactory::new(handler_factory.proxy.clone());

                        tokio::spawn(async move {
                            // process_socket signature: (socket, tls_acceptor, handler_factory)
                            // We don't use TLS, so pass None for the second parameter
                            if let Err(e) = process_socket(socket, None, handler).await {
                                warn!("Error processing connection from {}: {}", addr, e);
                            } else {
                                info!("Connection from {} closed", addr);
                            }
                        });
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {}", e);
                    }
                }
            }
        } => {
            result
        }
        _ = shutdown_signal => {
            info!("Shutting down gracefully...");
        }
    }

    info!("Proxy server stopped");
    Ok(())
}
