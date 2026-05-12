//! Thread-local execution state shared with runtime-adjacent code.

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
};

use solana_instruction_error::InstructionError;
use solana_pubkey::Pubkey;
use wincode::{SchemaWrite, config::Configuration};

/// Wincode-encoded message buffered for later handling on the same thread.
pub type EncodedMessage = Vec<u8>;

thread_local! {
    /// Per-thread queue for messages emitted while a transaction executes.
    pub static TLS: RefCell<TlsManager> = RefCell::new(Default::default());
    /// Signer authorized to invoke the MagicRoot program on the current thread.
    pub static AUTHORITY: Cell<Pubkey> = Cell::new(Default::default());
}

/// FIFO queue of encoded messages scoped to the current thread.
#[derive(Default)]
pub struct TlsManager(VecDeque<EncodedMessage>);

impl TlsManager {
    /// Encodes `msg` and appends it to the current thread's queue.
    pub fn enqueue<T>(msg: &T) -> Result<(), InstructionError>
    where
        T: SchemaWrite<Configuration, Src = T>,
    {
        let encoded = wincode::serialize(msg).map_err(|_| InstructionError::Custom(u32::MAX))?;
        TLS.with_borrow_mut(|tls| tls.0.push_back(encoded));
        Ok(())
    }

    /// Removes the oldest encoded message from the current thread's queue.
    pub fn dequeue() -> Option<EncodedMessage> {
        TLS.with_borrow_mut(|tls| tls.0.pop_front())
    }

    /// Drops every queued message for the current thread.
    pub fn clear() {
        TLS.with_borrow_mut(|tls| tls.0.clear())
    }
}
