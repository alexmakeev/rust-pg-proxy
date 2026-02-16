# Rust PostgreSQL Read-Only Proxy — Claude Code Instructions

## Project Overview

**rust-pg-proxy** is a lightweight PostgreSQL proxy written in Rust that enforces read-only access at the SQL parsing level. It sits between clients and a production PostgreSQL database, blocking all write operations while caching read-only query results.

**Use case:** Grant read-only database access to monitoring dashboards, BI tools, or third-party integrations without:
- Sharing production admin credentials
- Creating additional database users (may be impossible on some hosting platforms)
- Exposing full database privileges

**Key constraint:** Timeweb hosting (production database) closes idle connections after 15 minutes. The proxy must handle reconnection gracefully via deadpool-postgres connection pooling.

## Tech Stack

| Component | Crate | Version | Purpose |
|-----------|-------|---------|---------|
| **Async runtime** | tokio | 1.x | Event loop, spawning connection handlers |
| **PostgreSQL wire protocol** | pgwire | 0.25 | Implements PostgreSQL server protocol (accepts client connections) |
| **SQL parsing** | sqlparser | 0.59 | Parses SQL into AST, classifies statements (read/write) |
| **Query caching** | moka | 0.12 | LFU admission + LRU eviction, built-in TTL, thread-safe |
| **Upstream pooling** | deadpool-postgres | 0.14 | Connection pool to production database |
| **Postgres client** | tokio-postgres | 0.7 | Low-level async PostgreSQL client |
| **Hashing** | twox-hash | 2.0 | Fast non-cryptographic hash (cache key generation) |
| **Logging** | tracing + tracing-subscriber | 0.1 + 0.3 | Structured logging with environment filtering |
| **Env loading** | dotenvy | 0.15 | .env file support |
| **Utilities** | bytes, async-trait, futures-util | — | General utilities |

## File Structure

### `/src/main.rs` (157 lines)
**Responsibilities:**
- Binary entry point with `#[tokio::main]`
- Load environment variables and configuration via `Config::from_env()`
- Test upstream database connectivity before starting proxy
- Create tokio-postgres connection pool with deadpool
- Create query cache (moka)
- Bind TCP listener to `LISTEN_HOST:LISTEN_PORT`
- Accept incoming connections and spawn async handlers
- Handle graceful shutdown (Ctrl+C, SIGTERM)
- Initialize structured logging (tracing-subscriber)

**Key flow:**
1. Load config → test upstream → create pool + cache
2. Bind listener → log "listening on X:Y"
3. Accept connections in loop → spawn handler per connection
4. On shutdown signal → graceful stop

### `/src/config.rs` (75 lines)
**Responsibilities:**
- `Config` struct with all configuration fields
- Parse environment variables (required and optional)
- Provide sensible defaults (port 5432, ttl 900s, pool size 5, etc.)
- Return `Result<Config, String>` with clear error messages

**Required env vars:**
- `UPSTREAM_HOST`
- `UPSTREAM_USER`
- `UPSTREAM_PASSWORD`
- `UPSTREAM_DATABASE`

**Optional env vars with defaults:**
- `UPSTREAM_PORT` (5432)
- `LISTEN_HOST` (0.0.0.0)
- `LISTEN_PORT` (5433)
- `CACHE_TTL_SECONDS` (900)
- `MAX_CACHE_ENTRIES` (10000)
- `POOL_SIZE` (5)

### `/src/cache.rs` (70+ lines)
**Responsibilities:**
- `ColumnDescription` struct (name, type_oid)
- `CachedResult` struct (columns, rows — all data as strings)
- `QueryCache` wrapper around moka async cache
- Methods: `new()`, `get()`, `insert()`, `invalidate_all()`, `entry_count()`

**Key design:**
- Cache key is `u64` hash of SQL query string (via twox-hash)
- Cache values are `Vec<Vec<Option<String>>>` (all results stored as strings)
- TTL enforced by moka (entries expire after `ttl_seconds`)
- Max capacity enforced by moka (LFU admission + LRU eviction)

### `/src/classifier.rs` (100+ lines)
**Responsibilities:**
- `QueryClassification` enum: `ReadOnly`, `CacheReset`, `Blocked(String)`
- `classify_query(sql: &str) -> QueryClassification` main entry point
- `classify_statement(stmt: &Statement) -> QueryClassification` recursive helper

**Whitelist approach:**
- Allow: SELECT, SHOW, EXPLAIN, SET (and cache reset function)
- Block: INSERT, UPDATE, DELETE, CREATE, DROP, ALTER, TRUNCATE, etc.

**Key logic:**
1. Check for `rustproxy_cache_reset()` function call first (case-insensitive)
2. Parse SQL with sqlparser PostgreSQL dialect
3. For each parsed statement, recursively check if it's read-only
4. Return `ReadOnly` only if ALL statements are read-only
5. Return `Blocked` with descriptive error message if any write found

### `/src/proxy.rs` (300+ lines)
**Responsibilities:**
- `ReadOnlyProxy` struct (holds pool and cache references)
- `ProxyHandlerFactory` struct (pgwire factory for multi-connection handling)
- Implement `SimpleQueryHandler` trait (pgwire protocol interface)
- Implement `ExtendedQueryHandler` trait (prepared statements)
- Type mapping from tokio-postgres types to pgwire types

**Key flow per query:**
1. Receive SQL string from client
2. Call `classify_query()` to determine action
3. If **ReadOnly**:
   - Hash query string to get cache key
   - Check cache with `cache.get(key).await`
   - If hit → return cached columns + rows
   - If miss → acquire connection from pool → execute `SELECT ...` → cache result → return
4. If **CacheReset**:
   - Call `cache.invalidate_all()`
   - Return OK with entry count
5. If **Blocked**:
   - Return PostgreSQL error 25006 (read_only_sql_transaction)

**Type mapping:**
- `PgType::BOOL` → `pgwire::Type::BOOL`
- `PgType::INT*` → `pgwire::Type::INT*`
- `PgType::VARCHAR | TEXT` → `pgwire::Type::VARCHAR`
- `PgType::JSON | JSONB` → `pgwire::Type::JSON`
- Unknown → `pgwire::Type::VARCHAR` (safe fallback)

## Key Design Decisions

### 1. Whitelist-based SQL filtering
**Decision:** Block all writes by explicitly whitelisting read-only operations (SELECT, SHOW, EXPLAIN, SET).
**Why:** Safer than blacklist approach. If new SQL constructs appear, they default to blocked. Uses sqlparser AST for robust parsing (not regex).

### 2. No authentication
**Decision:** Proxy does NOT verify PostgreSQL credentials from clients. Any client can connect and authenticate with upstream password.
**Why:** The proxy is deployed on a secure internal network (Dokploy). Clients are already trusted. Authentication overhead would be unjustified for this use case.

### 3. String-based query caching
**Decision:** All cached results stored as `Vec<Vec<Option<String>>>`. No native type preservation.
**Why:** Simplifies cache key generation (hash of SQL string). Reduces memory overhead. Clients still see correct PostgreSQL types via wire protocol (type OIDs are preserved in column metadata).

### 4. Single binary deployment
**Decision:** No separate binary for admin tasks or cache management. Cache reset via SQL: `SELECT rustproxy_cache_reset()`.
**Why:** Simpler operations, less deployment complexity. Clients can reset cache themselves if needed.

### 5. Connection pooling with deadpool
**Decision:** Use deadpool-postgres for pooling, not custom connection management.
**Why:** Handles idle timeout gracefully (critical for Timeweb's 15-min timeout). Automatic reconnection. Battle-tested, used in production Rust services.

### 6. No TLS/SSL support
**Decision:** Proxy does not terminate TLS. Designed for internal network only.
**Why:** Deployment is on Dokploy (internal network). If external TLS needed, add reverse proxy (e.g., Traefik) in front.

## How to Build and Test

### Build

```bash
cargo build --release
```

Output: `target/release/rust-pg-proxy` (~8-15 MB, depends on Rust version)

### Build Docker image

```bash
docker build -t rust-pg-proxy:latest .
```

Image size: ~20 MB (Alpine base + Rust binary)

### Run locally (development)

```bash
# Create .env file with test database credentials
cp .env.example .env
# Edit .env with your test database details

# Run in debug mode (slower, more logging)
RUST_LOG=debug cargo run
```

### Unit tests

```bash
cargo test --lib
```

Tests are in submodules:
- `src/classifier.rs` has tests for SQL parsing
- `src/cache.rs` has tests for cache operations
- All tests run **offline** without connecting to PostgreSQL

### Integration tests

**Integration tests require upstream PostgreSQL access:**

```bash
# Set environment variables (or create .env)
export UPSTREAM_HOST=localhost
export UPSTREAM_USER=test_user
export UPSTREAM_PASSWORD=test_pass
export UPSTREAM_DATABASE=test_db

# Run integration tests
cargo test --test '*' -- --test-threads=1
```

Note: Integration tests are in `tests/` directory (if they exist). Most testing is via unit tests + manual psql checks.

### Manual testing with psql

```bash
# Terminal 1: Start proxy
cargo run --release

# Terminal 2: Connect client
psql -h 0.0.0.0 -p 5433 -U <UPSTREAM_USER> -d <UPSTREAM_DATABASE>

# Try queries
SELECT * FROM table LIMIT 1;  -- Should work (if cached, instant; if miss, query upstream)
SELECT rustproxy_cache_reset();  -- Should clear cache
INSERT INTO table VALUES (...);  -- Should error: "Write operations are not permitted"
```

## Environment Variables

### Connection to upstream database

```bash
UPSTREAM_HOST=postgres.example.com
UPSTREAM_PORT=5432
UPSTREAM_USER=admin_user
UPSTREAM_PASSWORD=secret_pass
UPSTREAM_DATABASE=production_db
```

### Proxy server settings

```bash
LISTEN_HOST=0.0.0.0         # Bind address
LISTEN_PORT=5433            # Bind port
```

### Cache behavior

```bash
CACHE_TTL_SECONDS=900       # Entry lifetime (15 minutes)
MAX_CACHE_ENTRIES=10000     # Max concurrent cache entries (LFU eviction)
POOL_SIZE=5                 # Max connections to upstream
RUST_LOG=info               # Log level (trace|debug|info|warn|error)
```

## Deployment: Dokploy

This proxy is designed for Dokploy deployment on `dokploy-network`.

### Prerequisites

- Dokploy instance running
- `dokploy-network` external Docker network exists
- Environment variables: `UPSTREAM_HOST`, `UPSTREAM_USER`, `UPSTREAM_PASSWORD`, `UPSTREAM_DATABASE`

### Deploy

1. **Push code** to your Dokploy project repository (or mount via volume)
2. **Create docker-compose service** in Dokploy dashboard or via CLI:

```bash
docker-compose -f docker-compose.yml up -d
```

3. **Verify service started:**

```bash
docker logs rust-pg-proxy
```

Expected output:
```
Starting PostgreSQL Read-Only Proxy
Testing upstream connection...
Successfully connected to upstream database
Proxy server listening on 0.0.0.0:5433
```

4. **Connect from other Dokploy services:**

```bash
# From another container on dokploy-network
psql -h rust-pg-proxy -p 5433 -U admin_user -d production_db
```

### Health check

```bash
# From Dokploy host
curl -I postgres://rust-pg-proxy:5433  # Won't work (not HTTP)

# Better: use psql from another container
docker exec <other-container> psql -h rust-pg-proxy -p 5433 -c "SELECT 1"
```

### Logs

```bash
# Tail logs
docker logs -f rust-pg-proxy

# Filter by level
docker logs rust-pg-proxy | grep ERROR
```

### Stop service

```bash
docker-compose down
```

Or via Dokploy dashboard.

## Important Notes

### PLAN.md contains production credentials
**NEVER commit PLAN.md to version control.** It contains:
- Production database hostname
- Production credentials (user, password)
- Connection strings

If PLAN.md is exposed, rotate credentials immediately.

**Gitignore check:**
```bash
grep PLAN.md .gitignore  # Should exist
```

### Timeweb connection timeout handling
Production database at Timeweb closes idle connections after 15 minutes (idle_session_timeout=900000 ms).

**How proxy handles this:**
1. deadpool-postgres connection pool detects dropped connections
2. Automatically reconnects on next query
3. Client sees transparent recovery (query may take slightly longer)
4. No manual intervention needed

**If connections keep dropping:**
- Check `POOL_SIZE` (ensure ≥ 5 for safety)
- Check upstream database is responsive: `SELECT 1` query should succeed
- Check network connectivity to Timeweb host

### Cache reset behavior
**Command:** `SELECT rustproxy_cache_reset()`

**Behavior:**
- Returns 1 row: count of entries that were cleared
- Is NOT cached itself (no infinite recursion)
- Useful when upstream data changes and you need fresh results immediately

**Typical use:**
```sql
-- After bulk data import on upstream:
SELECT rustproxy_cache_reset();

-- Or in monitoring dashboard scheduled task:
-- "Run every 12 hours to refresh cache"
SELECT rustproxy_cache_reset();
```

### Performance characteristics
- **Cache hit:** O(1) hash lookup + serialization (~1-5ms)
- **Cache miss:** Full query execution + caching (~10-500ms depending on query complexity)
- **Memory:** ~50 KB per cached query result (depends on data size, not query complexity)
- **Connection overhead:** Pooled connections (~1-3ms per transaction)

### Scaling considerations
- Single binary, single process per instance (can run multiple replicas)
- Horizontal scaling: deploy multiple proxy instances behind load balancer
- Shared cache: each instance has its own cache (no cross-instance invalidation currently)
- If cache coherency needed: implement Redis-backed cache (future enhancement)

## Code Style and Conventions

**Rust idioms:**
- Prefer `?` operator over `.unwrap()` in fallible code
- Use `Result<T, E>` for operations that can fail
- Favor `Arc<T>` for shared ownership (used for cache, pool, proxy)
- Async-first: all I/O is async via tokio

**Error handling:**
- Return descriptive error messages
- Use `.map_err(|e| format!("context: {}", e))` to add context
- Log errors with `tracing::error!()` macro

**Logging:**
```rust
tracing::info!("Message: {}", var);   // Info-level
tracing::warn!("Warning: {}", var);   // Warning-level
tracing::error!("Error: {}", err);    // Error-level
tracing::debug!("Debug: {}", var);    // Debug-level (if RUST_LOG=debug)
```

**Naming:**
- `snake_case` for functions, variables, constants
- `CamelCase` for types, structs, traits
- Prefix private functions with underscore if needed

## Common Tasks

### Add a new SQL operation to whitelist

Edit `src/classifier.rs`, function `classify_statement()`:

```rust
// Find the match statement, add new case:
match statement {
    Statement::Select(_) => QueryClassification::ReadOnly,
    Statement::Show(_) => QueryClassification::ReadOnly,
    Statement::Explain { .. } => QueryClassification::ReadOnly,
    // Add here:
    Statement::MyNewReadOnlyOp { .. } => QueryClassification::ReadOnly,

    // All writes fall through to Blocked
    _ => QueryClassification::Blocked("...".to_string()),
}
```

### Adjust cache settings

Edit `.env`:
```bash
CACHE_TTL_SECONDS=1800      # 30 minutes instead of 15
MAX_CACHE_ENTRIES=50000     # More entries allowed
```

Or rebuild config in code via environment at runtime.

### Enable debug logging

```bash
RUST_LOG=debug cargo run
# Or for production:
RUST_LOG=debug docker-compose up
```

### Check upstream connection

```bash
# From proxy container:
psql -h $UPSTREAM_HOST -p $UPSTREAM_PORT -U $UPSTREAM_USER -d $UPSTREAM_DATABASE -c "SELECT version()"
```

---

**Last updated:** 2026-02-16
**Maintainer:** alexmak
