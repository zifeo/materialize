// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! Per-connection configuration parameters and state.

#![warn(missing_docs)]

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::mem;

use chrono::{DateTime, Utc};
use derivative::Derivative;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use tokio::sync::OwnedMutexGuard;

use expr::GlobalId;
use pgrepr::Format;
use repr::{Datum, Diff, Row, ScalarType, Timestamp};
use sql::ast::{Raw, Statement};
use sql::plan::{Params, PlanContext, StatementDesc};

use crate::error::CoordError;

mod vars;

pub use self::vars::{Var, Vars};

const DUMMY_CONNECTION_ID: u32 = 0;

/// A session holds per-connection state.
#[derive(Debug)]
pub struct Session {
    conn_id: u32,
    prepared_statements: HashMap<String, PreparedStatement>,
    portals: HashMap<String, Portal>,
    transaction: TransactionStatus,
    pcx: Option<PlanContext>,
    user: String,
    vars: Vars,
    drop_sinks: Vec<GlobalId>,
}

impl Session {
    /// Creates a new session for the specified connection ID.
    pub fn new(conn_id: u32, user: String) -> Session {
        assert_ne!(conn_id, DUMMY_CONNECTION_ID);
        Self::new_internal(conn_id, user)
    }

    /// Creates a new dummy session.
    ///
    /// Dummy sessions are intended for use when executing queries on behalf of
    /// the system itself, rather than on behalf of a user.
    pub fn dummy() -> Session {
        Self::new_internal(DUMMY_CONNECTION_ID, "mz_system".into())
    }

    fn new_internal(conn_id: u32, user: String) -> Session {
        Session {
            conn_id,
            transaction: TransactionStatus::Default,
            pcx: None,
            prepared_statements: HashMap::new(),
            portals: HashMap::new(),
            user,
            vars: Vars::default(),
            drop_sinks: vec![],
        }
    }

    /// Returns the connection ID associated with the session.
    pub fn conn_id(&self) -> u32 {
        self.conn_id
    }

    /// Returns the current transaction's PlanContext. Panics if there is not a
    /// current transaction.
    pub fn pcx(&self) -> &PlanContext {
        &self.transaction().inner().unwrap().pcx
    }

    /// Starts an explicit transaction, or changes an implicit to an explicit
    /// transaction.
    pub fn start_transaction(mut self, wall_time: DateTime<Utc>) -> Self {
        match self.transaction {
            TransactionStatus::Default | TransactionStatus::Started(_) => {
                self.transaction = TransactionStatus::InTransaction(Transaction {
                    pcx: PlanContext::new(wall_time),
                    ops: TransactionOps::None,
                    write_lock_guard: None,
                });
            }
            TransactionStatus::InTransactionImplicit(txn) => {
                self.transaction = TransactionStatus::InTransaction(txn);
            }
            TransactionStatus::InTransaction(_) => {}
            TransactionStatus::Failed(_) => unreachable!(),
        };
        self
    }

    /// Starts either a single statement or implicit transaction based on the
    /// number of statements, but only if no transaction has been started already.
    pub fn start_transaction_implicit(mut self, wall_time: DateTime<Utc>, stmts: usize) -> Self {
        if let TransactionStatus::Default = self.transaction {
            let txn = Transaction {
                pcx: PlanContext::new(wall_time),
                ops: TransactionOps::None,
                write_lock_guard: None,
            };
            match stmts {
                1 => self.transaction = TransactionStatus::Started(txn),
                n if n > 1 => self.transaction = TransactionStatus::InTransactionImplicit(txn),
                _ => {}
            }
        }
        self
    }

    /// Clears a transaction, setting its state to Default and destroying all
    /// portals. Returned are:
    /// - sinks that were started in this transaction and need to be dropped
    /// - the cleared transaction so its operations can be handled
    ///
    /// The [Postgres protocol docs](https://www.postgresql.org/docs/current/protocol-flow.html#PROTOCOL-FLOW-EXT-QUERY) specify:
    /// > a named portal object lasts till the end of the current transaction
    /// and
    /// > An unnamed portal is destroyed at the end of the transaction
    #[must_use]
    pub fn clear_transaction(&mut self) -> (Vec<GlobalId>, TransactionStatus) {
        self.portals.clear();
        self.pcx = None;
        let drop_sinks = mem::take(&mut self.drop_sinks);
        let txn = mem::take(&mut self.transaction);
        (drop_sinks, txn)
    }

    /// Marks the current transaction as failed.
    pub fn fail_transaction(mut self) -> Self {
        match self.transaction {
            TransactionStatus::Default => unreachable!(),
            TransactionStatus::Started(txn)
            | TransactionStatus::InTransactionImplicit(txn)
            | TransactionStatus::InTransaction(txn) => {
                self.transaction = TransactionStatus::Failed(txn);
            }
            TransactionStatus::Failed(_) => {}
        };
        self
    }

    /// Returns the current transaction status.
    pub fn transaction(&self) -> &TransactionStatus {
        &self.transaction
    }

    /// Adds operations to the current transaction. An error is produced if they
    /// cannot be merged (i.e., a read cannot be merged to an insert).
    pub fn add_transaction_ops(&mut self, add_ops: TransactionOps) -> Result<(), CoordError> {
        match &mut self.transaction {
            TransactionStatus::Started(Transaction { ops, .. })
            | TransactionStatus::InTransaction(Transaction { ops, .. })
            | TransactionStatus::InTransactionImplicit(Transaction { ops, .. }) => match ops {
                TransactionOps::None => *ops = add_ops,
                TransactionOps::Peeks(txn_ts) => match add_ops {
                    TransactionOps::Peeks(add_ts) => {
                        assert_eq!(*txn_ts, add_ts);
                    }
                    _ => return Err(CoordError::ReadOnlyTransaction),
                },
                TransactionOps::Tail => return Err(CoordError::TailOnlyTransaction),
                TransactionOps::Writes(txn_writes) => match add_ops {
                    TransactionOps::Writes(mut add_writes) => {
                        txn_writes.append(&mut add_writes);
                    }
                    _ => {
                        return Err(CoordError::WriteOnlyTransaction);
                    }
                },
            },
            TransactionStatus::Default | TransactionStatus::Failed(_) => {
                unreachable!()
            }
        }
        Ok(())
    }

    /// Adds a sink that will need to be dropped when the current transaction is
    /// cleared.
    pub fn add_drop_sink(&mut self, name: GlobalId) {
        self.drop_sinks.push(name);
    }

    /// Assumes an active transaction. Returns its read timestamp. Errors if not
    /// a read transaction. Calls get_ts to get a timestamp if the transaction
    /// doesn't have an operation yet, converting the transaction to a read.
    pub fn get_transaction_timestamp<F: FnMut() -> Result<Timestamp, CoordError>>(
        &mut self,
        mut get_ts: F,
    ) -> Result<Timestamp, CoordError> {
        // If the transaction already has a peek timestamp, use it. Otherwise generate
        // one. We generate one even though we could check here that the transaction
        // isn't in some other conflicting state because we want all of that logic to
        // reside in add_transaction_ops.
        let ts = match self.transaction.inner() {
            Some(Transaction {
                pcx: _,
                ops: TransactionOps::Peeks(ts),
                write_lock_guard: _,
            }) => *ts,
            _ => get_ts()?,
        };
        self.add_transaction_ops(TransactionOps::Peeks(ts))?;
        Ok(ts)
    }

    /// Registers the prepared statement under `name`.
    pub fn set_prepared_statement(&mut self, name: String, statement: PreparedStatement) {
        self.prepared_statements.insert(name, statement);
    }

    /// Removes the prepared statement associated with `name`.
    ///
    /// Returns whether a statement previously existed.
    pub fn remove_prepared_statement(&mut self, name: &str) -> bool {
        self.prepared_statements.remove(name).is_some()
    }

    /// Removes all prepared statements.
    pub fn remove_all_prepared_statements(&mut self) {
        self.prepared_statements.clear();
    }

    /// Retrieves the prepared statement associated with `name`.
    pub fn get_prepared_statement(&self, name: &str) -> Option<&PreparedStatement> {
        self.prepared_statements.get(name)
    }

    /// Returns the prepared statements for the session.
    pub fn prepared_statements(&self) -> &HashMap<String, PreparedStatement> {
        &self.prepared_statements
    }

    /// Binds the specified portal to the specified prepared statement.
    ///
    /// If the prepared statement contains parameters, the values and types of
    /// those parameters must be provided in `params`. It is the caller's
    /// responsibility to ensure that the correct number of parameters is
    /// provided.
    ///
    // The `results_formats` parameter sets the desired format of the results,
    /// and is stored on the portal.
    pub fn set_portal(
        &mut self,
        portal_name: String,
        desc: StatementDesc,
        stmt: Option<Statement<Raw>>,
        params: Vec<(Datum, ScalarType)>,
        result_formats: Vec<pgrepr::Format>,
    ) -> Result<(), CoordError> {
        // The empty portal can be silently replaced.
        if !portal_name.is_empty() && self.portals.contains_key(&portal_name) {
            return Err(CoordError::DuplicateCursor(portal_name));
        }
        self.portals.insert(
            portal_name,
            Portal {
                stmt,
                desc,
                parameters: Params {
                    datums: Row::pack(params.iter().map(|(d, _t)| d)),
                    types: params.into_iter().map(|(_d, t)| t).collect(),
                },
                result_formats: result_formats.into_iter().map(Into::into).collect(),
                state: PortalState::NotStarted,
            },
        );
        Ok(())
    }

    /// Removes the specified portal.
    ///
    /// If there is no such portal, this method does nothing. Returns whether that portal existed.
    pub fn remove_portal(&mut self, portal_name: &str) -> bool {
        self.portals.remove(portal_name).is_some()
    }

    /// Retrieves a reference to the specified portal.
    ///
    /// If there is no such portal, returns `None`.
    pub fn get_portal(&self, portal_name: &str) -> Option<&Portal> {
        self.portals.get(portal_name)
    }

    /// Retrieves a mutable reference to the specified portal.
    ///
    /// If there is no such portal, returns `None`.
    pub fn get_portal_mut(&mut self, portal_name: &str) -> Option<&mut Portal> {
        self.portals.get_mut(portal_name)
    }

    /// Creates and installs a new portal.
    pub fn create_new_portal(
        &mut self,
        stmt: Option<Statement<Raw>>,
        desc: StatementDesc,
        parameters: Params,
        result_formats: Vec<Format>,
    ) -> Result<String, CoordError> {
        // See: https://github.com/postgres/postgres/blob/84f5c2908dad81e8622b0406beea580e40bb03ac/src/backend/utils/mmgr/portalmem.c#L234

        for i in 0usize.. {
            let name = format!("<unnamed portal {}>", i);
            match self.portals.entry(name.clone()) {
                Entry::Occupied(_) => continue,
                Entry::Vacant(entry) => {
                    entry.insert(Portal {
                        stmt,
                        desc,
                        parameters,
                        result_formats,
                        state: PortalState::NotStarted,
                    });
                    return Ok(name);
                }
            }
        }

        coord_bail!("unable to create a new portal");
    }

    /// Resets the session to its initial state. Returns sinks that need to be
    /// dropped.
    pub fn reset(&mut self) -> Vec<GlobalId> {
        let (drop_sinks, _) = self.clear_transaction();
        self.prepared_statements.clear();
        self.vars = Vars::default();
        drop_sinks
    }

    /// Returns the name of the user who owns this session.
    pub fn user(&self) -> &str {
        &self.user
    }

    /// Returns a reference to the variables in this session.
    pub fn vars(&self) -> &Vars {
        &self.vars
    }

    /// Returns a mutable reference to the variables in this session.
    pub fn vars_mut(&mut self) -> &mut Vars {
        &mut self.vars
    }

    /// Grants the coordinator's write lock guard to this session's inner
    /// transaction.
    ///
    /// # Panics
    /// If the inner transaction is idle. See
    /// [`TransactionStatus::grant_write_lock`].
    pub fn grant_write_lock(&mut self, guard: OwnedMutexGuard<()>) {
        self.transaction.grant_write_lock(guard);
    }

    /// Returns whether or not this session currently holds the write lock.
    pub fn has_write_lock(&self) -> bool {
        match self.transaction.inner() {
            None => false,
            Some(txn) => txn.write_lock_guard.is_some(),
        }
    }
}

/// A prepared statement.
#[derive(Debug)]
pub struct PreparedStatement {
    sql: Option<Statement<Raw>>,
    desc: StatementDesc,
}

impl PreparedStatement {
    /// Constructs a new prepared statement.
    pub fn new(sql: Option<Statement<Raw>>, desc: StatementDesc) -> PreparedStatement {
        PreparedStatement { sql, desc }
    }

    /// Returns the raw SQL string associated with this prepared statement,
    /// if the prepared statement was not the empty query.
    pub fn sql(&self) -> Option<&Statement<Raw>> {
        self.sql.as_ref()
    }

    /// Returns the description of the prepared statement.
    pub fn desc(&self) -> &StatementDesc {
        &self.desc
    }
}

/// A portal represents the execution state of a running or runnable query.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct Portal {
    /// The statement that is bound to this portal.
    pub stmt: Option<Statement<Raw>>,
    /// The statement description.
    pub desc: StatementDesc,
    /// The bound values for the parameters in the prepared statement, if any.
    pub parameters: Params,
    /// The desired output format for each column in the result set.
    pub result_formats: Vec<pgrepr::Format>,
    /// The execution state of the portal.
    #[derivative(Debug = "ignore")]
    pub state: PortalState,
}

/// Execution states of a portal.
pub enum PortalState {
    /// Portal not yet started.
    NotStarted,
    /// Portal is a rows-returning statement in progress with 0 or more rows
    /// remaining.
    InProgress(Option<InProgressRows>),
    /// Portal has completed and should not be re-executed. If the optional string
    /// is present, it is returned as a CommandComplete tag, otherwise an error
    /// is sent.
    Completed(Option<String>),
}

/// State of an in-progress, rows-returning portal.
pub struct InProgressRows {
    /// The current batch of rows.
    pub current: Option<Vec<Row>>,
    /// A stream from which to fetch more row batches.
    pub remaining: RowBatchStream,
}

impl InProgressRows {
    /// Creates a new InProgressRows from a batch stream.
    pub fn new(remaining: RowBatchStream) -> Self {
        Self {
            current: None,
            remaining,
        }
    }

    /// Creates a new InProgressRows from a single batch of rows.
    pub fn single_batch(rows: Vec<Row>) -> Self {
        let (_tx, rx) = unbounded_channel();
        Self {
            current: Some(rows),
            remaining: rx,
        }
    }
}

/// A channel of batched rows.
pub type RowBatchStream = UnboundedReceiver<Vec<Row>>;

/// The transaction status of a session.
///
/// PostgreSQL's transaction states are in backend/access/transam/xact.c.
#[derive(Debug)]
pub enum TransactionStatus {
    /// Idle. Matches `TBLOCK_DEFAULT`.
    Default,
    /// Running a single-query transaction. Matches `TBLOCK_STARTED`.
    Started(Transaction),
    /// Currently in a transaction issued from a `BEGIN`. Matches `TBLOCK_INPROGRESS`.
    InTransaction(Transaction),
    /// Currently in an implicit transaction started from a multi-statement query
    /// with more than 1 statements. Matches `TBLOCK_IMPLICIT_INPROGRESS`.
    InTransactionImplicit(Transaction),
    /// In a failed transaction that was started explicitly (i.e., previously
    /// InTransaction). We do not use Failed for implicit transactions because
    /// those cleanup after themselves. Matches `TBLOCK_ABORT`.
    Failed(Transaction),
}

impl TransactionStatus {
    /// Extracts the inner transaction ops if not failed.
    pub fn into_ops(self) -> Option<TransactionOps> {
        match self {
            TransactionStatus::Default | TransactionStatus::Failed(_) => None,
            TransactionStatus::Started(txn)
            | TransactionStatus::InTransaction(txn)
            | TransactionStatus::InTransactionImplicit(txn) => Some(txn.ops),
        }
    }

    /// Exposes the inner transaction.
    pub fn inner(&self) -> Option<&Transaction> {
        match self {
            TransactionStatus::Default => None,
            TransactionStatus::Started(txn)
            | TransactionStatus::InTransaction(txn)
            | TransactionStatus::InTransactionImplicit(txn)
            | TransactionStatus::Failed(txn) => Some(txn),
        }
    }

    /// Expresses whether or not the transaction was implicitly started.
    /// However, its negation does not imply explicitly started.
    pub fn is_implicit(&self) -> bool {
        match self {
            TransactionStatus::Started(_) | TransactionStatus::InTransactionImplicit(_) => true,
            TransactionStatus::Default
            | TransactionStatus::InTransaction(_)
            | TransactionStatus::Failed(_) => false,
        }
    }

    /// Grants the write lock to the inner transaction.
    ///
    /// # Panics
    /// If `self` is `TransactionStatus::Default`, which indicates that the
    /// transaction is idle, which is not appropriate to assign the
    /// coordinator's write lock to.
    pub fn grant_write_lock(&mut self, guard: OwnedMutexGuard<()>) {
        match self {
            TransactionStatus::Default => panic!("cannot grant write lock to txn not yet started"),
            TransactionStatus::Started(txn)
            | TransactionStatus::InTransaction(txn)
            | TransactionStatus::InTransactionImplicit(txn)
            | TransactionStatus::Failed(txn) => txn.grant_write_lock(guard),
        }
    }
}

impl Default for TransactionStatus {
    fn default() -> Self {
        TransactionStatus::Default
    }
}

/// State data for transactions.
#[derive(Debug)]
pub struct Transaction {
    /// Plan context.
    pub pcx: PlanContext,
    /// Transaction operations.
    pub ops: TransactionOps,
    /// Holds the coordinator's write lock.
    write_lock_guard: Option<OwnedMutexGuard<()>>,
}

impl Transaction {
    /// Grants the write lock to this transaction for the remainder of its lifetime.
    fn grant_write_lock(&mut self, guard: OwnedMutexGuard<()>) {
        self.write_lock_guard = Some(guard);
    }
}

/// The type of operation being performed by the transaction.
///
/// This is needed because we currently do not allow mixing reads and writes in
/// a transaction. Use this to record what we have done, and what may need to
/// happen at commit.
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionOps {
    /// The transaction has been initiated, but no statement has yet been executed
    /// in it.
    None,
    /// This transaction has had a peek (`SELECT`, `TAIL`) and must only do other peeks.
    Peeks(Timestamp),
    /// This transaction has done a TAIL and must do nothing else.
    Tail,
    /// This transaction has had a write (`INSERT`, `UPDATE`, `DELETE`) and must only do
    /// other writes.
    Writes(Vec<WriteOp>),
}

/// An `INSERT` waiting to be committed.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOp {
    /// The target table.
    pub id: GlobalId,
    /// The data rows.
    pub rows: Vec<(Row, Diff)>,
}

/// The action to take during end_transaction.
#[derive(Debug, PartialEq, Eq)]
pub enum EndTransactionAction {
    /// Commit the transaction.
    Commit,
    /// Rollback the transaction.
    Rollback,
}

impl EndTransactionAction {
    /// Returns the pgwire tag for this action.
    pub fn tag(&self) -> &'static str {
        match self {
            EndTransactionAction::Commit => "COMMIT",
            EndTransactionAction::Rollback => "ROLLBACK",
        }
    }
}
