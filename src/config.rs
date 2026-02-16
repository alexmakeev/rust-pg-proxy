use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub upstream_host: String,
    pub upstream_port: u16,
    pub upstream_user: String,
    pub upstream_password: String,
    pub upstream_database: String,
    pub listen_host: String,
    pub listen_port: u16,
    pub cache_ttl_seconds: u64,
    pub max_cache_entries: u64,
    pub pool_size: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let upstream_host = env::var("UPSTREAM_HOST")
            .map_err(|_| "UPSTREAM_HOST is required".to_string())?;

        let upstream_port = env::var("UPSTREAM_PORT")
            .unwrap_or_else(|_| "5432".to_string())
            .parse::<u16>()
            .map_err(|_| "UPSTREAM_PORT must be a valid port number".to_string())?;

        let upstream_user = env::var("UPSTREAM_USER")
            .map_err(|_| "UPSTREAM_USER is required".to_string())?;

        let upstream_password = env::var("UPSTREAM_PASSWORD")
            .map_err(|_| "UPSTREAM_PASSWORD is required".to_string())?;

        let upstream_database = env::var("UPSTREAM_DATABASE")
            .map_err(|_| "UPSTREAM_DATABASE is required".to_string())?;

        let listen_host = env::var("LISTEN_HOST")
            .unwrap_or_else(|_| "0.0.0.0".to_string());

        let listen_port = env::var("LISTEN_PORT")
            .unwrap_or_else(|_| "5433".to_string())
            .parse::<u16>()
            .map_err(|_| "LISTEN_PORT must be a valid port number".to_string())?;

        let cache_ttl_seconds = env::var("CACHE_TTL_SECONDS")
            .unwrap_or_else(|_| "900".to_string())
            .parse::<u64>()
            .map_err(|_| "CACHE_TTL_SECONDS must be a valid number".to_string())?;

        let max_cache_entries = env::var("MAX_CACHE_ENTRIES")
            .unwrap_or_else(|_| "10000".to_string())
            .parse::<u64>()
            .map_err(|_| "MAX_CACHE_ENTRIES must be a valid number".to_string())?;

        let pool_size = env::var("POOL_SIZE")
            .unwrap_or_else(|_| "5".to_string())
            .parse::<usize>()
            .map_err(|_| "POOL_SIZE must be a valid number".to_string())?;

        // Validate configuration values
        if pool_size == 0 {
            return Err("POOL_SIZE must be greater than 0".to_string());
        }
        if cache_ttl_seconds == 0 {
            return Err("CACHE_TTL_SECONDS must be greater than 0".to_string());
        }

        Ok(Config {
            upstream_host,
            upstream_port,
            upstream_user,
            upstream_password,
            upstream_database,
            listen_host,
            listen_port,
            cache_ttl_seconds,
            max_cache_entries,
            pool_size,
        })
    }
}
