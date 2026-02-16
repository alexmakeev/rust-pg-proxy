use crate::cache::{cache_key, CachedResult, ColumnDescription, QueryCache};
use crate::classifier::{classify_query, should_cache, QueryClassification};
use async_trait::async_trait;
use futures_util::stream;
use futures_util::sink::Sink;
use pgwire::api::auth::{ServerParameterProvider, StartupHandler};
use pgwire::api::query::{ExtendedQueryHandler, SimpleQueryHandler};
use pgwire::api::results::{DataRowEncoder, DescribePortalResponse, DescribeStatementResponse, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::stmt::NoopQueryParser;
use pgwire::api::{ClientInfo, ClientPortalStore, PgWireHandlerFactory, Type};
use pgwire::api::store::PortalStore;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::{PgWireBackendMessage, PgWireFrontendMessage};
use std::fmt::Debug;
use std::sync::Arc;
use tokio::time::{timeout, Duration};

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
        1700 => Type::NUMERIC,  // ADD: NUMERIC/DECIMAL support
        _ => Type::VARCHAR, // Default fallback
    }
}

pub struct ReadOnlyProxy {
    pool: deadpool_postgres::Pool,
    cache: Arc<QueryCache>,
}

impl ReadOnlyProxy {
    pub fn new(pool: deadpool_postgres::Pool, cache: Arc<QueryCache>) -> Self {
        Self { pool, cache }
    }
}

#[async_trait]
impl SimpleQueryHandler for ReadOnlyProxy {
    async fn do_query<'a, C>(
        &self,
        _client: &mut C,
        query: &'a str,
    ) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        tracing::info!("Received query: {}", query);

        // Classify the query
        let classification = classify_query(query);

        match classification {
            QueryClassification::Blocked(reason) => {
                tracing::warn!("Blocked write operation: {}", reason);
                let error_info = ErrorInfo::new(
                    "ERROR".to_owned(),
                    "25006".to_owned(),
                    format!("Write operations are not permitted: {}", reason),
                );
                return Err(PgWireError::UserError(Box::new(error_info)));
            }
            QueryClassification::CacheReset => {
                let count = self.cache.entry_count();
                self.cache.invalidate_all();
                tracing::info!("Cache cleared, {} entries removed", count);

                let response = Response::Execution(Tag::new("SELECT").with_rows(1));
                // Send a custom notice message about cache clearing
                let notice = format!("Cache cleared, {} entries removed", count);
                tracing::info!("{}", notice);

                return Ok(vec![response]);
            }
            QueryClassification::ReadOnly => {
                let key = cache_key(query);
                let should_cache_query = should_cache(query);

                // Check cache first
                if let Some(cached) = self.cache.get(key).await {
                    tracing::debug!("Cache hit for query");

                    // Build field info from cached columns
                    let fields: Vec<FieldInfo> = cached
                        .columns
                        .iter()
                        .map(|col| {
                            let pg_type = oid_to_pgwire_type(col.type_oid);
                            FieldInfo::new(col.name.clone(), None, None, pg_type, FieldFormat::Text)
                        })
                        .collect();

                    // Encode rows
                    let mut results = Vec::new();
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
                    // results are already PgWireResult<DataRow>, don't wrap in Ok
                    let row_stream = stream::iter(results.into_iter());
                    let query_response = QueryResponse::new(fields_arc, row_stream);
                    let response = Response::Query(query_response);

                    tracing::info!("Returned {} rows from cache", row_count);
                    return Ok(vec![response]);
                }

                // Cache miss - execute query on upstream
                tracing::debug!("Cache miss, executing on upstream");
                let pg_client = self.pool.get().await.map_err(|e| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_owned(),
                        "08006".to_owned(),
                        format!("Failed to get connection from pool: {}", e),
                    )))
                })?;

                let query_timeout = Duration::from_secs(30); // 30 second query timeout
                let rows = timeout(query_timeout, pg_client.query(query, &[]))
                    .await
                    .map_err(|_| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_owned(),
                            "57014".to_owned(), // query_canceled
                            "Query execution timed out (30s limit)".to_owned(),
                        )))
                    })?
                    .map_err(|e| {
                        let code = if let Some(db_err) = e.as_db_error() {
                            db_err.code().code().to_string()
                        } else {
                            "08006".to_string() // connection_failure
                        };
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_owned(),
                            code,
                            format!("Query execution failed: {}", e),
                        )))
                    })?;

                // Extract column information
                let columns: Vec<_> = if !rows.is_empty() {
                    rows[0]
                        .columns()
                        .iter()
                        .map(|col| {
                            let pg_type = col.type_();
                            let type_oid = pg_type.oid();
                            let pgwire_type = oid_to_pgwire_type(type_oid);
                            (col.name().to_owned(), type_oid, pgwire_type)
                        })
                        .collect()
                } else {
                    // For queries with no results, we might need to use a prepared statement
                    // to get column info, but for simplicity, return empty result
                    vec![]
                };

                // Build field info
                let fields: Vec<FieldInfo> = columns
                    .iter()
                    .map(|(name, _oid, pgwire_type)| {
                        FieldInfo::new(name.clone(), None, None, pgwire_type.clone(), FieldFormat::Text)
                    })
                    .collect();

                // Convert rows to string representation for caching and response
                let mut cached_rows = Vec::new();
                let mut result_rows = Vec::new();

                let fields_arc = Arc::new(fields.clone());
                for row in &rows {
                    let mut row_strings = Vec::new();
                    let mut encoder = DataRowEncoder::new(fields_arc.clone());

                    for (i, (_name, type_oid, _pgwire_type)) in columns.iter().enumerate() {
                        // Use the column type OID to guide extraction
                        let value_str: Option<String> = match type_oid {
                            // String types
                            25 | 1043 | 19 | 18 | 1042 => row.try_get::<_, Option<String>>(i).ok().flatten(),
                            // Int types
                            21 => row.try_get::<_, Option<i16>>(i).ok().flatten().map(|v| v.to_string()),
                            23 => row.try_get::<_, Option<i32>>(i).ok().flatten().map(|v| v.to_string()),
                            20 => row.try_get::<_, Option<i64>>(i).ok().flatten().map(|v| v.to_string()),
                            // Float types
                            700 => row.try_get::<_, Option<f32>>(i).ok().flatten().map(|v| v.to_string()),
                            701 => row.try_get::<_, Option<f64>>(i).ok().flatten().map(|v| v.to_string()),
                            // Boolean
                            16 => row.try_get::<_, Option<bool>>(i).ok().flatten().map(|v| v.to_string()),
                            // Fallback: try String first, then try common types
                            _ => {
                                if let Ok(Some(v)) = row.try_get::<_, Option<String>>(i) {
                                    Some(v)
                                } else if let Ok(Some(v)) = row.try_get::<_, Option<i64>>(i) {
                                    Some(v.to_string())
                                } else if let Ok(Some(v)) = row.try_get::<_, Option<f64>>(i) {
                                    Some(v.to_string())
                                } else if let Ok(Some(v)) = row.try_get::<_, Option<bool>>(i) {
                                    Some(v.to_string())
                                } else {
                                    // Check if column is actually NULL
                                    None
                                }
                            }
                        };

                        // Encode for response
                        match &value_str {
                            Some(v) => encoder.encode_field(&Some(v.as_bytes()))?,
                            None => encoder.encode_field(&None::<&[u8]>)?,
                        }

                        row_strings.push(value_str);
                    }

                    cached_rows.push(row_strings);
                    result_rows.push(encoder.finish());
                }

                // Cache the result if applicable
                if should_cache_query {
                    let cached_columns: Vec<ColumnDescription> = columns
                        .iter()
                        .map(|(name, type_oid, _)| ColumnDescription {
                            name: name.clone(),
                            type_oid: *type_oid,
                        })
                        .collect();

                    let cached_result = CachedResult {
                        columns: cached_columns,
                        rows: cached_rows,
                    };

                    self.cache.insert(key, cached_result).await;
                    tracing::debug!("Cached query result");
                }

                let row_count = result_rows.len();
                let fields_arc = Arc::new(fields);
                // result_rows are already PgWireResult<DataRow>, don't wrap in Ok
                let row_stream = stream::iter(result_rows.into_iter());
                let query_response = QueryResponse::new(fields_arc, row_stream);
                let response = Response::Query(query_response);

                tracing::info!("Returned {} rows from upstream", row_count);
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

    async fn do_query<'a, C>(
        &self,
        _client: &mut C,
        _portal: &'a pgwire::api::portal::Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Err(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "0A000".to_owned(),
            "Extended query protocol is not supported. Use simple query protocol instead.".to_owned(),
        ))))
    }

    async fn do_describe_statement<C>(
        &self,
        _client: &mut C,
        _statement: &pgwire::api::stmt::StoredStatement<Self::Statement>,
    ) -> PgWireResult<DescribeStatementResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Err(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "0A000".to_owned(),
            "Extended query protocol is not supported.".to_owned(),
        ))))
    }

    async fn do_describe_portal<C>(
        &self,
        _client: &mut C,
        _portal: &pgwire::api::portal::Portal<Self::Statement>,
    ) -> PgWireResult<DescribePortalResponse>
    where
        C: ClientInfo + ClientPortalStore + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::PortalStore: PortalStore<Statement = Self::Statement>,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        Err(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "0A000".to_owned(),
            "Extended query protocol is not supported.".to_owned(),
        ))))
    }
}

// Simple startup handler that accepts any connection
pub struct ProxyStartupHandler;

#[async_trait]
impl StartupHandler for ProxyStartupHandler {
    async fn on_startup<C>(
        &self,
        _client: &mut C,
        _message: PgWireFrontendMessage,
    ) -> PgWireResult<()>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        // Accept all connections without authentication
        Ok(())
    }
}

impl ServerParameterProvider for ProxyStartupHandler {
    fn server_parameters<C>(&self, _client: &C) -> Option<std::collections::HashMap<String, String>>
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
