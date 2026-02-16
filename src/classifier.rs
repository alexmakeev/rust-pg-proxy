use sqlparser::ast::{Expr, SelectItem, SetExpr, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

#[derive(Debug, Clone, PartialEq)]
pub enum QueryClassification {
    ReadOnly,
    CacheReset,
    Blocked(String),
}

/// Classifies a SQL query as read-only, cache-reset, or blocked
pub fn classify_query(sql: &str) -> QueryClassification {
    // Handle empty or whitespace-only queries
    if sql.trim().is_empty() {
        return QueryClassification::ReadOnly;
    }

    // Parse the SQL
    let statements = match Parser::parse_sql(&PostgreSqlDialect {}, sql) {
        Ok(stmts) => stmts,
        Err(_) => {
            return QueryClassification::Blocked("Failed to parse SQL".to_string());
        }
    };

    // Empty statement list after parsing
    if statements.is_empty() {
        return QueryClassification::ReadOnly;
    }

    // Check each statement - all must be read-only
    for statement in statements {
        match classify_statement(&statement) {
            QueryClassification::ReadOnly => continue,
            QueryClassification::CacheReset => return QueryClassification::CacheReset,
            QueryClassification::Blocked(reason) => {
                return QueryClassification::Blocked(reason);
            }
        }
    }

    QueryClassification::ReadOnly
}

/// Classifies a single parsed statement
fn classify_statement(statement: &Statement) -> QueryClassification {
    match statement {
        // SELECT statements - check for cache reset function, INTO, and FOR UPDATE/SHARE
        Statement::Query(query) => {
            // Check for cache reset function
            if let SetExpr::Select(select) = query.body.as_ref() {
                // Check for rustproxy_cache_reset() function call
                if select.projection.len() == 1 {
                    if let SelectItem::UnnamedExpr(Expr::Function(func)) = &select.projection[0] {
                        let func_name = func.name.to_string().to_lowercase();
                        if func_name == "rustproxy_cache_reset" {
                            return QueryClassification::CacheReset;
                        }
                    }
                }

                // Check for SELECT INTO
                if select.into.is_some() {
                    return QueryClassification::Blocked("SELECT INTO is not allowed".to_string());
                }
            }

            // Check for FOR UPDATE / FOR SHARE
            if !query.locks.is_empty() {
                return QueryClassification::Blocked(
                    "SELECT FOR UPDATE/SHARE is not allowed".to_string(),
                );
            }

            QueryClassification::ReadOnly
        }

        // Transaction control - BLOCKED (no session affinity, meaningless in proxy)
        Statement::StartTransaction { .. } => QueryClassification::Blocked(
            "Transactions are not supported in proxy mode".to_string(),
        ),
        Statement::Commit { .. } => QueryClassification::Blocked(
            "Transactions are not supported in proxy mode".to_string(),
        ),
        Statement::Rollback { .. } => QueryClassification::Blocked(
            "Transactions are not supported in proxy mode".to_string(),
        ),

        // SET commands - BLOCKED (session state leaks between pooled connections)
        Statement::Set(_) => QueryClassification::Blocked(
            "SET is not supported in proxy mode".to_string(),
        ),

        // SHOW commands - allowed
        Statement::ShowVariable { .. } => QueryClassification::ReadOnly,
        Statement::ShowCreate { .. } => QueryClassification::ReadOnly,
        Statement::ShowColumns { .. } => QueryClassification::ReadOnly,
        Statement::ShowTables { .. } => QueryClassification::ReadOnly,
        Statement::ShowCollation { .. } => QueryClassification::ReadOnly,

        // EXPLAIN - allowed
        Statement::Explain { .. } => QueryClassification::ReadOnly,
        Statement::ExplainTable { .. } => QueryClassification::ReadOnly,

        // Write operations - blocked
        Statement::Insert { .. } => {
            QueryClassification::Blocked("INSERT is not allowed".to_string())
        }
        Statement::Update { .. } => {
            QueryClassification::Blocked("UPDATE is not allowed".to_string())
        }
        Statement::Delete { .. } => {
            QueryClassification::Blocked("DELETE is not allowed".to_string())
        }
        Statement::Merge { .. } => {
            QueryClassification::Blocked("MERGE is not allowed".to_string())
        }

        // DDL - blocked
        Statement::CreateTable { .. } => {
            QueryClassification::Blocked("CREATE TABLE is not allowed".to_string())
        }
        Statement::CreateIndex { .. } => {
            QueryClassification::Blocked("CREATE INDEX is not allowed".to_string())
        }
        Statement::CreateView { .. } => {
            QueryClassification::Blocked("CREATE VIEW is not allowed".to_string())
        }
        Statement::CreateSchema { .. } => {
            QueryClassification::Blocked("CREATE SCHEMA is not allowed".to_string())
        }
        Statement::CreateDatabase { .. } => {
            QueryClassification::Blocked("CREATE DATABASE is not allowed".to_string())
        }
        Statement::CreateFunction { .. } => {
            QueryClassification::Blocked("CREATE FUNCTION is not allowed".to_string())
        }
        Statement::CreateProcedure { .. } => {
            QueryClassification::Blocked("CREATE PROCEDURE is not allowed".to_string())
        }
        Statement::CreateRole { .. } => {
            QueryClassification::Blocked("CREATE ROLE is not allowed".to_string())
        }
        Statement::CreateSequence { .. } => {
            QueryClassification::Blocked("CREATE SEQUENCE is not allowed".to_string())
        }
        Statement::CreateType { .. } => {
            QueryClassification::Blocked("CREATE TYPE is not allowed".to_string())
        }
        Statement::CreateVirtualTable { .. } => {
            QueryClassification::Blocked("CREATE VIRTUAL TABLE is not allowed".to_string())
        }

        Statement::AlterTable { .. } => {
            QueryClassification::Blocked("ALTER TABLE is not allowed".to_string())
        }
        Statement::AlterIndex { .. } => {
            QueryClassification::Blocked("ALTER INDEX is not allowed".to_string())
        }
        Statement::AlterView { .. } => {
            QueryClassification::Blocked("ALTER VIEW is not allowed".to_string())
        }
        Statement::AlterRole { .. } => {
            QueryClassification::Blocked("ALTER ROLE is not allowed".to_string())
        }

        Statement::Drop { .. } => {
            QueryClassification::Blocked("DROP is not allowed".to_string())
        }
        Statement::Truncate { .. } => {
            QueryClassification::Blocked("TRUNCATE is not allowed".to_string())
        }

        // Security - blocked
        Statement::Grant { .. } => {
            QueryClassification::Blocked("GRANT is not allowed".to_string())
        }
        Statement::Revoke { .. } => {
            QueryClassification::Blocked("REVOKE is not allowed".to_string())
        }

        // COPY - blocked
        Statement::Copy { .. } => {
            QueryClassification::Blocked("COPY is not allowed".to_string())
        }

        // Procedure calls - blocked
        Statement::Call(_) => {
            QueryClassification::Blocked("CALL is not allowed".to_string())
        }
        Statement::Execute { .. } => {
            QueryClassification::Blocked("EXECUTE is not allowed".to_string())
        }

        // Any other statement - blocked
        _ => QueryClassification::Blocked("Unrecognized statement".to_string()),
    }
}

/// Determines if a query result should be cached
pub fn should_cache(sql: &str) -> bool {
    let classification = classify_query(sql);

    // Don't cache cache-reset queries
    if classification == QueryClassification::CacheReset {
        return false;
    }

    // Don't cache blocked queries
    if matches!(classification, QueryClassification::Blocked(_)) {
        return false;
    }

    let sql_lower = sql.to_lowercase();

    // Don't cache non-deterministic functions (case-insensitive)
    let non_deterministic_functions = [
        "now()",
        "random()",
        "clock_timestamp()",
        "current_timestamp",
        "pg_sleep",
    ];

    for func in &non_deterministic_functions {
        if sql_lower.contains(func) {
            return false;
        }
    }

    // Parse to check statement types
    let statements = match Parser::parse_sql(&PostgreSqlDialect {}, sql) {
        Ok(stmts) => stmts,
        Err(_) => return false,
    };

    // Don't cache certain statement types
    for statement in statements {
        match statement {
            // Don't cache SET commands
            Statement::Set(_) => return false,

            // Don't cache EXPLAIN
            Statement::Explain { .. } | Statement::ExplainTable { .. } => return false,

            // Don't cache transaction control
            Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. } => return false,

            _ => continue,
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_select_allowed() {
        let result = classify_query("SELECT * FROM users");
        assert_eq!(result, QueryClassification::ReadOnly);
    }

    #[test]
    fn test_select_into_blocked() {
        let result = classify_query("SELECT * INTO new_table FROM users");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_select_for_update_blocked() {
        let result = classify_query("SELECT * FROM users FOR UPDATE");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_insert_blocked() {
        let result = classify_query("INSERT INTO users (name) VALUES ('test')");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_update_blocked() {
        let result = classify_query("UPDATE users SET name = 'test'");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_delete_blocked() {
        let result = classify_query("DELETE FROM users");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_create_blocked() {
        let result = classify_query("CREATE TABLE test (id INT)");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_drop_blocked() {
        let result = classify_query("DROP TABLE users");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_explain_allowed() {
        let result = classify_query("EXPLAIN SELECT * FROM users");
        assert_eq!(result, QueryClassification::ReadOnly);
    }

    #[test]
    fn test_set_blocked() {
        let result = classify_query("SET search_path TO public");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_show_allowed() {
        let result = classify_query("SHOW search_path");
        assert_eq!(result, QueryClassification::ReadOnly);
    }

    #[test]
    fn test_begin_blocked() {
        let result = classify_query("BEGIN");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_commit_blocked() {
        let result = classify_query("COMMIT");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_rollback_blocked() {
        let result = classify_query("ROLLBACK");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_cache_reset() {
        let result = classify_query("SELECT rustproxy_cache_reset()");
        assert_eq!(result, QueryClassification::CacheReset);
    }

    #[test]
    fn test_cache_reset_case_insensitive() {
        let result = classify_query("SELECT RUSTPROXY_CACHE_RESET()");
        assert_eq!(result, QueryClassification::CacheReset);
    }

    #[test]
    fn test_cache_reset_not_triggered_by_string_literal() {
        // String literal should not trigger cache reset
        let result = classify_query("SELECT 'rustproxy_cache_reset()'");
        assert_eq!(result, QueryClassification::ReadOnly);
    }

    #[test]
    fn test_empty_query() {
        let result = classify_query("   ");
        assert_eq!(result, QueryClassification::ReadOnly);
    }

    #[test]
    fn test_multi_statement_all_read() {
        let result = classify_query("SELECT 1; SELECT 2");
        assert_eq!(result, QueryClassification::ReadOnly);
    }

    #[test]
    fn test_multi_statement_one_blocked() {
        let result = classify_query("SELECT 1; INSERT INTO users (name) VALUES ('test')");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_should_cache_select() {
        assert!(should_cache("SELECT * FROM users"));
    }

    #[test]
    fn test_should_not_cache_now() {
        assert!(!should_cache("SELECT NOW()"));
    }

    #[test]
    fn test_should_not_cache_random() {
        assert!(!should_cache("SELECT random()"));
    }

    #[test]
    fn test_should_not_cache_clock_timestamp() {
        assert!(!should_cache("SELECT clock_timestamp()"));
    }

    #[test]
    fn test_should_not_cache_current_timestamp() {
        assert!(!should_cache("SELECT current_timestamp"));
    }

    #[test]
    fn test_should_not_cache_pg_sleep() {
        assert!(!should_cache("SELECT pg_sleep(1)"));
    }

    #[test]
    fn test_should_not_cache_set() {
        assert!(!should_cache("SET search_path TO public"));
    }

    #[test]
    fn test_should_not_cache_explain() {
        assert!(!should_cache("EXPLAIN SELECT * FROM users"));
    }

    #[test]
    fn test_should_not_cache_begin() {
        assert!(!should_cache("BEGIN"));
    }

    #[test]
    fn test_should_not_cache_cache_reset() {
        assert!(!should_cache("SELECT rustproxy_cache_reset()"));
    }

    #[test]
    fn test_should_not_cache_blocked() {
        assert!(!should_cache("INSERT INTO users (name) VALUES ('test')"));
    }

    #[test]
    fn test_call_blocked() {
        let result = classify_query("CALL my_procedure()");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_execute_blocked() {
        let result = classify_query("EXECUTE my_prepared_statement");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_grant_blocked() {
        let result = classify_query("GRANT SELECT ON users TO someuser");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }

    #[test]
    fn test_copy_blocked() {
        let result = classify_query("COPY users TO '/tmp/users.csv'");
        assert!(matches!(result, QueryClassification::Blocked(_)));
    }
}
