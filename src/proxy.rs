use crate::cache::{cache_key, CachedResult, ColumnDescription, QueryCache};
use crate::classifier::{classify_query, should_cache, QueryClassification};
use async_trait::async_trait;
use futures_util::sink::{Sink, SinkExt};
use futures_util::stream;
use pgwire::api::auth::{
    finish_authentication, save_startup_parameters_to_metadata, ServerParameterProvider,
    StartupHandler,
};
use pgwire::api::portal::Portal;
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{
    DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo,
    QueryResponse, Response, Tag,
};
use pgwire::api::stmt::{NoopQueryParser, QueryParser, StoredStatement};
use pgwire::api::store::PortalStore;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireHandlerFactory, Type, DEFAULT_NAME};
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::extendedquery::{Parse, ParseComplete};
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use std::fmt::Debug;
use std::sync::Arc;
use tokio::time::{timeout, Duration};

/// Query timeout for upstream database operations
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Convert a PostgreSQL type OID to pgwire Type
fn oid_to_pgwire_type(oid: u32) -> Type {
    match oid {
        16 => Type::BOOL,
        21 => Type::INT2,
        23 => Type::INT4,
        20 => Type::INT8,
        700 => Type::FLOAT4,
        701 => Type::FLOAT8,
        1043 | 25 => Type::VARCHAR,
        17 => Type::BYTEA,
        1114 => Type::TIMESTAMP,
        1184 => Type::TIMESTAMPTZ,
        1082 => Type::DATE,
        1083 => Type::TIME,
        114 | 3802 => Type::JSON,
        2950 => Type::UUID,
        1700 => Type::NUMERIC,
        _ => Type::VARCHAR, // Default fallback
    }
}

/// Create a PgWireError for blocked write operations
fn blocked_error(reason: &str) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        "25006".to_owned(),
        format!("Write operations are not permitted: {}", reason),
    )))
}

/// Create a PgWireError for connection pool failures
fn pool_error(err: impl std::fmt::Display) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        "08006".to_owned(),
        format!("Failed to get connection from pool: {}", err),
    )))
}

/// Create a PgWireError for query timeout
fn timeout_error(context: &str) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        "57014".to_owned(),
        format!("{} timed out ({}s limit)", context, QUERY_TIMEOUT.as_secs()),
    )))
}

/// Create a PgWireError from a tokio-postgres error
fn upstream_error(err: &tokio_postgres::Error, context: &str) -> PgWireError {
    let code = if let Some(db_err) = err.as_db_error() {
        db_err.code().code().to_string()
    } else {
        "08006".to_string()
    };
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_owned(),
        code,
        format!("{}: {}", context, err),
    )))
}

/// Column metadata extracted from upstream query results
struct ColumnMeta {
    name: String,
    type_oid: u32,
    pgwire_type: Type,
}

/// Extract column metadata from upstream query result columns
fn extract_column_meta(columns: &[tokio_postgres::Column]) -> Vec<ColumnMeta> {
    columns
        .iter()
        .map(|col| {
            let type_oid = col.type_().oid();
            ColumnMeta {
                name: col.name().to_owned(),
                type_oid,
                pgwire_type: oid_to_pgwire_type(type_oid),
            }
        })
        .collect()
}

/// Build pgwire FieldInfo from column metadata
fn build_field_info(columns: &[ColumnMeta]) -> Vec<FieldInfo> {
    columns
        .iter()
        .map(|col| {
            FieldInfo::new(
                col.name.clone(),
                None,
                None,
                col.pgwire_type.clone(),
                FieldFormat::Text,
            )
        })
        .collect()
}

/// Build pgwire FieldInfo from tokio-postgres Statement columns (for Describe)
fn build_field_info_from_stmt(stmt: &tokio_postgres::Statement) -> Vec<FieldInfo> {
    stmt.columns()
        .iter()
        .map(|col| {
            FieldInfo::new(
                col.name().to_owned(),
                None,
                None,
                oid_to_pgwire_type(col.type_().oid()),
                FieldFormat::Text,
            )
        })
        .collect()
}

/// Extract a single column value from a tokio-postgres Row as an Option<String>
fn extract_row_value(row: &tokio_postgres::Row, idx: usize, type_oid: u32) -> Option<String> {
    match type_oid {
        // String types
        25 | 1043 | 19 | 18 | 1042 => row.try_get::<_, Option<String>>(idx).ok().flatten(),
        // Int types
        21 => row
            .try_get::<_, Option<i16>>(idx)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        23 => row
            .try_get::<_, Option<i32>>(idx)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        20 => row
            .try_get::<_, Option<i64>>(idx)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        // Float types
        700 => row
            .try_get::<_, Option<f32>>(idx)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        701 => row
            .try_get::<_, Option<f64>>(idx)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        // Boolean
        16 => row
            .try_get::<_, Option<bool>>(idx)
            .ok()
            .flatten()
            .map(|v| v.to_string()),
        // Fallback: try String first, then common types
        _ => {
            if let Ok(Some(v)) = row.try_get::<_, Option<String>>(idx) {
                Some(v)
            } else if let Ok(Some(v)) = row.try_get::<_, Option<i64>>(idx) {
                Some(v.to_string())
            } else if let Ok(Some(v)) = row.try_get::<_, Option<f64>>(idx) {
                Some(v.to_string())
            } else if let Ok(Some(v)) = row.try_get::<_, Option<bool>>(idx) {
                Some(v.to_string())
            } else {
                None
            }
        }
    }
}

/// Encode upstream rows into pgwire DataRows and optionally collect string
/// representations for caching.
///
/// Returns (encoded_rows, cached_rows_if_requested).
fn encode_upstream_rows(
    rows: &[tokio_postgres::Row],
    columns: &[ColumnMeta],
    fields: &Arc<Vec<FieldInfo>>,
    collect_for_cache: bool,
) -> PgWireResult<(Vec<PgWireResult<pgwire::messages::data::DataRow>>, Option<Vec<Vec<Option<String>>>>)> {
    let mut result_rows = Vec::with_capacity(rows.len());
    let mut cached_rows = if collect_for_cache {
        Some(Vec::with_capacity(rows.len()))
    } else {
        None
    };

    for row in rows {
        let mut row_strings = if collect_for_cache {
            Some(Vec::with_capacity(columns.len()))
        } else {
            None
        };
        let mut encoder = DataRowEncoder::new(fields.clone());

        for (i, col) in columns.iter().enumerate() {
            let value_str = extract_row_value(row, i, col.type_oid);

            match &value_str {
                Some(v) => encoder.encode_field(&Some(v.as_bytes()))?,
                None => encoder.encode_field(&None::<&[u8]>)?,
            }

            if let Some(ref mut strings) = row_strings {
                strings.push(value_str);
            }
        }

        result_rows.push(encoder.finish());
        if let Some(ref mut cache) = cached_rows {
            cache.push(row_strings.unwrap());
        }
    }

    Ok((result_rows, cached_rows))
}

/// Build typed parameter values from portal raw bytes for upstream query execution.
///
/// Extended protocol clients send parameters as raw bytes. For text-format parameters
/// (the common case), bytes are UTF-8 text. We parse them into appropriate Rust types
/// based on the upstream statement's parameter type information.
fn build_typed_params(
    portal_params: &[Option<bytes::Bytes>],
    param_types: &[tokio_postgres::types::Type],
) -> Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> {
    let mut params: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> =
        Vec::with_capacity(portal_params.len());

    for (i, raw_param) in portal_params.iter().enumerate() {
        match raw_param {
            None => {
                // NULL parameter
                params.push(Box::new(None::<String>));
            }
            Some(bytes) => {
                // Text format: bytes are UTF-8 string representation
                let text = String::from_utf8_lossy(bytes).to_string();

                if i < param_types.len() {
                    // Compare by reference since tokio_postgres::types::Type
                    // uses identity comparison, not pattern matching
                    let pt = &param_types[i];
                    if pt == &tokio_postgres::types::Type::INT2 {
                        if let Ok(val) = text.parse::<i16>() {
                            params.push(Box::new(val));
                        } else {
                            params.push(Box::new(text));
                        }
                    } else if pt == &tokio_postgres::types::Type::INT4 {
                        if let Ok(val) = text.parse::<i32>() {
                            params.push(Box::new(val));
                        } else {
                            params.push(Box::new(text));
                        }
                    } else if pt == &tokio_postgres::types::Type::INT8 {
                        if let Ok(val) = text.parse::<i64>() {
                            params.push(Box::new(val));
                        } else {
                            params.push(Box::new(text));
                        }
                    } else if pt == &tokio_postgres::types::Type::FLOAT4 {
                        if let Ok(val) = text.parse::<f32>() {
                            params.push(Box::new(val));
                        } else {
                            params.push(Box::new(text));
                        }
                    } else if pt == &tokio_postgres::types::Type::FLOAT8 {
                        if let Ok(val) = text.parse::<f64>() {
                            params.push(Box::new(val));
                        } else {
                            params.push(Box::new(text));
                        }
                    } else if pt == &tokio_postgres::types::Type::BOOL {
                        let val =
                            text == "t" || text == "true" || text == "1" || text == "TRUE";
                        params.push(Box::new(val));
                    } else {
                        // Default: pass as string, PostgreSQL will coerce
                        params.push(Box::new(text));
                    }
                } else {
                    params.push(Box::new(text));
                }
            }
        }
    }

    params
}

pub struct ReadOnlyProxy {
    pool: deadpool_postgres::Pool,
    cache: Arc<QueryCache>,
}

impl ReadOnlyProxy {
    pub fn new(pool: deadpool_postgres::Pool, cache: Arc<QueryCache>) -> Self {
        Self { pool, cache }
    }

    /// Get a connection from the pool with proper error handling
    async fn get_connection(
        &self,
    ) -> PgWireResult<deadpool_postgres::Client> {
        self.pool.get().await.map_err(|e| pool_error(e))
    }

    /// Prepare a statement on upstream with timeout
    async fn prepare_upstream(
        &self,
        pg_client: &deadpool_postgres::Client,
        sql: &str,
    ) -> PgWireResult<tokio_postgres::Statement> {
        let prepare_fut = pg_client.prepare(sql);
        let result: Result<tokio_postgres::Statement, tokio_postgres::Error> =
            timeout(QUERY_TIMEOUT, prepare_fut)
                .await
                .map_err(|_| timeout_error("Query preparation"))?;
        result.map_err(|e| upstream_error(&e, "Query preparation failed"))
    }

    /// Execute a query with parameters on upstream with timeout
    async fn query_upstream(
        &self,
        pg_client: &deadpool_postgres::Client,
        stmt: &tokio_postgres::Statement,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> PgWireResult<Vec<tokio_postgres::Row>> {
        let query_fut = pg_client.query(stmt, params);
        let result: Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> =
            timeout(QUERY_TIMEOUT, query_fut)
                .await
                .map_err(|_| timeout_error("Query execution"))?;
        result.map_err(|e| upstream_error(&e, "Query execution failed"))
    }

    /// Execute a read-only query on upstream and build a Response.
    /// Used by both simple and extended query handlers.
    async fn execute_readonly_query<'a>(
        &self,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
        use_cache: bool,
    ) -> PgWireResult<Response<'a>> {
        let cache_key_val = cache_key(sql);
        let should_cache_query = use_cache && params.is_empty() && should_cache(sql);

        // Check cache (only for queries without parameters)
        if should_cache_query {
            if let Some(cached) = self.cache.get(cache_key_val).await {
                tracing::debug!("Cache hit for query");
                return self.build_cached_response(&cached);
            }
        }

        // Cache miss or uncacheable - execute on upstream
        tracing::debug!("Executing on upstream");
        let pg_client = self.get_connection().await?;

        let stmt = self.prepare_upstream(&pg_client, sql).await?;
        let rows = self.query_upstream(&pg_client, &stmt, params).await?;

        // Extract column metadata
        let columns: Vec<ColumnMeta> = if !rows.is_empty() {
            extract_column_meta(rows[0].columns())
        } else {
            extract_column_meta(stmt.columns())
        };

        let fields = Arc::new(build_field_info(&columns));
        let (result_rows, cached_rows) =
            encode_upstream_rows(&rows, &columns, &fields, should_cache_query)?;

        // Cache the result if applicable
        if should_cache_query {
            if let Some(cache_data) = cached_rows {
                let cached_columns: Vec<ColumnDescription> = columns
                    .iter()
                    .map(|col| ColumnDescription {
                        name: col.name.clone(),
                        type_oid: col.type_oid,
                    })
                    .collect();

                let cached_result = CachedResult {
                    columns: cached_columns,
                    rows: cache_data,
                };

                self.cache.insert(cache_key_val, cached_result).await;
                tracing::debug!("Cached query result");
            }
        }

        let row_count = result_rows.len();
        let row_stream = stream::iter(result_rows.into_iter());
        let query_response = QueryResponse::new(fields, row_stream);
        tracing::info!("Returned {} rows from upstream", row_count);
        Ok(Response::Query(query_response))
    }

    /// Build a Response from cached data
    fn build_cached_response<'a>(&self, cached: &CachedResult) -> PgWireResult<Response<'a>> {
        let fields: Vec<FieldInfo> = cached
            .columns
            .iter()
            .map(|col| {
                let pg_type = oid_to_pgwire_type(col.type_oid);
                FieldInfo::new(col.name.clone(), None, None, pg_type, FieldFormat::Text)
            })
            .collect();

        let mut results = Vec::with_capacity(cached.rows.len());
        for row in &cached.rows {
            let mut encoder = DataRowEncoder::new(Arc::new(fields.clone()));
            for value in row {
                match value {
                    Some(v) => encoder.encode_field(&Some(v.as_bytes()))?,
                    None => encoder.encode_field(&None::<&[u8]>)?,
                }
            }
            results.push(encoder.finish());
        }

        let row_count = results.len();
        let fields_arc = Arc::new(fields);
        let row_stream = stream::iter(results.into_iter());
        let query_response = QueryResponse::new(fields_arc, row_stream);
        tracing::info!("Returned {} rows from cache", row_count);
        Ok(Response::Query(query_response))
    }

    /// Handle cache reset command, returning an Execution response
    fn handle_cache_reset<'a>(&self) -> Response<'a> {
        let count = self.cache.entry_count();
        self.cache.invalidate_all();
        tracing::info!("Cache cleared, {} entries removed", count);
        Response::Execution(Tag::new("SELECT").with_rows(1))
    }
}

#[async_trait]
impl SimpleQueryHandler for ReadOnlyProxy {
    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        _client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        tracing::info!("Received simple query: {}", query);

        let classification = classify_query(query);

        match classification {
            QueryClassification::Blocked(reason) => {
                tracing::warn!("Blocked write operation: {}", reason);
                Err(blocked_error(&reason))
            }
            QueryClassification::CacheReset => Ok(vec![self.handle_cache_reset()]),
            QueryClassification::ReadOnly => {
                let response = self.execute_readonly_query(query, &[], true).await?;
                Ok(vec![response])
            }
        }
    }
}

#[async_trait]
impl ExtendedQueryHandler for ReadOnlyProxy {
    type Statement = String;
    type QueryParser = NoopQueryParser;

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        Arc::new(NoopQueryParser)
    }

    /// Override on_parse to classify SQL at Parse time.
    ///
    /// This rejects blocked queries immediately when the client sends a Prepare,
    /// rather than waiting until Execute. This means `client.prepare("INSERT ...")`
    /// fails right away in the extended protocol.
    ///
    /// For allowed queries, we replicate the default on_parse behavior:
    /// parse the SQL with our NoopQueryParser, store the statement, and send ParseComplete.
    async fn on_parse<C>(&self, client: &mut C, message: Parse) -> PgWireResult<()>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let sql = &message.query;
        tracing::info!("Extended protocol Parse: {}", sql);

        // Classify early - reject blocked queries at Parse time
        let classification = classify_query(sql);
        if let QueryClassification::Blocked(reason) = classification {
            tracing::warn!("Blocked write operation at Parse: {}", reason);
            return Err(blocked_error(&reason));
        }

        // Replicate default on_parse behavior:
        // 1. Parse SQL with query parser (NoopQueryParser just returns the SQL string)
        let parser = self.query_parser();
        let types: Vec<Type> = message
            .type_oids
            .iter()
            .map(|oid| Type::from_oid(*oid).unwrap_or(Type::UNKNOWN))
            .collect();
        let statement = parser.parse_sql(&message.query, &types).await?;

        // 2. Create StoredStatement and store in portal store
        let id = message
            .name
            .clone()
            .unwrap_or_else(|| DEFAULT_NAME.to_owned());
        let stored = StoredStatement::new(id, statement, types);
        client.portal_store().put_statement(Arc::new(stored));

        // 3. Send ParseComplete
        client
            .send(PgWireBackendMessage::ParseComplete(ParseComplete::new()))
            .await?;

        Ok(())
    }

    async fn do_query<'a, 'b: 'a, C>(
        &'b self,
        _client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let sql = &portal.statement.statement;
        tracing::info!("Extended protocol Execute: {}", sql);

        let classification = classify_query(sql);

        match classification {
            QueryClassification::Blocked(reason) => {
                // This should not normally happen since on_parse rejects blocked queries,
                // but we keep this as a safety net.
                tracing::warn!("Blocked write operation at Execute: {}", reason);
                Err(blocked_error(&reason))
            }
            QueryClassification::CacheReset => Ok(self.handle_cache_reset()),
            QueryClassification::ReadOnly => {
                if portal.parameters.is_empty() {
                    // No parameters - can use the shared method with caching
                    self.execute_readonly_query(sql, &[], true).await
                } else {
                    // Has parameters - prepare on upstream and execute with typed params
                    let pg_client = self.get_connection().await?;
                    let stmt = self.prepare_upstream(&pg_client, sql).await?;

                    let typed_params = build_typed_params(&portal.parameters, stmt.params());
                    let param_refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                        typed_params.iter().map(|p| &**p as &(dyn tokio_postgres::types::ToSql + Sync)).collect();

                    let rows = self.query_upstream(&pg_client, &stmt, &param_refs).await?;

                    // Extract column metadata
                    let columns: Vec<ColumnMeta> = if !rows.is_empty() {
                        extract_column_meta(rows[0].columns())
                    } else {
                        extract_column_meta(stmt.columns())
                    };

                    let fields = Arc::new(build_field_info(&columns));
                    // Parameterized queries are not cached (different params = different results)
                    let (result_rows, _) =
                        encode_upstream_rows(&rows, &columns, &fields, false)?;

                    let row_count = result_rows.len();
                    let row_stream = stream::iter(result_rows.into_iter());
                    let query_response = QueryResponse::new(fields, row_stream);
                    tracing::info!("Returned {} rows from upstream (parameterized)", row_count);
                    Ok(Response::Query(query_response))
                }
            }
        }
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        statement: &StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let sql = &statement.statement;
        tracing::debug!("Describe statement: {}", sql);

        // Check classification (safety net, on_parse already blocks writes)
        let classification = classify_query(sql);
        if let QueryClassification::Blocked(reason) = classification {
            return Err(blocked_error(&reason));
        }

        // For cache reset, return a simple response with no params and one column
        if classification == QueryClassification::CacheReset {
            let fields = vec![FieldInfo::new(
                "rustproxy_cache_reset".to_owned(),
                None,
                None,
                Type::VARCHAR,
                FieldFormat::Text,
            )];
            return Ok(DescribeStatementResponse::new(vec![], fields));
        }

        // Prepare statement on upstream to get column and parameter type info
        let pg_client = self.get_connection().await?;
        let stmt = self.prepare_upstream(&pg_client, sql).await?;

        let param_types: Vec<Type> = stmt
            .params()
            .iter()
            .map(|pt| oid_to_pgwire_type(pt.oid()))
            .collect();

        let fields = build_field_info_from_stmt(&stmt);

        Ok(DescribeStatementResponse::new(param_types, fields))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        portal: &Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let sql = &portal.statement.statement;
        tracing::debug!("Describe portal: {}", sql);

        // Check classification
        let classification = classify_query(sql);
        if let QueryClassification::Blocked(reason) = classification {
            return Err(blocked_error(&reason));
        }

        // For cache reset, return a simple response
        if classification == QueryClassification::CacheReset {
            let fields = vec![FieldInfo::new(
                "rustproxy_cache_reset".to_owned(),
                None,
                None,
                Type::VARCHAR,
                FieldFormat::Text,
            )];
            return Ok(DescribePortalResponse::new(fields));
        }

        // Prepare on upstream to get column info
        let pg_client = self.get_connection().await?;
        let stmt = self.prepare_upstream(&pg_client, sql).await?;
        let fields = build_field_info_from_stmt(&stmt);

        Ok(DescribePortalResponse::new(fields))
    }
}

// Simple startup handler that accepts any connection
pub struct ProxyStartupHandler;

#[async_trait]
impl StartupHandler for ProxyStartupHandler {
    async fn on_startup<C>(
        &self,
        client: &mut C,
        message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        if let PgWireFrontendMessage::Startup(ref startup) = message {
            save_startup_parameters_to_metadata(client, startup);
            finish_authentication(client, self).await;
        }
        Ok(())
    }
}

impl ServerParameterProvider for ProxyStartupHandler {
    fn server_parameters<C>(
        &self,
        _client: &C,
    ) -> Option<std::collections::HashMap<String, String>>
    where
        C: ClientInfo,
    {
        let mut params = std::collections::HashMap::new();
        params.insert("server_version".to_owned(), "14.0".to_owned());
        params.insert("server_encoding".to_owned(), "UTF8".to_owned());
        params.insert("client_encoding".to_owned(), "UTF8".to_owned());
        params.insert("DateStyle".to_owned(), "ISO, MDY".to_owned());
        Some(params)
    }
}

// Wrapper type to implement PgWireHandlerFactory (avoids orphan rule)
pub struct ProxyHandlerFactory {
    pub proxy: Arc<ReadOnlyProxy>,
}

impl ProxyHandlerFactory {
    pub fn new(proxy: Arc<ReadOnlyProxy>) -> Self {
        Self { proxy }
    }
}

impl PgWireHandlerFactory for ProxyHandlerFactory {
    type StartupHandler = ProxyStartupHandler;
    type SimpleQueryHandler = ReadOnlyProxy;
    type ExtendedQueryHandler = ReadOnlyProxy;
    type CopyHandler = pgwire::api::copy::NoopCopyHandler;

    fn simple_query_handler(&self) -> Arc<Self::SimpleQueryHandler> {
        self.proxy.clone()
    }

    fn extended_query_handler(&self) -> Arc<Self::ExtendedQueryHandler> {
        self.proxy.clone()
    }

    fn startup_handler(&self) -> Arc<Self::StartupHandler> {
        Arc::new(ProxyStartupHandler)
    }

    fn copy_handler(&self) -> Arc<Self::CopyHandler> {
        Arc::new(pgwire::api::copy::NoopCopyHandler)
    }
}
