# BUG-001: Extended Query Protocol Not Supported — Breaks All Modern PostgreSQL Client Libraries

## Status
Open

## Priority
High

## Summary

The proxy currently rejects ALL extended query protocol messages (Parse, Bind, Describe, Execute) with error `0A000: Extended query protocol is not supported`. This makes the proxy incompatible with virtually every modern PostgreSQL client library, since they all use extended protocol by default — even when prepared statements are disabled.

## Discovery Context

The Majesty AI admin dashboard (Node.js + postgres.js v3.4.8) connects to the proxy and fails on every query, including the initial type-fetching query on connection startup.

## Technical Analysis

### PostgreSQL Wire Protocol: Two Modes

PostgreSQL has two query execution modes:

1. **Simple Query Protocol**: Client sends a single `Q` (Query) message containing a SQL string. Server responds with `RowDescription` + `DataRow` + `CommandComplete` + `ReadyForQuery`. No parameters — all values are inlined in the SQL text.

2. **Extended Query Protocol**: Client sends `Parse` -> `Bind` -> `Describe` -> `Execute` -> `Sync` messages. This supports parameterized queries (`$1`, `$2`), prepared statement caching, and binary format results.

### What the Proxy Does Now

In `src/proxy.rs`, the `ExtendedQueryHandler` trait methods (`do_query`, `do_describe_statement`, `do_describe_portal`) all return:
```rust
Err(PgWireError::UserError(Box::new(ErrorInfo::new(
    "ERROR".to_owned(),
    "0A000".to_owned(),
    "Extended query protocol is not supported. Use simple query protocol instead.".to_owned(),
))))
```

### Why `prepare: false` Doesn't Help

In postgres.js (and most libraries), `prepare: false` only controls **named prepared statement caching**. It does NOT switch the wire protocol:

```javascript
// postgres.js v3 internal logic (connection.js toBuffer function):
function toBuffer(q) {
    return q.options.simple          // Only this flag triggers simple protocol (Q message)
      ? b().Q().str(...)             // Simple protocol
      : q.prepare
        ? prepared(q)                // Extended: Parse(named)+Bind+Execute
        : unnamed(q)                 // Extended: Parse(unnamed)+Bind+Execute  <- prepare:false lands HERE
}
```

With `prepare: false`, queries still go through `unnamed()` which sends Parse/Bind/Execute — all extended protocol messages.

### Affected Clients

| Client Library | Language | Default Protocol | Can Force Simple? |
|---|---|---|---|
| postgres.js | Node.js | Extended | Only via `.simple()` (no params support) |
| node-postgres (pg) | Node.js | Extended | `queryMode: 'simple'` but no param support |
| asyncpg | Python | Extended | No simple mode option |
| sqlx | Rust | Extended | No simple mode option |
| pgx/pgxpool | Go | Extended | `prefer_simple_protocol` but limited |
| JDBC | Java | Extended | `preferQueryMode=simple` available |
| psql | CLI | Simple | Default simple, works fine |

**Conclusion: Only `psql` and raw TCP clients work with the proxy in its current state.**

### Even Connection Startup Fails

Many libraries send type-fetching queries on first connection (e.g., postgres.js queries `pg_catalog.pg_type`). These use extended protocol, so the connection fails before any user query is attempted.

## Reproduction

```bash
# Works (psql uses simple protocol):
psql -h localhost -p 5433 -U gen_user -d default_db -c "SELECT 1"

# Fails (node uses extended protocol):
node -e "
  const sql = require('postgres')('postgres://gen_user:pass@localhost:5433/default_db', {prepare: false});
  sql\`SELECT 1\`.then(console.log).catch(e => console.error(e.message));
"
# Output: Extended query protocol is not supported.
```

## Recommended Fix

### Approach: Forward Extended Protocol Messages to Upstream

Instead of rejecting extended protocol messages, forward them to the upstream PostgreSQL server after applying the read-only classifier to the SQL content.

The key messages to handle:

1. **Parse** (`'P'`): Contains the SQL query text. Extract the SQL, run it through `classifier.rs` to verify it's read-only, then forward to upstream.
2. **Bind** (`'B'`): Contains parameter values. Forward as-is (parameters don't affect read-only classification).
3. **Describe** (`'D'`): Requests metadata. Forward as-is (read-only operation).
4. **Execute** (`'E'`): Executes the prepared statement. Forward as-is (SQL was already checked in Parse).
5. **Sync** (`'S'`): Transaction sync. Forward as-is.
6. **Close** (`'C'`): Closes statement/portal. Forward as-is.

### Security Considerations

- SQL classification happens at **Parse** time, same as it does for simple queries now
- Parameter values (in Bind) don't change what the query does — `SELECT * FROM users WHERE id = $1` is read-only regardless of `$1`'s value
- The existing `classifier.rs` logic works on SQL text, which is available in the Parse message
- No changes needed to the read-only enforcement logic

### Implementation Sketch

In the `ExtendedQueryHandler` implementation for `ReadOnlyProxy`:

```rust
async fn do_query<'a>(
    &self,
    _client: &mut C,
    portal: &'a Portal<Self::Statement>,
    _max_rows: usize,
) -> PgWireResult<Response<QueryResponse<'a>>> {
    // The SQL was already classified in on_parse()
    // Portal contains bound parameters
    // Forward Parse+Bind+Execute to upstream and stream results back
}
```

The `pgwire` crate (which this proxy uses) already has the trait methods for extended protocol — they just need real implementations instead of error returns.

### Caching Implications

The current caching mechanism works at the simple query level (full SQL text -> cached response). For extended protocol:
- Cache key could be: parsed SQL text + serialized parameter values
- Or: cache at the Parse level (same SQL text -> same plan) and invalidate on Bind differences
- Simplest: initially skip caching for extended protocol queries, add it later

### Effort Estimate

- Parse message handling + classifier integration: the main work
- Bind/Describe/Execute forwarding: straightforward relay
- Connection multiplexing: may need per-client upstream connection for statement lifecycle
- Testing: integration tests with postgres.js, asyncpg, JDBC

### Alternative (Quick Workaround)

If extended protocol support takes time, a shorter-term fix:
- Add a `force_simple_protocol` proxy config option
- When enabled, intercept Parse messages, extract the SQL + parameters, reconstruct a simple query with inlined parameters, and forward via simple protocol
- This is a protocol translator, not a full extended protocol implementation
- Risk: SQL injection if parameter inlining is done incorrectly. Must use proper PostgreSQL literal escaping.

## Workaround for Dashboard (Current)

Until this is fixed, the dashboard will connect directly to the upstream PostgreSQL database, bypassing the proxy. This loses the read-only enforcement and caching benefits but is acceptable for an admin-only tool.
