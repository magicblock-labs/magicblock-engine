//! Account lock tracking for scheduled transactions.

use ahash::HashMap;
use derive_more::{Deref, DerefMut};
use keeper::ResolvedTransaction;
use solana_pubkey::Pubkey;
use solana_svm_transaction::svm_message::SVMMessage;

use crate::executor::{ExecutorHandle, ExecutorId};

pub(super) const MAX_EXECUTORS: u32 = u64::BITS - 1;
const WRITE_BIT: u64 = 1 << MAX_EXECUTORS;

/// Global account locks held by in-flight executor work.
#[derive(Deref, DerefMut)]
pub(super) struct LockTable {
    #[deref]
    #[deref_mut]
    locks: HashMap<Pubkey, AccountLock>,
    /// Executor lease generations used to expire priority reservations on ready.
    epochs: [u64; MAX_EXECUTORS as usize],
}

impl Default for LockTable {
    fn default() -> Self {
        Self {
            locks: HashMap::default(),
            epochs: [0; MAX_EXECUTORS as usize],
        }
    }
}

#[derive(Clone, Copy)]
struct Contender {
    executor: ExecutorId,
    epoch: u64,
}

/// Tracks active account holders as executor bits plus a top write-mode bit,
/// with an optional executor lease granted priority on the account.
///
/// [`WRITE_BIT`] is lock-mode metadata for the active holder set, not an
/// executor. `contender` is priority metadata and is not an active holder.
///
/// Locks are reentrant per executor: an executor that already holds the account
/// can re-acquire it — including upgrading its read hold to a write — because a
/// conflict is only raised against a *different* executor.
#[derive(Default)]
pub(super) struct AccountLock {
    /// Bitset of holding executors; [`WRITE_BIT`] set marks an exclusive lock.
    lock: u64,
    /// Executor lease that lost a conflict here and is owed the account next.
    contender: Option<Contender>,
}

impl LockTable {
    /// Acquires all account locks required by `txn` for `executor`.
    ///
    /// On conflict, locks acquired earlier in the transaction are rolled back
    /// while preserving the blocking executor's contender priority.
    pub(super) fn acquire(
        &mut self,
        executor: &mut ExecutorHandle,
        txn: &ResolvedTransaction,
    ) -> Result<(), ExecutorId> {
        let id = executor.id;
        let epochs = &self.epochs;
        let locks = &mut self.locks;
        let mut locked = 0;
        let mut result = Ok(());
        for (i, &acc) in txn.static_account_keys().iter().enumerate() {
            let lock = locks.entry(acc).or_default();
            lock.expire(epochs);
            result = if txn.is_writable(i) { lock.write(id) } else { lock.read(id) };
            if result.is_err() {
                break;
            }
            *executor.locks.entry(acc).or_default() += 1;
            locked += 1;
        }
        let Err(blocker) = result else {
            return Ok(());
        };
        for (i, acc) in txn.static_account_keys().iter().take(locked + 1).enumerate() {
            let Some(lock) = locks.get_mut(acc) else {
                continue;
            };
            lock.contend(blocker, epochs[blocker as usize]);
            // The last account conflicted and was never acquired by this attempt.
            if i == locked {
                continue;
            }
            let Some(count) = executor.locks.get_mut(acc) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                executor.locks.remove(acc);
                lock.unlock(id);
            }
        }
        Err(blocker)
    }

    /// Expires the completed executor lease and releases all of its account locks.
    pub(super) fn release(&mut self, executor: &mut ExecutorHandle) {
        let id = executor.id;
        self.epochs[id as usize] += 1;
        for (acc, _) in executor.locks.drain() {
            let Some(lock) = self.locks.get_mut(&acc) else {
                continue;
            };
            lock.unlock(id);
            if !lock.locked() && lock.contender.is_none() {
                self.locks.remove(&acc);
            }
        }
    }
}

impl AccountLock {
    fn expire(&mut self, epochs: &[u64; MAX_EXECUTORS as usize]) {
        if self.contender.is_some_and(|c| c.epoch != epochs[c.executor as usize]) {
            self.contender = None;
        }
    }

    /// Acquires an exclusive (write) lock for `executor`.
    ///
    /// Fails with the blocking executor's id if a contender other than
    /// `executor` is queued, or if another executor already holds the account.
    pub(super) fn write(&mut self, executor: ExecutorId) -> Result<(), ExecutorId> {
        if let Some(contender) = self.contender
            && contender.executor != executor
        {
            return Err(contender.executor);
        }
        self.contender.take();
        let holders = self.lock & !WRITE_BIT;
        let bit = 1 << executor;
        let others = holders & !bit;
        if others != 0 {
            return Err(others.trailing_zeros());
        }
        self.lock = WRITE_BIT | bit;

        Ok(())
    }

    /// Acquires a shared (read) lock for `executor`.
    ///
    /// Fails with the blocking executor's id if a contender other than
    /// `executor` is queued, or if another executor holds it for writing.
    pub(super) fn read(&mut self, executor: ExecutorId) -> Result<(), ExecutorId> {
        if let Some(contender) = self.contender
            && contender.executor != executor
        {
            return Err(contender.executor);
        }
        self.contender.take();
        let holders = self.lock & !WRITE_BIT;
        let bit = 1 << executor;
        if self.lock & WRITE_BIT != 0 && holders & bit == 0 {
            return Err(holders.trailing_zeros());
        }
        self.lock |= bit;

        Ok(())
    }

    /// Releases `executor`'s hold on the account and its write mode, if held.
    pub(super) fn unlock(&mut self, executor: ExecutorId) {
        let bit = 1 << executor;
        if self.lock & bit != 0 {
            self.lock &= !(bit | WRITE_BIT);
        }
    }

    /// Records the executor lease owed the account next.
    pub(super) fn contend(&mut self, executor: ExecutorId, epoch: u64) {
        self.contender.replace(Contender { executor, epoch });
    }

    /// Returns whether any executor still actively holds the account.
    pub(super) fn locked(&self) -> bool {
        self.lock & !WRITE_BIT != 0
    }
}
