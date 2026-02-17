# BUG-002: UUID and Array Parameter Serialization Failure

## Status
Open

## Priority
High

## Summary

When PostgreSQL client libraries send parameterized queries against UUID-typed or array-typed columns, the proxy fails to serialize these parameters correctly. This causes `08006: error serializing parameter N` errors, resulting in HTTP 500 responses for any query that uses UUID or array parameters. This blocks core functionality in downstream applications (e.g., Majesty AI dashboard user event feed).

## Discovery Context

Found while debugging the Majesty AI dashboard (`/home/alexmak/majesty-ai/new/dashboard/`). The events API endpoint (`GET /api/users/:userId/events`) returned 500 for all users. The root cause traced to the pg-proxy failing to handle UUID-typed and array-typed bound parameters in extended query protocol.

## Development Access

Dashboard connects to pg-proxy with:
```
DATABASE_URL=postgresql://any:any@majesty-rust-pg-proxy:5433/default_db?sslmode=disable
```

- Proxy host (Docker network): `majesty-rust-pg-proxy:5433`
- Proxy host (localhost): `localhost:5433`
- Username/password: any value (proxy passes through to upstream)
- Database: `default_db`
- Container: `rustpgproxy` on `dokploy-network`

## Technical Analysis

### Problem 1: UUID Parameter Type (OID 2950)

When `postgres.js` sends a parameterized query like:
```sql
WHERE user_id = $1
```
where `user_id` is a `UUID` column, PostgreSQL infers the parameter type as UUID (OID 2950). The proxy's parameter serialization code does not handle OID 2950, causing:
```
08006: error serializing parameter 0
```

### Problem 2: Array Parameter Type

When queries use array parameters:
```sql
WHERE conversation_id = ANY($1)
```
where `$1` is a JavaScript array `['uuid1', 'uuid2', ...]`, the proxy cannot serialize the array-typed parameter.

### Affected Queries Pattern

Any parameterized query where:
- A bound parameter targets a UUID-typed column
- A bound parameter is an array (ANY/ALL operators)

## Reproduction

```bash
# Connect through proxy and try a UUID parameterized query
# Using postgres.js (Node.js):

import postgres from 'postgres';
const sql = postgres('postgresql://any:any@localhost:5433/default_db');

// This FAILS — UUID column with parameterized value
const userId = 'some-uuid-here';
await sql`SELECT * FROM users WHERE id = ${userId}`;
// Error: 08006: error serializing parameter 0

// This FAILS — array parameter
const ids = ['uuid1', 'uuid2'];
await sql`SELECT * FROM users WHERE id = ANY(${ids})`;
// Error: 08006: error serializing parameter 0
```

## Recommended Fix

### Approach: Add UUID and Array Type Serialization

The proxy's parameter serialization logic needs to support additional PostgreSQL type OIDs:
- **UUID (OID 2950)**: Serialize as text representation (hyphenated string format `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`)
- **UUID Array (OID 2951)**: Serialize as PostgreSQL array literal `{uuid1,uuid2,...}`
- **Text Array (OID 1009)**: Serialize as PostgreSQL array literal

Look at the parameter serialization code in the Rust source — likely in the extended query protocol handler where Bind message parameters are processed and forwarded to upstream.

### Effort Estimate

Medium — requires identifying all type OID handling paths and adding cases for UUID and array types.

## Workaround for Dashboard (Current)

In the dashboard's `events.ts`, all UUID comparisons are cast to text to force text-type parameter inference:
```sql
-- Instead of: WHERE user_id = $1
WHERE user_id::text = $1

-- Instead of: WHERE conversation_id = ANY($1)
WHERE conversation_id::text IN ($1, $2, $3, ...)
```
This works but is fragile and requires all downstream consumers to know about this proxy limitation.
