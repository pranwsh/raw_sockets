//! Embedded persistent store (redb) with non-blocking offload.
//!
//! The [`Store`] handle is `Send + Sync` and `Clone`. It communicates with a
//! dedicated background thread that owns the `redb::Database`. All operations
//! are dispatched through a channel and acknowledged via oneshot response
//! channels, keeping the event loop thread free of synchronous I/O.
//!
//! The background thread is a simple poll loop over an incoming channel.
//! It processes one operation at a time — redb is single-writer already, so
//! serializing at this level adds no contention that wouldn't exist anyway.

#![forbid(unsafe_code)]

use redb::{Database, ReadableTable, TableDefinition};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

// ---------------------------------------------------------------------------
// Table definitions
// ---------------------------------------------------------------------------

static TABLE_ACCOUNTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts_v1");
static TABLE_CONVERSATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conversations_v1");
static TABLE_INBOX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("inbox_v1");
#[allow(dead_code)]
static TABLE_MESSAGE_LOG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("message_log_v1");
static TABLE_SEQUENCE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("sequence_v1");

// ---------------------------------------------------------------------------
// Op enum
// ---------------------------------------------------------------------------

enum Op {
    PutAccount { user_id: Vec<u8>, data: Vec<u8>, tx: Sender<Result<(), StoreError>> },
    GetAccount { user_id: Vec<u8>, tx: Sender<Result<Option<Vec<u8>>, StoreError>> },
    PutConversation { conv_id: Vec<u8>, data: Vec<u8>, tx: Sender<Result<(), StoreError>> },
    GetConversation { conv_id: Vec<u8>, tx: Sender<Result<Option<Vec<u8>>, StoreError>> },
    NextSequence { conv_id: Vec<u8>, tx: Sender<Result<u64, StoreError>> },
    PutInbox { key: Vec<u8>, data: Vec<u8>, tx: Sender<Result<(), StoreError>> },
    GetInboxRange { prefix: Vec<u8>, limit: u32, tx: Sender<Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError>> },
    DeleteInbox { key: Vec<u8>, tx: Sender<Result<(), StoreError>> },
    Shutdown,
}

// ---------------------------------------------------------------------------
// StoreError
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum StoreError {
    Redb(String),
    Channel,
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redb(m) => write!(f, "redb: {m}"),
            Self::Channel => write!(f, "store channel closed"),
        }
    }
}

impl std::error::Error for StoreError {}

// ---------------------------------------------------------------------------
// Store handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Store {
    tx: Sender<Op>,
}

impl Store {
    /// Open (or create) the database at `path` and spawn the background
    /// worker thread. Returns the handle.
    pub fn open(path: &str) -> Result<Self, StoreError> {
        let db = Database::create(path).map_err(|e| StoreError::Redb(e.to_string()))?;
        let (tx, rx) = mpsc::channel::<Op>();
        let _handle = spawn_worker(db, rx);
        Ok(Self { tx })
    }

    fn send(&self, op: Op) -> Result<(), StoreError> {
        self.tx.send(op).map_err(|_| StoreError::Channel)
    }

    // ---- Account ----

    pub fn put_account(&self, user_id: &[u8], data: &[u8]) -> Result<(), StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::PutAccount { user_id: user_id.to_vec(), data: data.to_vec(), tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    pub fn get_account(&self, user_id: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::GetAccount { user_id: user_id.to_vec(), tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    // ---- Conversation ----

    pub fn put_conversation(&self, conv_id: &[u8], data: &[u8]) -> Result<(), StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::PutConversation { conv_id: conv_id.to_vec(), data: data.to_vec(), tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    pub fn get_conversation(&self, conv_id: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::GetConversation { conv_id: conv_id.to_vec(), tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    // ---- Sequence ----

    pub fn next_sequence(&self, conv_id: &[u8]) -> Result<u64, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::NextSequence { conv_id: conv_id.to_vec(), tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    // ---- Inbox ----

    pub fn put_inbox(&self, key: &[u8], data: &[u8]) -> Result<(), StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::PutInbox { key: key.to_vec(), data: data.to_vec(), tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    pub fn get_inbox_range(&self, prefix: &[u8], limit: u32) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::GetInboxRange { prefix: prefix.to_vec(), limit, tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    pub fn delete_inbox(&self, key: &[u8]) -> Result<(), StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::DeleteInbox { key: key.to_vec(), tx })?;
        rx.recv().map_err(|_| StoreError::Channel)?
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(Op::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

fn spawn_worker(db: Database, rx: Receiver<Op>) -> JoinHandle<()> {
    thread::Builder::new()
        .name("store-worker".into())
        .spawn(move || {
            // Ensure all tables exist (write transaction auto-creates).
            if let Ok(txn) = db.begin_write() {
                let _ = txn.open_table(TABLE_ACCOUNTS);
                let _ = txn.open_table(TABLE_CONVERSATIONS);
                let _ = txn.open_table(TABLE_INBOX);
                let _ = txn.open_table(TABLE_MESSAGE_LOG);
                let _ = txn.open_table(TABLE_SEQUENCE);
                let _ = txn.commit();
            } else {
                eprintln!("store: failed to initialize tables");
            }
            worker_loop(db, rx)
        })
        .expect("spawn store worker")
}

fn worker_loop(db: Database, rx: Receiver<Op>) {
    loop {
        match rx.recv() {
            Ok(op) => {
                if !process_op(&db, op) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

fn db_err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Redb(e.to_string())
}

fn process_op(db: &Database, op: Op) -> bool {
    match op {
        Op::Shutdown => return false,

        Op::PutAccount { user_id, data, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let txn = db.begin_write().map_err(db_err)?;
                {
                    let mut table = txn.open_table(TABLE_ACCOUNTS).map_err(db_err)?;
                    table.insert(user_id.as_slice(), data.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(result);
        }

        Op::GetAccount { user_id, tx } => {
            let result = (|| -> Result<Option<Vec<u8>>, StoreError> {
                let txn = db.begin_read().map_err(db_err)?;
                let table = txn.open_table(TABLE_ACCOUNTS).map_err(db_err)?;
                let value = table.get(user_id.as_slice()).map_err(db_err)?;
                Ok(value.map(|v| v.value().to_vec()))
            })();
            let _ = tx.send(result);
        }

        Op::PutConversation { conv_id, data, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let txn = db.begin_write().map_err(db_err)?;
                {
                    let mut table = txn.open_table(TABLE_CONVERSATIONS).map_err(db_err)?;
                    table.insert(conv_id.as_slice(), data.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(result);
        }

        Op::GetConversation { conv_id, tx } => {
            let result = (|| -> Result<Option<Vec<u8>>, StoreError> {
                let txn = db.begin_read().map_err(db_err)?;
                let table = txn.open_table(TABLE_CONVERSATIONS).map_err(db_err)?;
                let value = table.get(conv_id.as_slice()).map_err(db_err)?;
                Ok(value.map(|v| v.value().to_vec()))
            })();
            let _ = tx.send(result);
        }

        Op::NextSequence { conv_id, tx } => {
            let result = (|| -> Result<u64, StoreError> {
                let txn = db.begin_write().map_err(db_err)?;
                let next = {
                    let mut table = txn.open_table(TABLE_SEQUENCE).map_err(db_err)?;
                    let key = conv_id.as_slice();
                    let n = match table.get(key).map_err(db_err)? {
                        Some(v) => {
                            let bytes = v.value();
                            if bytes.len() < 8 {
                                return Err(StoreError::Redb("corrupt sequence".into()));
                            }
                            let mut arr = [0u8; 8];
                            arr.copy_from_slice(&bytes[..8]);
                            u64::from_le_bytes(arr) + 1
                        }
                        None => 1,
                    };
                    let seq_bytes = n.to_le_bytes();
                    table.insert(key, &seq_bytes[..]).map_err(db_err)?;
                    n
                };
                txn.commit().map_err(db_err)?;
                Ok(next)
            })();
            let _ = tx.send(result);
        }

        Op::PutInbox { key, data, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let txn = db.begin_write().map_err(db_err)?;
                {
                    let mut table = txn.open_table(TABLE_INBOX).map_err(db_err)?;
                    table.insert(key.as_slice(), data.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(result);
        }

        Op::GetInboxRange { prefix, limit, tx } => {
            let result = (|| -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
                let txn = db.begin_read().map_err(db_err)?;
                let table = txn.open_table(TABLE_INBOX).map_err(db_err)?;
                let range = table.range::<&[u8]>(prefix.as_slice()..).map_err(db_err)?;
                let items: Vec<(Vec<u8>, Vec<u8>)> = range
                    .take(limit as usize)
                    .filter_map(|r| r.ok())
                    .map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
                    .collect();
                Ok(items)
            })();
            let _ = tx.send(result);
        }

        Op::DeleteInbox { key, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let txn = db.begin_write().map_err(db_err)?;
                {
                    let mut table = txn.open_table(TABLE_INBOX).map_err(db_err)?;
                    table.remove(key.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(result);
        }
    }
    true
}
