# rust-pg-proxy

A lightweight PostgreSQL proxy that enforces read-only access and caches query results.

[![CI](https://github.com/alexmakeev/rust-pg-proxy/actions/workflows/ci.yml/badge.svg)](https://github.com/alexmakeev/rust-pg-proxy/actions/workflows/ci.yml)

## Why?

When you need to grant read-only database access to BI tools, dashboards, or analytics services, you face a dilemma: creating restricted database users may be impossible due to hosting limitations, and sharing production credentials exposes full write privileges. This proxy solves the problem by sitting between clients and PostgreSQL, enforcing read-only access at the SQL parsing level while transparently caching results to reduce database load.

## Features

- **SQL-level write protection** — blocks all write operations before they reach the database (not just permissions)
- **Transparent query caching** — configurable TTL and eviction policy to reduce load on upstream database
- **PostgreSQL wire protocol compatible** — works with any PostgreSQL client (psql, DBeaver, Metabase, Grafana, JDBC/ODBC drivers)
- **Connection pooling** — efficient reuse of connections to upstream database with automatic reconnection
- **Lightweight** — single Rust binary, ~15MB Docker image, minimal memory footprint
- **Zero client configuration** — clients connect as if it's a regular PostgreSQL database

## Quick Start

### Docker (recommended)

```bash
docker run -d \
  --name rust-pg-proxy \
  -p 5433:5433 \
  -e UPSTREAM_HOST=your-db-host \
  -e UPSTREAM_USER=your-user \
  -e UPSTREAM_PASSWORD=your-password \
  -e UPSTREAM_DATABASE=your-database \
  ghcr.io/alexmakeev/rust-pg-proxy:latest
```

Then connect:
```bash
psql -h localhost -p 5433 -U any_user -d any_db
```

### Docker Compose

```yaml
services:
  pg-proxy:
    image: ghcr.io/alexmakeev/rust-pg-proxy:latest
    ports:
      - "5433:5433"
    environment:
      UPSTREAM_HOST: your-db-host
      UPSTREAM_USER: your-user
      UPSTREAM_PASSWORD: your-password
      UPSTREAM_DATABASE: your-database
```

### Build from Source

```bash
git clone https://github.com/alexmakeev/rust-pg-proxy.git
cd rust-pg-proxy
cargo build --release
```

Binary will be at `target/release/rust-pg-proxy`.

## How It Works

```
Client (psql/DBeaver/Grafana)
        │
        ▼
┌─────────────────┐
│  rust-pg-proxy   │  ← SQL classification + caching
│   :5433          │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  PostgreSQL      │
│   :5432          │
└─────────────────┘
```

1. Client sends SQL query via standard PostgreSQL wire protocol
2. Proxy parses and classifies the query using sqlparser
3. If write operation → reject with PostgreSQL error code 25006
4. If read operation → check cache → return cached result or forward to upstream
5. Cache results with configurable TTL (default: 15 minutes)

## What's Allowed / Blocked

| Operation | Status | Example |
|-----------|--------|---------|
| SELECT | ✅ Allowed | `SELECT * FROM users` |
| SHOW | ✅ Allowed | `SHOW server_version` |
| EXPLAIN | ✅ Allowed | `EXPLAIN SELECT ...` |
| SET | ❌ Blocked | Not supported in proxy mode |
| INSERT | ❌ Blocked | Returns SQLSTATE 25006 |
| UPDATE | ❌ Blocked | Returns SQLSTATE 25006 |
| DELETE | ❌ Blocked | Returns SQLSTATE 25006 |
| CREATE/ALTER/DROP | ❌ Blocked | Returns SQLSTATE 25006 |
| TRUNCATE | ❌ Blocked | Returns SQLSTATE 25006 |
| BEGIN/COMMIT/ROLLBACK | ❌ Blocked | Not supported in proxy mode |

Blocked queries return:
```
ERROR: 25006: Write operations are not permitted: INSERT statements are not allowed
```

## Configuration

All configuration is via environment variables.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `UPSTREAM_HOST` | Yes | — | PostgreSQL host |
| `UPSTREAM_PORT` | No | `5432` | PostgreSQL port |
| `UPSTREAM_USER` | Yes | — | Database user |
| `UPSTREAM_PASSWORD` | Yes | — | Database password |
| `UPSTREAM_DATABASE` | Yes | — | Database name |
| `LISTEN_HOST` | No | `0.0.0.0` | Proxy listen address |
| `LISTEN_PORT` | No | `5433` | Proxy listen port |
| `CACHE_TTL_SECONDS` | No | `900` | Cache TTL (15 min) |
| `MAX_CACHE_ENTRIES` | No | `10000` | Max cached queries |
| `POOL_SIZE` | No | `5` | Upstream connection pool |
| `RUST_LOG` | No | `info` | Log level (trace/debug/info/warn/error) |

### Example .env file

```bash
UPSTREAM_HOST=db.example.com
UPSTREAM_PORT=5432
UPSTREAM_USER=readonly_user
UPSTREAM_PASSWORD=secret_password
UPSTREAM_DATABASE=production
CACHE_TTL_SECONDS=900
MAX_CACHE_ENTRIES=10000
POOL_SIZE=5
RUST_LOG=info
```

## Cache Management

Reset all cached queries:
```sql
SELECT rustproxy_cache_reset();
```

Queries with non-deterministic functions (`now()`, `random()`, `current_timestamp`, etc.) are never cached.

## Use Cases

- **BI & Analytics** — Connect Metabase, Grafana, Redash, or Looker safely to production databases
- **Read Replicas** — Add a caching layer in front of read replicas to reduce load
- **API Backends** — Protect databases from accidental writes in read-heavy services
- **Development** — Give developers read-only access without managing database permissions
- **Dashboards** — Safe database access for monitoring and observability tools

## Testing

The project includes comprehensive integration tests.

### Running Tests

Integration tests require a PostgreSQL database. Set up environment variables:

```bash
export UPSTREAM_HOST=localhost
export UPSTREAM_PORT=5432
export UPSTREAM_USER=postgres
export UPSTREAM_PASSWORD=test_password
export UPSTREAM_DATABASE=postgres
```

Run tests:
```bash
cargo test -- --ignored
```

Run with output:
```bash
cargo test -- --ignored --nocapture
```

Unit tests (no database required):
```bash
cargo test --lib
```

## Tech Stack

- [pgwire](https://github.com/sunng87/pgwire) — PostgreSQL wire protocol implementation
- [sqlparser](https://github.com/sqlparser-rs/sqlparser-rs) — SQL parsing and classification
- [moka](https://github.com/moka-rs/moka) — High-performance async cache with LFU/LRU eviction
- [tokio](https://tokio.rs) — Async runtime
- [deadpool-postgres](https://github.com/bikeshedder/deadpool) — Async connection pooling

## License

MIT
