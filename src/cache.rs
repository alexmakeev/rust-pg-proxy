use moka::future::Cache;
use std::hash::Hasher;
use std::time::Duration;
use twox_hash::XxHash64;

/// Description of a column in a query result
#[derive(Clone, Debug)]
pub struct ColumnDescription {
    pub name: String,
    pub type_oid: u32,
}

/// Cached query result containing columns and rows
#[derive(Clone, Debug)]
pub struct CachedResult {
    pub columns: Vec<ColumnDescription>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// Query cache using moka async cache
pub struct QueryCache {
    cache: Cache<u64, CachedResult>,
}

impl QueryCache {
    /// Create a new query cache with specified limits
    ///
    /// # Arguments
    /// * `max_entries` - Maximum number of cached queries
    /// * `ttl_seconds` - Time-to-live for cache entries in seconds
    pub fn new(max_entries: u64, ttl_seconds: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_entries)
            .time_to_live(Duration::from_secs(ttl_seconds))
            .build();

        Self { cache }
    }

    /// Retrieve a cached result by key
    pub async fn get(&self, key: u64) -> Option<CachedResult> {
        self.cache.get(&key).await
    }

    /// Insert a query result into the cache
    pub async fn insert(&self, key: u64, result: CachedResult) {
        self.cache.insert(key, result).await;
    }

    /// Invalidate all cached entries
    pub fn invalidate_all(&self) {
        self.cache.invalidate_all();
    }

    /// Get the current number of entries in the cache
    pub fn entry_count(&self) -> u64 {
        self.cache.entry_count()
    }
}

/// Generate a cache key from SQL query string
///
/// Uses xxHash (twox-hash) for fast hashing.
/// Normalizes the SQL by trimming whitespace but preserves case
/// (SQL values are case-sensitive).
pub fn cache_key(sql: &str) -> u64 {
    let normalized = sql.trim();
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(normalized.as_bytes());
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cache_insert_and_get() {
        let cache = QueryCache::new(100, 60);
        let key = cache_key("SELECT 1");

        let result = CachedResult {
            columns: vec![ColumnDescription {
                name: "?column?".to_string(),
                type_oid: 23,
            }],
            rows: vec![vec![Some("1".to_string())]],
        };

        cache.insert(key, result.clone()).await;
        let retrieved = cache.get(key).await;

        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.columns.len(), 1);
        assert_eq!(retrieved.columns[0].name, "?column?");
        assert_eq!(retrieved.rows.len(), 1);
    }

    #[tokio::test]
    async fn test_cache_miss() {
        let cache = QueryCache::new(100, 60);
        let key = cache_key("SELECT 2");

        let retrieved = cache.get(key).await;
        assert!(retrieved.is_none());
    }

    #[tokio::test]
    async fn test_invalidate_all() {
        let cache = QueryCache::new(100, 60);
        let key = cache_key("SELECT 3");

        let result = CachedResult {
            columns: vec![],
            rows: vec![],
        };

        cache.insert(key, result).await;

        // Verify the entry exists before invalidation
        assert!(cache.get(key).await.is_some());

        // Invalidate all entries
        cache.invalidate_all();

        // Verify the entry is gone after invalidation
        assert!(cache.get(key).await.is_none());
    }

    #[test]
    fn test_cache_key_normalization() {
        let key1 = cache_key("SELECT 1");
        let key2 = cache_key("  SELECT 1  ");
        let key3 = cache_key("SELECT 1\n");

        assert_eq!(key1, key2);
        assert_eq!(key1, key3);
    }

    #[test]
    fn test_cache_key_case_sensitivity() {
        let key1 = cache_key("SELECT 'Hello'");
        let key2 = cache_key("SELECT 'hello'");

        // Different cases should produce different keys
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_cache_key_deterministic() {
        let sql = "SELECT * FROM users WHERE id = 1";
        let key1 = cache_key(sql);
        let key2 = cache_key(sql);

        assert_eq!(key1, key2);
    }
}
