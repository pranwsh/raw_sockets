//! embedded persistent store (redb) with non-blocking offload

#![forbid(unsafe_code)]

use redb::{Database, ReadableTable, TableDefinition};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub use redb::Durability;

// table definitions

static TABLE_ACCOUNTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("accounts_v1");
static TABLE_CONVERSATIONS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conversations_v1");
static TABLE_INBOX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("inbox_v1");
static TABLE_SEQUENCE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("sequence_v1");

// StoreResult — type-erased response so every async method returns the same

pub type InboxItem = (Vec<u8>, Vec<u8>);

#[derive(Debug)]
pub enum StoreResult {
    Account(Result<Option<Vec<u8>>, StoreError>),
    Conversation(Result<Option<Vec<u8>>, StoreError>),
    Sequence(Result<u64, StoreError>),
    InboxRange(Result<Vec<InboxItem>, StoreError>),
    Stored(Result<(), StoreError>),
}

// op enum

enum Op {
    PutAccount { user_id: Vec<u8>, data: Vec<u8>, tx: Sender<StoreResult> },
    GetAccount { user_id: Vec<u8>, tx: Sender<StoreResult> },
    PutConversation { conv_id: Vec<u8>, data: Vec<u8>, tx: Sender<StoreResult> },
    GetConversation { conv_id: Vec<u8>, tx: Sender<StoreResult> },
    NextSequence { conv_id: Vec<u8>, tx: Sender<StoreResult> },
    PutInbox { key: Vec<u8>, data: Vec<u8>, tx: Sender<StoreResult> },
    GetInboxRange { prefix: Vec<u8>, limit: u32, tx: Sender<StoreResult> },
    DeleteInbox { key: Vec<u8>, tx: Sender<StoreResult> },
}

// StoreError

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

// store handle

/// wake callback: invoked by the worker after each op so the reactor's epoll
/// loop can return immediately and drain the result instead of waiting out its
/// poll timeout
pub type Notify = Option<Arc<dyn Fn() + Send + Sync>>;

#[derive(Clone)]
pub struct Store {
    tx: Sender<Op>,
}

impl Store {
    /// open (or create) the database at path and spawn the background worker thread.
    /// `notify` wakes the reactor when a result is ready; `durable` selects the
    /// write-commit durability (Eventual skips the per-commit fsync).
    pub fn open(path: &str, notify: Notify, durable: Durability) -> Result<Self, StoreError> {
        let db = Database::create(path).map_err(|e| StoreError::Redb(e.to_string()))?;
        let (tx, rx) = mpsc::channel::<Op>();
        let _handle = spawn_worker(db, rx, notify, durable);
        Ok(Self { tx })
    }

    fn send(&self, op: Op) -> Result<(), StoreError> {
        self.tx.send(op).map_err(|_| StoreError::Channel)
    }

    // async API — returns immediately, caller polls with try_recv()

    pub fn put_account_async(&self, user_id: &[u8], data: &[u8]) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::PutAccount { user_id: user_id.to_vec(), data: data.to_vec(), tx })?;
        Ok(rx)
    }

    pub fn get_account_async(&self, user_id: &[u8]) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::GetAccount { user_id: user_id.to_vec(), tx })?;
        Ok(rx)
    }

    pub fn put_conversation_async(&self, conv_id: &[u8], data: &[u8]) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::PutConversation { conv_id: conv_id.to_vec(), data: data.to_vec(), tx })?;
        Ok(rx)
    }

    pub fn get_conversation_async(&self, conv_id: &[u8]) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::GetConversation { conv_id: conv_id.to_vec(), tx })?;
        Ok(rx)
    }

    pub fn next_sequence_async(&self, conv_id: &[u8]) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::NextSequence { conv_id: conv_id.to_vec(), tx })?;
        Ok(rx)
    }

    pub fn put_inbox_async(&self, key: &[u8], data: &[u8]) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::PutInbox { key: key.to_vec(), data: data.to_vec(), tx })?;
        Ok(rx)
    }

    pub fn get_inbox_range_async(&self, prefix: &[u8], limit: u32) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::GetInboxRange { prefix: prefix.to_vec(), limit, tx })?;
        Ok(rx)
    }

    pub fn delete_inbox_async(&self, key: &[u8]) -> Result<Receiver<StoreResult>, StoreError> {
        let (tx, rx) = mpsc::channel();
        self.send(Op::DeleteInbox { key: key.to_vec(), tx })?;
        Ok(rx)
    }
}

// worker thread

fn spawn_worker(db: Database, rx: Receiver<Op>, notify: Notify, durable: Durability) -> JoinHandle<()> {
    thread::Builder::new()
        .name("store-worker".into())
        .spawn(move || {
            // ensure all tables exist (write transaction auto-creates)
            if let Ok(mut txn) = db.begin_write() {
                txn.set_durability(durable);
                let _ = txn.open_table(TABLE_ACCOUNTS);
                let _ = txn.open_table(TABLE_CONVERSATIONS);
                let _ = txn.open_table(TABLE_INBOX);
                let _ = txn.open_table(TABLE_SEQUENCE);
                let _ = txn.commit();
            } else {
                eprintln!("store: failed to initialize tables");
            }
            worker_loop(db, rx, notify, durable)
        })
        .expect("spawn store worker")
}

fn worker_loop(db: Database, rx: Receiver<Op>, notify: Notify, durable: Durability) {
    loop {
        match rx.recv() {
            Ok(op) => {
                process_op(&db, op, durable);
                if let Some(f) = &notify {
                    f();
                }
            }
            Err(_) => return,
        }
    }
}

fn db_err<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Redb(e.to_string())
}

fn process_op(db: &Database, op: Op, durable: Durability) {
    match op {
        Op::PutAccount { user_id, data, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let mut txn = db.begin_write().map_err(db_err)?;
                txn.set_durability(durable);
                {
                    let mut table = txn.open_table(TABLE_ACCOUNTS).map_err(db_err)?;
                    table.insert(user_id.as_slice(), data.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(StoreResult::Stored(result));
        }

        Op::GetAccount { user_id, tx } => {
            let result = (|| -> Result<Option<Vec<u8>>, StoreError> {
                let txn = db.begin_read().map_err(db_err)?;
                let table = txn.open_table(TABLE_ACCOUNTS).map_err(db_err)?;
                let value = table.get(user_id.as_slice()).map_err(db_err)?;
                Ok(value.map(|v| v.value().to_vec()))
            })();
            let _ = tx.send(StoreResult::Account(result));
        }

        Op::PutConversation { conv_id, data, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let mut txn = db.begin_write().map_err(db_err)?;
                txn.set_durability(durable);
                {
                    let mut table = txn.open_table(TABLE_CONVERSATIONS).map_err(db_err)?;
                    table.insert(conv_id.as_slice(), data.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(StoreResult::Stored(result));
        }

        Op::GetConversation { conv_id, tx } => {
            let result = (|| -> Result<Option<Vec<u8>>, StoreError> {
                let txn = db.begin_read().map_err(db_err)?;
                let table = txn.open_table(TABLE_CONVERSATIONS).map_err(db_err)?;
                let value = table.get(conv_id.as_slice()).map_err(db_err)?;
                Ok(value.map(|v| v.value().to_vec()))
            })();
            let _ = tx.send(StoreResult::Conversation(result));
        }

        Op::NextSequence { conv_id, tx } => {
            let result = (|| -> Result<u64, StoreError> {
                let mut txn = db.begin_write().map_err(db_err)?;
                txn.set_durability(durable);
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
            let _ = tx.send(StoreResult::Sequence(result));
        }

        Op::PutInbox { key, data, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let mut txn = db.begin_write().map_err(db_err)?;
                txn.set_durability(durable);
                {
                    let mut table = txn.open_table(TABLE_INBOX).map_err(db_err)?;
                    table.insert(key.as_slice(), data.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(StoreResult::Stored(result));
        }

        Op::GetInboxRange { prefix, limit, tx } => {
            let result = (|| -> Result<Vec<InboxItem>, StoreError> {
                let txn = db.begin_read().map_err(db_err)?;
                let table = txn.open_table(TABLE_INBOX).map_err(db_err)?;
                let range = table.range::<&[u8]>(prefix.as_slice()..).map_err(db_err)?;
                let items: Vec<InboxItem> = range
                    .take(limit as usize)
                    .filter_map(|r| r.ok())
                    .map(|(k, v)| (k.value().to_vec(), v.value().to_vec()))
                    .collect();
                Ok(items)
            })();
            let _ = tx.send(StoreResult::InboxRange(result));
        }

        Op::DeleteInbox { key, tx } => {
            let result = (|| -> Result<(), StoreError> {
                let mut txn = db.begin_write().map_err(db_err)?;
                txn.set_durability(durable);
                {
                    let mut table = txn.open_table(TABLE_INBOX).map_err(db_err)?;
                    table.remove(key.as_slice()).map_err(db_err)?;
                }
                txn.commit().map_err(db_err)?;
                Ok(())
            })();
            let _ = tx.send(StoreResult::Stored(result));
        }
    }
}
