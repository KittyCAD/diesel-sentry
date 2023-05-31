//! Add Sentry tracing probes to Diesel connections.
//!
//! The `diesel-sentry` crate provides a diesel [`Connection`] that includes Sentry tracing points.
//! These are fired when a connection to the database is established and for each query.
#![allow(non_snake_case)]

use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

use diesel::{
    backend::Backend,
    connection::{
        AnsiTransactionManager, ConnectionGatWorkaround, LoadConnection, LoadRowIter,
        SimpleConnection, TransactionManager,
    },
    debug_query,
    expression::QueryMetadata,
    prelude::*,
    query_builder::{AsQuery, QueryFragment, QueryId},
    r2d2::R2D2Connection,
};
use sentry::Hub;
use uuid::Uuid;

pub mod phone_number;

// https://dev.mysql.com/doc/refman/8.0/en/information-functions.html
// db.name
diesel::sql_function!(fn DATABASE() -> diesel::sql_types::Text);
// db.version
diesel::sql_function!(fn VERSION() -> diesel::sql_types::Text);

#[derive(Queryable, Clone, Debug, PartialEq, Default)]
struct ConnectionInfo {
    current_database: String,
    version: String,
}

/// A [`Connection`] that includes Sentry tracing points.
#[derive(Debug)]
pub struct SentryConnection<C: Connection> {
    inner: C,
    id: Uuid,
    info: ConnectionInfo,
}

impl<C: Connection> Deref for SentryConnection<C> {
    type Target = C;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<C: Connection> DerefMut for SentryConnection<C> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<C: Connection> SimpleConnection for SentryConnection<C> {
    #[tracing::instrument(
        fields(
            db.name=%self.info.current_database,
            db.system="mysql",
            db.version=%self.info.version,
            db.statement=%query,
            otel.kind="client",
        ),
        skip(self, query),
    )]
    fn batch_execute(&mut self, query: &str) -> QueryResult<()> {
        let mut txn = start_sentry_db_transaction("sql.query", query);

        let result = self.inner.batch_execute(query);
        txn.finish();
        result
    }
}

impl<'conn, 'query, C: Connection> ConnectionGatWorkaround<'conn, 'query, C::Backend>
    for SentryConnection<C>
{
    type Cursor = <C as ConnectionGatWorkaround<'conn, 'query, C::Backend>>::Cursor;
    type Row = <C as ConnectionGatWorkaround<'conn, 'query, C::Backend>>::Row;
}

impl<C> LoadConnection for SentryConnection<C>
where
    C: Connection<TransactionManager = AnsiTransactionManager, Backend = diesel::mysql::Mysql>
        + LoadConnection,
    C::Backend: Default,
    <C::Backend as Backend>::QueryBuilder: Default,
{
    #[tracing::instrument(
        fields(
            db.name=%self.info.current_database,
            db.system="mysql",
            db.version=%self.info.version,
            db.statement=tracing::field::Empty,
            otel.kind="client",
        ),
        skip(self, source),
    )]
    fn load<'conn, 'query, T>(
        &'conn mut self,
        source: T,
    ) -> QueryResult<LoadRowIter<'conn, 'query, Self, Self::Backend>>
    where
        T: AsQuery + QueryFragment<Self::Backend>,
        T::Query: QueryFragment<Self::Backend> + QueryId + 'query,
        Self::Backend: QueryMetadata<T::SqlType>,
    {
        let q = source.as_query();
        let query = debug_query::<Self::Backend, _>(&q).to_string();

        let mut txn = start_sentry_db_transaction("sql.query", &query);
        let span = tracing::Span::current();
        #[allow(clippy::needless_borrow)]
        span.record("db.statement", &query.as_str());

        let result = self.inner.load(q);
        txn.finish();
        result
    }
}

impl<C> Connection for SentryConnection<C>
where
    C: Connection<TransactionManager = AnsiTransactionManager, Backend = diesel::mysql::Mysql>
        + diesel::connection::LoadConnection,
    <C::Backend as Backend>::QueryBuilder: Default,
{
    type Backend = diesel::mysql::Mysql;
    type TransactionManager = C::TransactionManager;

    #[tracing::instrument(
        fields(
            db.name=tracing::field::Empty,
            db.system="mysql",
            db.version=tracing::field::Empty,
            otel.kind="client",
        ),
        skip(database_url),
    )]
    fn establish(database_url: &str) -> ConnectionResult<Self> {
        tracing::debug!("establishing mysql connection");
        let conn_id = Uuid::new_v4();
        let mut txn = start_sentry_db_transaction("connection", "establish");
        let conn = C::establish(database_url);
        let mut inner = conn?;

        tracing::debug!("querying mysql connection information");
        let info: ConnectionInfo = diesel::select((DATABASE(), VERSION()))
            .get_result(&mut inner)
            .map_err(ConnectionError::CouldntSetupConfiguration)?;

        let span = tracing::Span::current();
        // TODO: figure out why clippy thinks this is needless?
        #[allow(clippy::needless_borrow)]
        span.record("db.name", &info.current_database.as_str());
        #[allow(clippy::needless_borrow)]
        span.record("db.version", &info.version.as_str());

        tracing::debug!("db.name: {}", info.current_database);
        tracing::debug!("db.version: {}", info.version);

        txn.finish();

        Ok(SentryConnection {
            inner,
            id: conn_id,
            info,
        })
    }

    #[tracing::instrument(
        fields(
            db.name=%self.info.current_database,
            db.system="mysql",
            db.version=%self.info.version,
            otel.kind="client",
        ),
        skip(self, f),
    )]
    fn transaction<T, E, F>(&mut self, f: F) -> Result<T, E>
    where
        F: FnOnce(&mut Self) -> Result<T, E>,
        E: From<diesel::result::Error>,
    {
        let mut txn = start_sentry_db_transaction("transaction", &self.id.to_string());
        let result = f(self);
        txn.finish();
        result
    }

    #[tracing::instrument(
        fields(
            db.name=%self.info.current_database,
            db.system="mysql",
            db.version=%self.info.version,
            db.statement=tracing::field::Empty,
            otel.kind="client",
        ),
        skip(self, source),
    )]
    fn execute_returning_count<T>(&mut self, source: &T) -> QueryResult<usize>
    where
        T: QueryFragment<Self::Backend> + QueryId,
    {
        let query = debug_query::<Self::Backend, _>(&source).to_string();
        let mut txn = start_sentry_db_transaction("sql.query", &query);
        let span = tracing::Span::current();
        #[allow(clippy::needless_borrow)]
        span.record("db.statement", &query.as_str());

        let result = self.inner.execute_returning_count(source);
        txn.finish();
        result
    }

    #[tracing::instrument(
        fields(
            db.name=%self.info.current_database,
            db.system="mysql",
            db.version=%self.info.version,
            otel.kind="client",
        ),
        skip(self),
    )]
    fn transaction_state(
        &mut self,
    ) -> &mut <C::TransactionManager as TransactionManager<C>>::TransactionStateData {
        self.inner.transaction_state()
    }
}

impl<C> R2D2Connection for SentryConnection<C>
where
    C: R2D2Connection
        + Connection<TransactionManager = AnsiTransactionManager, Backend = diesel::mysql::Mysql>
        + diesel::connection::LoadConnection,
    <C::Backend as Backend>::QueryBuilder: Default,
{
    fn ping(&mut self) -> QueryResult<()> {
        self.inner.ping()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SentryTransaction {
    transaction: Option<sentry::TransactionOrSpan>,
    parent_span: Option<sentry::TransactionOrSpan>,
    hub: Option<Arc<sentry::Hub>>,
    skip: bool,
}

fn start_sentry_db_transaction(op: &str, name: &str) -> SentryTransaction {
    if name.contains(" `Session` ") {
        // Skip all transactions dealing with Session.
        // We just get way too much annoying data this way.
        return SentryTransaction {
            transaction: None,
            parent_span: None,
            hub: None,
            skip: true,
        };
    }

    // Create a new Sentry hub for every request.
    // Ensures the scope stays right.
    // The Clippy lint here is a false positive, the suggestion to write
    // `Hub::with(Hub::new_from_top)` does not compiles:
    //     143 |         Hub::with(Hub::new_from_top).into()
    //         |         ^^^^^^^^^ implementation of `std::ops::FnOnce` is not general enough
    #[allow(clippy::redundant_closure)]
    let hub = Arc::new(Hub::with(|hub| Hub::new_from_top(hub)));

    let trx_ctx = sentry::TransactionContext::new(name, &format!("db.{op}"));

    let mut trx: SentryTransaction = Default::default();

    hub.configure_scope(|scope| {
        let transaction: sentry::TransactionOrSpan = sentry::start_transaction(trx_ctx).into();

        let parent_span = scope.get_span();
        scope.set_span(Some(transaction.clone()));
        trx = SentryTransaction {
            transaction: Some(transaction),
            parent_span,
            hub: Some(hub.clone()),
            skip: false,
        };
    });

    trx
}

impl SentryTransaction {
    pub fn finish(&mut self) {
        if self.skip {
            return;
        }

        let transaction = self.transaction.as_ref().unwrap();
        if transaction.get_status().is_none() {
            transaction.set_status(sentry::protocol::SpanStatus::Ok);
        }
        transaction.clone().finish();

        if let Some(parent_span) = &self.parent_span {
            self.hub.as_ref().unwrap().configure_scope(|scope| {
                scope.set_span(Some(parent_span.clone()));
            });
        }
    }
}
