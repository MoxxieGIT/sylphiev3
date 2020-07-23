use async_trait::*;
use crate::migrations::MigrationManager;
use futures::Stream;
use futures_async_stream::*;
use rusqlite::{Connection, Transaction, OpenFlags, TransactionBehavior, AndThenRows, Row, Statement, Rows};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::borrow::Cow;
use std::ops::{Deref, DerefMut, Generator};
use std::marker::PhantomData;
use std::path::{PathBuf, Path};
use std::time;
use std::sync::Arc;
use sylphie_core::core::EarlyInitEvent;
use sylphie_core::prelude::*;
use tokio::runtime::Handle;
use tokio::task;

mod pool;
use pool::{Pool, ManageConnection, PooledConnection};
use parking_lot::Mutex;

struct BlockingWrapper<T: Send + 'static> {
    inner: Option<Box<T>>,
    handle: Arc<Handle>,
}
impl <T: Send + 'static> BlockingWrapper<T> {
    async fn run_blocking<R: Send + 'static>(
        &mut self, func: impl FnOnce(&mut T) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        if self.inner.is_none() {
            bail!("BlockingWrapper is not active, it has probably been poisoned by a Drop.");
        }

        let mut inner = self.inner.take();
        let (result, inner) = self.handle.spawn_blocking(move || {
            let result = func(inner.as_mut().unwrap());
            (result, inner)
        }).await?;
        self.inner = inner;
        result
    }
    fn get(&mut self) -> Result<&mut T> {
        match &mut self.inner {
            Some(x) => Ok(x),
            None => bail!("BlockingWrapper is empty, it has probably been poisoned by a Drop."),
        }
    }
    fn take(&mut self) -> Self {
        BlockingWrapper {
            inner: self.inner.take(),
            handle: self.handle.clone(),
        }
    }
}

struct ConnectionManager {
    db_file: Arc<Path>,
    transient_db_file: Arc<Path>,
    handle: Arc<Handle>,
}
impl ConnectionManager {
    fn new(path: PathBuf, transient_path: PathBuf) -> Result<ConnectionManager> {
        Ok(ConnectionManager {
            db_file: path.into(),
            transient_db_file: transient_path.into(),
            handle: Arc::new(Handle::current()),
        })
    }
}
#[async_trait]
impl ManageConnection for ConnectionManager {
    type Connection = BlockingWrapper<Connection>;
    type Error = ErrorWrapper;

    async fn connect(&self) -> StdResult<BlockingWrapper<Connection>, ErrorWrapper> {
        let db_file = self.db_file.clone();
        let transient_db_file = self.transient_db_file.clone();
        let handle = self.handle.clone();
        Ok(self.handle.spawn_blocking(move || -> Result<_> {
            let conn = Connection::open_with_flags(&db_file,
                OpenFlags::SQLITE_OPEN_READ_WRITE |
                OpenFlags::SQLITE_OPEN_CREATE)?;
            conn.set_prepared_statement_cache_capacity(64);
            conn.execute_batch(include_str!("setup_connection.sql"))?;
            conn.execute(
                r#"ATTACH DATABASE ? AS transient;"#,
                &[transient_db_file.to_str().expect("Could not convert path to str.")],
            )?;
            Ok(BlockingWrapper {
                inner: Some(Box::new(conn)),
                handle,
            })
        }).await.map_err(ErrorWrapper::new)??)
    }
    async fn is_valid(
        &self, conn: &mut BlockingWrapper<Connection>,
    ) -> StdResult<(), ErrorWrapper> {
        Ok(conn.run_blocking(|c| {
            c.prepare_cached("SELECT 1")?.query_row(&[0i32; 0], |_| Ok(()))?;
            Ok(())
        }).await.map_err(ErrorWrapper::new)?)
    }

    fn has_broken(&self, conn: &mut BlockingWrapper<Connection>) -> bool {
        conn.inner.is_some()
    }
}

/// The type of transaction to perform.
///
/// See the Sqlite documentation for more information.
#[derive(Copy, Clone, Debug)]
pub enum TransactionType {
    Deferred,
    Immediate,
    Exclusive,
}

/// The underlying struct that contains database operations. This is obtained via [`DerefMut`] in
/// [`DbConnection`] and [`DbTransaction`].
pub struct DbOps(BlockingWrapper<DbOpsData>);
struct DbOpsData {
    conn_handle: Option<PooledConnection<ConnectionManager>>,
    conn: BlockingWrapper<Connection>,
    is_begin_transaction: bool,
    is_begin_commit: bool,
    is_in_transaction: bool,
    return_cell: Option<Arc<Mutex<Option<BlockingWrapper<Connection>>>>>,
}
impl DbOpsData {
    fn begin_transaction(&mut self, t: TransactionType) -> Result<()> {
        assert!(!self.is_in_transaction);

        let sql = match t {
            TransactionType::Exclusive => "BEGIN EXCLUSIVE TRANSACTION;",
            TransactionType::Immediate => "BEGIN IMMEDIATE TRANSACTION;",
            TransactionType::Deferred => "BEGIN DEFERRED TRANSACTION;",
        };

        self.is_begin_transaction = true;
        self.execute_batch(sql.into())?;
        self.is_in_transaction = true;
        self.is_begin_transaction = false;

        Ok(())
    }
    fn commit_transaction(&mut self) -> Result<()> {
        assert!(self.is_in_transaction);
        self.is_begin_commit = true;
        self.execute_batch("COMMIT;".into())?;
        self.is_in_transaction = false;
        self.is_begin_commit = false;
        Ok(())
    }
    fn rollback_transaction(&mut self) -> Result<()> {
        assert!(self.is_in_transaction);
        self.is_begin_commit = true;
        self.execute_batch("ROLLBACK;".into())?;
        self.is_in_transaction = false;
        self.is_begin_commit = false;
        Ok(())
    }
    fn rollback_in_drop(&mut self) {
        // rollback the transaction in a blocking thread. The connection will only be returned
        // to the pool once this is done.
        //
        // this poisons this DbOps and makes it unusable for further operations.
        let mut conn_handle = self.conn_handle.take().unwrap();
        let conn = self.conn.take();
        self.conn.handle.clone().spawn_blocking(move || {
            match conn.inner.as_ref().unwrap().execute_batch("ROLLBACK;") {
                Ok(_) => *conn_handle = conn,
                Err(e) => Error::from(e).report_error(),
            }
            ::std::mem::drop(conn_handle);
        });
    }
    fn transaction_dropped(&mut self) {
        if self.is_in_transaction {
            self.rollback_in_drop();
        }
    }

    fn execute(
        &mut self, sql: Cow<'static, str>, params: impl Serialize + Send + 'static,
    ) -> Result<usize> {
        let data = serde_rusqlite::to_params(params)?;
        Ok(self.conn.get()?.execute(&sql, &data.to_slice())?)
    }
    fn execute_named(
        &mut self, sql: Cow<'static, str>, params: impl Serialize + Send + 'static,
    ) -> Result<usize> {
        let data = serde_rusqlite::to_params_named(params)?;
        Ok(self.conn.get()?.execute_named(&sql, &data.to_slice())?)
    }
    fn execute_batch(&mut self, sql: Cow<'static, str>) -> Result<()> {
        self.conn.get()?.execute_batch(&sql)?;
        Ok(())
    }

    fn do_query_stream<T: DeserializeOwned + Send + 'static>(
        &mut self,
        sql: Cow<'static, str>,
        query: impl for <'a> FnOnce(&'a mut Statement<'_>) -> Result<Rows<'a>> + Send + 'static,
    ) -> impl Stream<Item = Result<T>> {
        if self.return_cell.is_none() {
            self.return_cell = Some(Arc::new(Mutex::new(None)));
        }

        struct QueryGeneratorData {
            return_cell: Arc<Mutex<Option<BlockingWrapper<Connection>>>>,
            conn: BlockingWrapper<Connection>,
        }
        impl Drop for QueryGeneratorData {
            fn drop(&mut self) {
                *self.return_cell.lock() = Some(self.conn.take());
            }
        }

        let mut gen_data = QueryGeneratorData {
            return_cell: self.return_cell.as_ref().unwrap().clone(),
            conn: self.conn.take(),
        };
        let query_generator = static move || {
            let result: Result<()> = try {
                let mut stat = gen_data.conn.get()?.prepare(&sql)?;
                let columns = serde_rusqlite::columns_from_statement(&stat);
                let mut query = query(&mut stat)?;
                while let Some(row) = query.next()? {
                    yield Ok(serde_rusqlite::from_row_with_columns::<T>(&row, &columns)?);
                }
            };
            if let Err(e) = result {
                yield Err(e);
            }
        };

        #[try_stream(ok = T, error = Error)]
        async fn async_stream<T: DeserializeOwned + Send + 'static>(
            query_generator: impl Generator<Yield = Result<T>> + Send + 'static
        ) {

        }

        async_stream(query_generator)
    }
}
impl Drop for DbOpsData {
    fn drop(&mut self) {
        if self.is_begin_commit || self.is_begin_transaction {
            self.conn_handle.as_mut().unwrap().take();
        } else if self.is_in_transaction {
            self.rollback_in_drop()
        } else {
            if let Some(mut handle) = self.conn_handle.take() {
                *handle = self.conn.take();
            }
        }
    }
}
impl DbOps {
    /// Executes a SQL query with unnamed parameters.
    pub async fn execute(
        &mut self, sql: impl Into<Cow<'static, str>>, params: impl Serialize + Send + 'static,
    ) -> Result<usize> {
        let sql = sql.into();
        self.0.run_blocking(move |c| c.execute(sql, params)).await
    }
    /// Executes a SQL query with no parameters.
    pub async fn execute_nullary(&mut self, sql: impl Into<Cow<'static, str>>) -> Result<usize> {
        let sql = sql.into();
        self.0.run_blocking(move |c| c.execute(sql, ())).await
    }
    /// Executes a SQL query with named parameters.
    pub async fn execute_named(
        &mut self, sql: impl Into<Cow<'static, str>>, params: impl Serialize + Send + 'static,
    ) -> Result<usize> {
        let sql = sql.into();
        self.0.run_blocking(move |c| c.execute_named(sql, params)).await
    }
    /// Executes multiple SQL statements.
    pub async fn execute_batch(&mut self, sql: impl Into<Cow<'static, str>>) -> Result<()> {
        let sql = sql.into();
        self.0.run_blocking(move |c| c.execute_batch(sql)).await
    }
}

/// A connection to the database.
pub struct DbConnection {
    ops: DbOps,
}
impl Deref for DbConnection {
    type Target = DbOps;
    fn deref(&self) -> &Self::Target {
        &self.ops
    }
}
impl DerefMut for DbConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ops
    }
}
impl DbConnection {
    /// Checkpoints the database, dumping the write-ahead log to disk.
    pub async fn checkpoint(&mut self) -> Result<()> {
        self.ops.execute_batch("PRAGMA wal_checkpoint(RESTART);").await
    }

    /// Starts a new deferred transaction.
    ///
    /// The transaction is normally rolled back when it is dropped. If you want to commit the
    /// transaction, you must call [`commit`](`DbTransaction::commit`).
    pub async fn transaction(&mut self) -> Result<DbTransaction<'_>> {
        self.transaction_with_type(TransactionType::Deferred).await
    }

    /// Starts a new transaction of the given type.
    ///
    /// The transaction is normally rolled back when it is dropped. If you want to commit the
    /// transaction, you must call [`commit`](`DbTransaction::commit`).
    pub async fn transaction_with_type(
        &mut self, t: TransactionType,
    ) -> Result<DbTransaction<'_>> {
        self.ops.0.run_blocking(move |c| c.begin_transaction(t)).await?;
        let ops = self.ops.0.take();
        Ok(DbTransaction {
            parent: self,
            ops: DbOps(ops),
        })
    }

    /*
    /// Queries a row of the SQL statements with no parameters.
    pub async fn query_row<T: DeserializeOwned + Send + 'static>(
        &mut self, sql: impl Into<Cow<'static, str>>, params: impl Serialize + Send + 'static,
    ) -> Result<T> {
        self.query_row_0(sql.into(), params).await
    }
    async fn query_row_0<T: DeserializeOwned + Send + 'static>(
        &mut self, sql: Cow<'static, str>, params: impl Serialize + Send + 'static,
    ) -> Result<T> {
        self.conn.run_blocking(move |c| -> Result<T> {
            let data = serde_rusqlite::to_params(params)?;
            let mut stat = c.prepare(&sql)?;
            let mut rows = stat.query_and_then(&data.to_slice(), serde_rusqlite::from_row)?;
            Ok(rows.next().internal_err(|| "No rows returned from query.")??)
        }).await
    }

    /// Queries a row of the SQL statements.
    pub async fn query_row_nullary<T: DeserializeOwned + Send + 'static>(
        &mut self, sql: impl Into<Cow<'static, str>>,
    ) -> Result<T> {
        self.query_row(sql, ()).await
    }

    /// Queries a row of the SQL statements with named parameters.
    pub async fn query_row_named<T: DeserializeOwned + Send + 'static>(
        &mut self, sql: impl Into<Cow<'static, str>>, params: impl Serialize + Send + 'static,
    ) -> Result<T> {
        self.query_row_0(sql.into(), params).await
    }
    async fn query_row_named_0<T: DeserializeOwned + Send + 'static>(
        &mut self, sql: Cow<'static, str>, params: impl Serialize + Send + 'static,
    ) -> Result<T> {
        self.conn.run_blocking(move |c| -> Result<T> {
            let data = serde_rusqlite::to_params_named(params)?;
            let mut stat = c.prepare(&sql)?;
            let mut rows = stat.query_and_then_named(&data.to_slice(), serde_rusqlite::from_row)?;
            Ok(rows.next().internal_err(|| "No rows returned from query.")??)
        }).await
    }
    */
}

pub struct DbTransaction<'a> {
    parent: &'a mut DbConnection,
    ops: DbOps,
}
impl <'a> Deref for DbTransaction<'a> {
    type Target = DbOps;
    fn deref(&self) -> &Self::Target {
        &self.ops
    }
}
impl <'a> DerefMut for DbTransaction<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.ops
    }
}
impl <'a> DbTransaction<'a> {
    /// Commits the transaction.
    pub async fn commit(mut self) -> Result<()> {
        self.ops.0.run_blocking(|c| c.commit_transaction()).await
    }
    /// Rolls back the transaction.
    pub async fn rollback(mut self) -> Result<()> {
        self.ops.0.run_blocking(|c| c.rollback_transaction()).await
    }
}
impl <'a> Drop for DbTransaction<'a> {
    fn drop(&mut self) {
        self.ops.0.get().unwrap().rollback_in_drop()
    }
}

/// Manages connections to the database.
#[derive(Clone)]
pub struct Database {
    pool: Arc<Pool<ConnectionManager>>,
}
impl Database {
    pub async fn connect(&self) -> Result<DbConnection> {
        let mut conn_handle = self.pool.get().await?;
        let conn = conn_handle.take();
        let handle = conn.handle.clone();
        Ok(DbConnection {
            ops: DbOps(BlockingWrapper {
                inner: Some(Box::new(DbOpsData {
                    conn_handle: Some(conn_handle),
                    conn,
                    is_begin_transaction: false,
                    is_begin_commit: false,
                    is_in_transaction: false,
                    return_cell: None,
                })),
                handle,
            }),
        })
    }
}

/// The module that handles database connections and migrations.
///
/// This should be a part of the module tree for database connections and migrations to work
/// correctly.
#[derive(Events)]
pub struct DatabaseModule {
    #[service] database: Database,
    #[service] migrations: MigrationManager,
}
impl DatabaseModule {
    pub fn new(path: PathBuf, transient_path: PathBuf) -> Result<Self> {
        let manager = ConnectionManager::new(path, transient_path)?;
        let pool = Arc::new(Handle::current().block_on(
            Pool::builder()
                .max_size(15)
                .idle_timeout(Some(time::Duration::from_secs(60 * 5)))
                .build(manager)
        )?);
        let database = Database { pool: pool.clone() };
        Ok(DatabaseModule {
            database: database.clone(),
            migrations: MigrationManager::new(database),
        })
    }
}
#[events_impl]
impl DatabaseModule {
    #[event_handler(EvInit)]
    fn init_database(target: &Handler<impl Events>, _: &EarlyInitEvent) {
        crate::kvs::init_kvs(target);
    }
}