//! Input-ordered account dependency tracking.
//!
//! The table builds a directed acyclic graph (DAG) in canonical transaction
//! stream order. For each account, a read depends on its latest unfinished
//! writer. A write depends on both its latest unfinished writer and every
//! unfinished reader registered after that writer. Read/read and disjoint
//! accesses therefore remain parallel, while every conflict containing a write
//! follows the input stream.
//!
//! Registration stores both directions of each direct edge: the later node's
//! predecessor count and the earlier node's dependents. Completion marks the
//! earlier node and decrements only its direct dependents; a dependent becomes
//! ready when its count reaches zero. For example, with writes
//! `X(A) -> Y(A, B) -> Z(B)`, registering `Y` makes it the frontier for both
//! accounts, so `Z` depends on `Y` even before `Y` can execute. Completing `X`
//! releases `Y`, and only completing `Y` can release `Z`.
//!
//! Tickets are append-only node indices within one drain epoch. Every
//! predecessor ticket is lower than its dependents, nodes never move, and a
//! completed sentinel keeps frontier cleanup independent of payload storage.
//! This requires each sanitized transaction's static account keys to be unique,
//! as guaranteed by [#66]; otherwise one registration could create a self-edge.
//!
//! A full drain leaves no outstanding, blocked, or ready work. The sequencer
//! then resets the arena and account frontiers at a block boundary, barrier, or
//! orderly shutdown, allowing the next epoch's tickets to start at zero.
//!
//! [#66]: https://github.com/magicblock-labs/magicblock-engine/issues/66

use std::{collections::VecDeque, mem};

use ahash::HashMap;
use keeper::ResolvedTransaction;
use smallvec::SmallVec;
use solana_pubkey::Pubkey;
use solana_svm_transaction::svm_message::SVMMessage;

use super::{ReadyTransaction, Ticket};

/// Sentinel predecessor count marking a completed arena node.
const COMPLETE: usize = usize::MAX;

/// Append-only transaction arena and per-account ordering frontiers.
#[derive(Default)]
pub(super) struct OrderingTable {
    /// Latest unfinished writer and readers observed for each account.
    accounts: HashMap<Pubkey, AccountFrontier>,
    /// Stable transaction nodes; indices are tickets until [`Self::reset`].
    nodes: Vec<TransactionNode>,
    /// Payloads retained only while their nodes have unfinished predecessors.
    blocked: HashMap<Ticket, ResolvedTransaction>,
    /// Dependency-free transactions awaiting an executor.
    ready: VecDeque<ReadyTransaction>,
    /// Number of registered transactions that have not completed.
    outstanding: usize,
}

/// Unfinished transactions relevant to the next access of an account.
#[derive(Default)]
struct AccountFrontier {
    /// Most recently registered unfinished writer.
    last_writer: Option<Ticket>,
    /// Readers since `last_writer`; stale tickets are pruned by the next write.
    readers: SmallVec<[Ticket; 1]>,
}

/// Stable dependency metadata retained until the arena resets.
struct TransactionNode {
    /// Unfinished predecessor count, or [`COMPLETE`] after execution.
    predecessors: usize,
    /// Later transactions directly waiting for this node.
    dependents: SmallVec<[Ticket; 1]>,
}

impl OrderingTable {
    /// Registers a transaction after every earlier accepted transaction.
    ///
    /// Returns whether it has unfinished predecessors. Reads follow the latest
    /// writer; writes follow both the latest writer and all current readers.
    /// Sanitization guarantees unique static keys, so frontier tickets always
    /// precede the ticket registered here.
    pub(super) fn register(&mut self, transaction: ResolvedTransaction) -> bool {
        let ticket = self.nodes.len();
        let mut predecessors = SmallVec::<[Ticket; 2]>::new();
        let nodes = &self.nodes;

        for (index, &account) in transaction.static_account_keys().iter().enumerate() {
            let frontier = self.accounts.entry(account).or_default();
            frontier.last_writer.take_if(|&mut prior| nodes[prior].predecessors == COMPLETE);
            predecessors.extend(frontier.last_writer);

            if transaction.is_writable(index) {
                frontier.readers.retain(|&mut prior| nodes[prior].predecessors != COMPLETE);
                predecessors.extend(frontier.readers.drain(..));
                frontier.last_writer = Some(ticket);
            } else {
                frontier.readers.push(ticket);
            }
        }

        predecessors.sort_unstable();
        predecessors.dedup();
        for &predecessor in &predecessors {
            self.nodes[predecessor].dependents.push(ticket);
        }

        let blocked = !predecessors.is_empty();
        if blocked {
            self.blocked.insert(ticket, transaction);
        } else {
            self.ready.push_back(ReadyTransaction { ticket, transaction });
        }
        self.nodes.push(TransactionNode {
            predecessors: predecessors.len(),
            dependents: SmallVec::new(),
        });
        self.outstanding += 1;
        blocked
    }

    /// Takes the next dependency-free transaction for dispatch.
    pub(super) fn take_ready(&mut self) -> Option<ReadyTransaction> {
        self.ready.pop_front()
    }

    /// Completes a ticket and returns how many direct dependents became ready.
    pub(super) fn complete(&mut self, ticket: Ticket) -> usize {
        debug_assert!(ticket < self.nodes.len());
        debug_assert_ne!(self.nodes[ticket].predecessors, COMPLETE);
        debug_assert!(self.outstanding > 0);

        let node = &mut self.nodes[ticket];
        node.predecessors = COMPLETE;
        let dependents = mem::take(&mut node.dependents);
        self.outstanding -= 1;

        let mut ready = 0;
        for dependent in dependents {
            debug_assert!(dependent > ticket);
            let node = &mut self.nodes[dependent];
            debug_assert_ne!(node.predecessors, COMPLETE);
            debug_assert!(node.predecessors > 0);
            node.predecessors -= 1;
            if node.predecessors != 0 {
                continue;
            }

            if let Some(transaction) = self.blocked.remove(&dependent) {
                self.ready.push_back(ReadyTransaction { ticket: dependent, transaction });
                ready += 1;
            }
        }
        ready
    }

    /// Number of registered transactions that have not completed.
    pub(super) fn len(&self) -> usize {
        self.outstanding
    }

    /// Whether every registered transaction has completed.
    pub(super) fn is_empty(&self) -> bool {
        self.outstanding == 0
    }

    /// Clears stable nodes and account frontiers after all work drains.
    pub(super) fn reset(&mut self) {
        debug_assert_eq!(self.outstanding, 0);
        debug_assert!(self.blocked.is_empty());
        debug_assert!(self.ready.is_empty());
        self.accounts.clear();
        self.nodes.clear();
        self.blocked.clear();
        self.ready.clear();
    }
}
