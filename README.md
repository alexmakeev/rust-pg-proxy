# Rust PostgreSQL Read-Only Proxy

A lightweight, high-performance proxy that provides read-only access to PostgreSQL databases without exposing production credentials or creating additional database users.

## Problem Statement

When you have a single PostgreSQL user account (e.g., an admin user) and need to grant read-only access to third-party tools or dashboards, you face a security dilemma: sharing the production credentials exposes full database privileges. Creating additional users may be impossible due to hosting limitations.

This proxy solves that problem by sitting between clients and PostgreSQL, enforcing read-only access at the SQL parsing level and caching results for analytics workloads.

## Architecture

```
Dashboard/Monitoring Client
         ↓
    :5433 (proxy)
    ┌─────────────────────────────┐
    │  rust-pg-proxy              │
    │ ┌──────────────────────────┐ │
    │ │  SQL Parser              │ │  Whitelist: SELECT, SHOW, EXPLAIN, SET
    │ │  (sqlparser AST)         │ │  Block: INSERT, UPDATE, DELETE, etc.
    │ │  Error code: 25006       │ │
    │ └──────────────────────────┘ │
    │ ┌──────────────────────────┐ │
    │ │  Query Cache             │ │  Moka async cache, configurable TTL
    │ │  (string-keyed)          │ │  LFU admission + LRU eviction
    │ └──────────────────────────┘ │
    │ ┌──────────────────────────┐ │
    │ │  Connection Pool         │ │  deadpool-postgres, handles idle timeout
    │ └──────────────────────────┘ │
    └─────────────────────────────┘
         ↓ :5432 (upstream)
    PostgreSQL (production)
```

**How it works:**
1. Client connects to proxy on port 5433 using standard PostgreSQL protocol
2. Proxy receives SQL query, parses it with sqlparser
3. If **read-only** (SELECT, SHOW, EXPLAIN, SET) → check cache → forward if miss → cache result → return
4. If **write** (INSERT, UPDATE, DELETE, etc.) → return error "Write operations are not permitted" (PostgreSQL error 25006)
5. Special command `SELECT rustproxy_cache_reset()` clears all cached entries

## Features

- **SQL-level read-only enforcement** — blocks all write operations before they reach the database
- **Query result caching** — configurable TTL and max entries, reduces load on upstream database
- **Connection pooling** — efficient reuse of connections to upstream PostgreSQL
- **Standard PostgreSQL wire protocol** — works with any PostgreSQL client (psql, JDBC, pgAdmin, etc.)
- **Minimal resource footprint** — ~10-30MB RAM, single Rust binary
- **Graceful shutdown** — handles Ctrl+C and SIGTERM signals
- **Structured logging** — tracing-based logs with environment-based filtering (RUST_LOG)
- **Docker ready** — multi-stage Dockerfile for minimal image size (~20MB)

## Quick Start

### Prerequisites
- Rust 1.56+ (install via `https://rustup.rs`)
- Access to an upstream PostgreSQL database

### Build

```bash
cd /home/alexmak/rust-pg-proxy
cargo build --release
```

Binary: `target/release/rust-pg-proxy`

### Configure

Create a `.env` file (or export environment variables):

```bash
# Upstream PostgreSQL connection
UPSTREAM_HOST=postgres.example.com
UPSTREAM_PORT=5432
UPSTREAM_USER=admin_user
UPSTREAM_PASSWORD=secret_password
UPSTREAM_DATABASE=production_db

# Proxy listening
LISTEN_HOST=0.0.0.0
LISTEN_PORT=5433

# Cache settings
CACHE_TTL_SECONDS=900        # 15 minutes
MAX_CACHE_ENTRIES=10000

# Connection pool
POOL_SIZE=5

# Logging
RUST_LOG=info
```

### Run

```bash
./target/release/rust-pg-proxy
```

Expected output:
```
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Starting PostgreSQL Read-Only Proxy
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Upstream: postgres.example.com:5432/production_db
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Listening on: 0.0.0.0:5433
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Cache settings: max_entries=10000, ttl=900s
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Connection pool size: 5
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Testing upstream connection...
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Successfully connected to upstream database
2026-02-16T10:30:00Z  INFO rust_pg_proxy: Proxy server listening on 0.0.0.0:5433
```

## Configuration Reference

All configuration is via environment variables (no config files needed).

### Required Variables

| Variable | Description |
|----------|-------------|
| `UPSTREAM_HOST` | PostgreSQL hostname or IP address |
| `UPSTREAM_USER` | PostgreSQL username |
| `UPSTREAM_PASSWORD` | PostgreSQL password |
| `UPSTREAM_DATABASE` | Database name to use |

### Optional Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `UPSTREAM_PORT` | `5432` | PostgreSQL port |
| `LISTEN_HOST` | `0.0.0.0` | Proxy listen address (0.0.0.0 = all interfaces) |
| `LISTEN_PORT` | `5433` | Proxy listen port |
| `CACHE_TTL_SECONDS` | `900` | Cache entry time-to-live in seconds |
| `MAX_CACHE_ENTRIES` | `10000` | Maximum number of cached query results |
| `POOL_SIZE` | `5` | Maximum connections to upstream database |
| `RUST_LOG` | `info` | Log level (trace, debug, info, warn, error) |

## Usage Examples

### Connect with psql

```bash
psql -h 0.0.0.0 -p 5433 -U admin_user -d production_db
```

### Select data

```sql
-- These work (cached)
SELECT id, name, email FROM users WHERE id > 100;
SELECT COUNT(*) FROM orders;
SELECT * FROM products LIMIT 10;
EXPLAIN SELECT * FROM large_table;
```

### Try to write (blocked)

```sql
-- This returns error 25006 (read_only_sql_transaction)
INSERT INTO users (name) VALUES ('new user');
UPDATE products SET price = 100 WHERE id = 1;
DELETE FROM logs WHERE created_at < NOW();
```

Output:
```
ERROR:  25006: Write operations are not permitted: INSERT statements are not allowed
LOCATION:  <proxy>
```

### Clear cache

```sql
SELECT rustproxy_cache_reset();
```

Response: `(1 row)` with entry count that was cleared.

### Check connection status

```sql
SELECT version();  -- Shows PostgreSQL version (from upstream)
```

## Docker Deployment

### Build image

```bash
docker build -t rust-pg-proxy:latest .
```

### Run container

```bash
docker run -d \
  --name rust-pg-proxy \
  -p 5433:5433 \
  -e UPSTREAM_HOST=postgres.example.com \
  -e UPSTREAM_USER=admin_user \
  -e UPSTREAM_PASSWORD=secret_password \
  -e UPSTREAM_DATABASE=production_db \
  -e CACHE_TTL_SECONDS=900 \
  -e MAX_CACHE_ENTRIES=10000 \
  rust-pg-proxy:latest
```

### Docker Compose

```bash
# Set environment variables
export UPSTREAM_HOST=postgres.example.com
export UPSTREAM_USER=admin_user
export UPSTREAM_PASSWORD=secret_password
export UPSTREAM_DATABASE=production_db

# Start service
docker-compose up -d

# View logs
docker-compose logs -f rust-pg-proxy

# Stop service
docker-compose down
```

### Dokploy Deployment

The `docker-compose.yml` is configured for Dokploy:

```bash
# On Dokploy host, in project directory:
docker-compose -f docker-compose.yml up -d
```

Service will be available at:
- Hostname: `rust-pg-proxy` (internal network)
- Port: `5433`
- Network: `dokploy-network` (shared with other Dokploy services)

Clients within the Dokploy network can connect as:
```bash
psql -h rust-pg-proxy -p 5433 -U admin_user -d production_db
```

## Testing

The project includes comprehensive integration tests that verify the proxy's behavior against a real PostgreSQL database.

### Running Tests

Integration tests are marked with `#[ignore]` by default because they require a real PostgreSQL connection. To run them:

1. Set up environment variables for the test database:

```bash
export UPSTREAM_HOST=localhost
export UPSTREAM_PORT=5432
export UPSTREAM_USER=postgres
export UPSTREAM_PASSWORD=your_password
export UPSTREAM_DATABASE=postgres
```

2. Run the ignored tests:

```bash
cargo test -- --ignored
```

3. Or run all tests (including unit tests):

```bash
cargo test -- --ignored --nocapture
```

### Test Coverage

The integration tests verify:

- **Read operations**: `SELECT 1`, `SELECT version()`, `SELECT current_database()`, queries from `information_schema`, `SHOW` commands
- **Write blocking**: `CREATE TABLE`, `INSERT`, `UPDATE`, `DELETE`, `DROP TABLE` (all should return SQLSTATE 25006)
- **Caching behavior**: Same query executed twice uses cache on second call
- **Cache reset**: `SELECT rustproxy_cache_reset()` clears cache entries
- **Multi-statement blocking**: Queries with mixed read/write statements are blocked

### Running Unit Tests

Unit tests for the classifier and cache modules don't require a database:

```bash
cargo test --lib
```

## Project Structure

```
rust-pg-proxy/
├── Cargo.toml                 # Rust dependencies and metadata
├── Cargo.lock                 # Locked dependency versions (for reproducible builds)
├── src/
│   ├── main.rs                # Entry point, server loop, signal handling
│   ├── lib.rs                 # Library exports for testing
│   ├── config.rs              # Environment variable parsing
│   ├── cache.rs               # Query result caching (moka)
│   ├── classifier.rs          # SQL parsing and classification (read/write/cache-reset)
│   └── proxy.rs               # PostgreSQL protocol handler (pgwire)
├── tests/
│   └── integration_test.rs    # Integration tests against real PostgreSQL
├── Dockerfile                 # Multi-stage Docker build
├── docker-compose.yml         # Docker Compose for local/Dokploy deployment
├── .env.example               # Example configuration
├── README.md                  # This file
├── CLAUDE.md                  # Project-specific Claude Code instructions
└── PLAN.md                    # Implementation plan and production credentials
```

## License

MIT

---

**Version:** 0.1.0
**Last updated:** 2026-02-16
