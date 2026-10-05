//! In-memory [`OtaKv`] with a commit counter and fault injection, for host tests.
//!
//! Clones share one state, so a test keeps a handle, hands a clone to the store, and after an injected fault re-opens a new store from the surviving map.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use super::kv::{KvError, KvStr, OtaKv};

/// The store died here (power loss): this commit applied, nothing after it does.
pub const CRASHED: i32 = -1;
/// A write or read failed by injection.
pub const INJECTED: i32 = -2;

/// A stored value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Val {
    U8(u8),
    U32(u32),
    Str(KvStr),
}

/// Kind of one logged commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Set,
    Erase,
    Remove,
}

/// One logged commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub op: Op,
    pub key: String,
    pub value: Option<Val>,
}

#[derive(Default)]
struct Inner {
    map: BTreeMap<String, Val>,
    log: Vec<LogEntry>,
    commits: usize,
    reads: usize,
    crash_after: Option<usize>,
    fail_at: Option<usize>,
    fail_after_erase: Option<usize>,
    fail_read_at: Option<usize>,
    corrupt: BTreeMap<String, i32>,
    crashed: bool,
}

fn err(code: i32) -> KvError {
    KvError::new(code)
}

impl Inner {
    fn commit(&mut self, op: Op, key: &str, value: Option<Val>) -> Result<usize, KvError> {
        if self.crashed {
            return Err(err(CRASHED));
        }
        self.commits += 1;
        let n = self.commits;
        if self.fail_at == Some(n) {
            return Err(err(INJECTED));
        }
        match (&op, &value) {
            (Op::Set, Some(v)) => {
                // A set replaces a value of any type, so it also heals an injected unreadable key (as `nvs_set_*` does).
                self.map.insert(key.to_string(), v.clone());
                self.corrupt.remove(key);
            }
            _ => {
                self.map.remove(key);
            }
        }
        self.log.push(LogEntry {
            op,
            key: key.to_string(),
            value,
        });
        if self.crash_after == Some(n) {
            self.crashed = true;
            return Err(err(CRASHED));
        }
        Ok(n)
    }

    fn read<T>(&mut self, key: &str, pick: fn(&Val) -> Option<T>) -> Result<Option<T>, KvError> {
        if self.crashed {
            return Err(err(CRASHED));
        }
        self.reads += 1;
        if self.fail_read_at == Some(self.reads) {
            return Err(err(INJECTED));
        }
        if let Some(code) = self.corrupt.get(key) {
            return Err(err(*code));
        }
        match self.map.get(key) {
            None => Ok(None),
            Some(v) => pick(v)
                .map(Some)
                .ok_or_else(|| err(KvError::CODE_TYPE_MISMATCH)),
        }
    }
}

/// Shared-state fault-injecting key-value backend.
#[derive(Clone, Default)]
pub struct FaultKv {
    inner: Rc<RefCell<Inner>>,
}

impl FaultKv {
    pub fn new() -> Self {
        Self::default()
    }

    /// A fresh backend (no faults, empty log) holding `map`: the flash content after a reboot.
    pub fn from_map(map: BTreeMap<String, Val>) -> Self {
        let kv = Self::default();
        kv.inner.borrow_mut().map = map;
        kv
    }

    // ---- fixtures and peeks (no commit, no read counted) ----

    pub fn put_u8(&self, key: &str, v: u8) -> &Self {
        self.inner
            .borrow_mut()
            .map
            .insert(key.to_string(), Val::U8(v));
        self
    }

    pub fn put_u32(&self, key: &str, v: u32) -> &Self {
        self.inner
            .borrow_mut()
            .map
            .insert(key.to_string(), Val::U32(v));
        self
    }

    pub fn put_str(&self, key: &str, v: &str) -> &Self {
        let v = KvStr::try_from(v).expect("fixture string fits");
        self.inner
            .borrow_mut()
            .map
            .insert(key.to_string(), Val::Str(v));
        self
    }

    pub fn u8_of(&self, key: &str) -> Option<u8> {
        match self.inner.borrow().map.get(key) {
            Some(Val::U8(v)) => Some(*v),
            _ => None,
        }
    }

    pub fn u32_of(&self, key: &str) -> Option<u32> {
        match self.inner.borrow().map.get(key) {
            Some(Val::U32(v)) => Some(*v),
            _ => None,
        }
    }

    pub fn str_of(&self, key: &str) -> Option<String> {
        match self.inner.borrow().map.get(key) {
            Some(Val::Str(v)) => Some(v.as_str().to_string()),
            _ => None,
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.inner.borrow().map.contains_key(key)
    }

    pub fn snapshot(&self) -> BTreeMap<String, Val> {
        self.inner.borrow().map.clone()
    }

    // ---- counters and log ----

    pub fn commits(&self) -> usize {
        self.inner.borrow().commits
    }

    pub fn reads(&self) -> usize {
        self.inner.borrow().reads
    }

    pub fn log(&self) -> Vec<LogEntry> {
        self.inner.borrow().log.clone()
    }

    /// The committed operations as `set:key` / `rm:key`; the erase half of a string set is folded into its `set`.
    pub fn ops(&self) -> Vec<String> {
        self.inner
            .borrow()
            .log
            .iter()
            .filter(|e| e.op != Op::Erase)
            .map(|e| match e.op {
                Op::Set => format!("set:{}", e.key),
                _ => format!("rm:{}", e.key),
            })
            .collect()
    }

    /// Resets the commit and read counters and the log; the map and armed faults stay.
    pub fn reset_counters(&self) {
        let mut i = self.inner.borrow_mut();
        i.commits = 0;
        i.reads = 0;
        i.log.clear();
    }

    // ---- fault injection ----

    /// Commit `n` (1-based since the last reset) applies, then the store is dead until [`FaultKv::revive`].
    pub fn crash_after(&self, n: usize) {
        self.inner.borrow_mut().crash_after = Some(n);
    }

    /// Commit `n` returns an error and is not applied; the store survives.
    pub fn fail_at(&self, n: usize) {
        self.inner.borrow_mut().fail_at = Some(n);
    }

    /// Commit `n` must be the erase half of a string set: it applies, the set returns an error, the key stays absent.
    pub fn fail_after_erase(&self, n: usize) {
        self.inner.borrow_mut().fail_after_erase = Some(n);
    }

    /// The `n`-th read (1-based since the last reset) returns an error.
    pub fn fail_read_at(&self, n: usize) {
        self.inner.borrow_mut().fail_read_at = Some(n);
    }

    /// Every read of `key` returns an error ([`INJECTED`]) until the key is removed.
    pub fn corrupt(&self, key: &str) {
        self.corrupt_with(key, INJECTED);
    }

    /// Every read of `key` returns `code` (for example [`KvError::CODE_INVALID_VALUE`]) until the key is removed.
    pub fn corrupt_with(&self, key: &str, code: i32) {
        self.inner
            .borrow_mut()
            .corrupt
            .insert(key.to_string(), code);
    }

    /// Clears every fault and the dead flag; the map survives ("reboot").
    pub fn revive(&self) {
        let mut i = self.inner.borrow_mut();
        i.crash_after = None;
        i.fail_at = None;
        i.fail_after_erase = None;
        i.fail_read_at = None;
        i.corrupt.clear();
        i.crashed = false;
    }

    fn get<T>(&self, key: &str, pick: fn(&Val) -> Option<T>) -> Result<Option<T>, KvError> {
        self.inner.borrow_mut().read(key, pick)
    }
}

impl OtaKv for FaultKv {
    fn get_u8(&self, key: &str) -> Result<Option<u8>, KvError> {
        self.get(key, |v| if let Val::U8(x) = v { Some(*x) } else { None })
    }

    fn get_u32(&self, key: &str) -> Result<Option<u32>, KvError> {
        self.get(key, |v| if let Val::U32(x) = v { Some(*x) } else { None })
    }

    fn get_str(&self, key: &str) -> Result<Option<KvStr>, KvError> {
        self.get(key, |v| {
            if let Val::Str(x) = v {
                Some(x.clone())
            } else {
                None
            }
        })
    }

    fn set_u8(&mut self, key: &str, value: u8) -> Result<(), KvError> {
        self.inner
            .borrow_mut()
            .commit(Op::Set, key, Some(Val::U8(value)))
            .map(drop)
    }

    fn set_u32(&mut self, key: &str, value: u32) -> Result<(), KvError> {
        self.inner
            .borrow_mut()
            .commit(Op::Set, key, Some(Val::U32(value)))
            .map(drop)
    }

    fn set_str(&mut self, key: &str, value: &str) -> Result<(), KvError> {
        let value = KvStr::try_from(value).map_err(|_| err(KvError::CODE_INVALID_VALUE))?;
        let mut i = self.inner.borrow_mut();
        let erased = i.commit(Op::Erase, key, None)?;
        if i.fail_after_erase == Some(erased) {
            return Err(err(INJECTED));
        }
        i.commit(Op::Set, key, Some(Val::Str(value))).map(drop)
    }

    fn remove(&mut self, key: &str) -> Result<(), KvError> {
        let mut i = self.inner.borrow_mut();
        if !i.map.contains_key(key) && !i.crashed {
            return Ok(());
        }
        let removed = i.commit(Op::Remove, key, None).map(drop);
        if removed.is_ok() {
            i.corrupt.remove(key);
        }
        removed
    }
}
