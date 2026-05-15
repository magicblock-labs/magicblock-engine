# Engine Runtime Differences from Agave

This directory contains the Agave runtime forks required by the engine. These
crates execute caller-loaded transactions and return account changes; they do
not own consensus, fork choice, confirmation, persistence, or validator commit
policy.

The differences below are intentional compatibility constraints for account
representation, transaction context, serialization, VM mapping, and CPI.

## Runtime boundary

- `solana-svm` loads accounts through a caller callback and returns execution
  results and mutated accounts.
- Persistence, commit decisions, and deployment policy remain outside the fork.
- Program loading is limited to programs required by the transaction and checks
  native-loader or `PROGRAM_OWNERS` ownership.
- Rent-state and lamport-balance checks remain part of execution.

## Account representation

`solana-account` replaces the shared-data representation with copy-on-write
storage. `AccountSharedData` contains either an owned `Arc<Vec<u8>>` or a borrowed
view into an aligned external buffer. `DirtyMarkers` record changes to data,
owner, lamports, slot, mode, and state flags for higher-layer writeback.

Borrowed storage has these invariants:

- The buffer is 8-byte aligned and remains live for the borrow.
- One header and pubkey prefix are followed by two account images.
- `AccountHeader::sequence` selects the active image.
- `translate` copies active state into the shadow image before mutation.
- `commit` publishes the shadow image; `reset` abandons it.
- `rollback` is valid only after `commit`.

Writes remain borrowed while they fit the image capacity. Growth beyond that
capacity promotes the account to owned storage. Shared owned data becomes unique
through `Arc::make_mut` before mutation.

`AccountMode` contains `ReadOnly`, `Placeholder`, `System`, `Delegated`,
`Ephemeral`, `Transient`, and `Closed`. Only delegated and ephemeral accounts are
mutable by user programs. Transient accounts remain persistent but immutable
after the transaction that legally transitions them from delegated. The
transaction access guard recognizes that transition through the mode dirty
marker; a freshly loaded transient account has a clean marker and remains
immutable. The same transaction-local exception lets a legal mode transition
close an account. Ephemeral accounts remain persistent until then. `StateFlags`
contains `EXECUTABLE`. Complete-account patch sequences cover non-flag fields;
MagicRoot finalization installs the caller's complete flag value without
changing lamports. Replacement freshness remains the caller's responsibility.
`AccountSharedData` does not store `rent_epoch`; compatibility APIs return or
ignore the masked value required by their interface.

## Transaction context

`solana-transaction-context` stores accounts in `UnsafeCell`s guarded by explicit
borrow counters. This permits the VM access handler to remap account data while
runtime borrow rules remain enforced.

`TransactionAccounts` records touched accounts, total account-data resize, and
instruction lamport deltas. `AccountRef` and `AccountRefMut` release their
counters on drop. `ExecutionRecord` returns keyed accounts, return data, touched
count, and resize delta. All references must be released before context
deconstruction; failure of `Rc::try_unwrap` indicates a lifetime bug.

## Transaction parsing and Engine-private transactions

`agave-transaction-view` parses Legacy, v0, V1, and Engine-private Magicblock
transactions directly from their serialized bytes. Legacy and v0 retain the
standard Solana wire layouts, while V1 retains the Agave V1 layout. All three
accept serialized sizes through `u16::MAX` bytes, inclusive. The compact-u16
parser supports the complete canonical one-, two-, and three-byte encoding, so
instruction data and other framed arrays are no longer limited by the former
two-byte parser assumption.

Frame offsets and total lengths are stored as `u32`. Fallible parsing uses
checked range arithmetic and validates every frame before unchecked iterators
or typed views access the original bytes. The engine is guaranteed not to run
on 16-bit targets, so conversion from validated `u32` offsets to `usize` is
direct.

Magicblock is private transaction version 127 and reuses the V1 layout with a
distinct prefix. Its signatures follow the V1 message at the end of the byte
stream. The Engine transaction composer compiles account operations as V1,
writes the Magicblock prefix, signs the exact message range, and verifies that
the first static account is the configured Engine authority. Magicblock
transactions may be at most 16 MiB and raise only the SVM instruction-trace
limit to 255; CPI invocations remain limited to 64 and reserve capacity for all
top-level instructions. Standard versions retain their existing structural
limits. The account accessor uses this private path so a 64 KiB account payload
can be split into patch instructions and executed atomically without relaxing
standard transaction policy. Its V1-shaped address count remains encodable at a
maximum of 255.

Address lookup tables are intentionally disabled. Any transaction containing a
lookup table entry fails sanitization with `AddressLookupMismatch`; an empty v0
lookup list remains valid and resolves without loaded addresses. Sequencing and
simulation therefore resolve transactions without supplying loaded addresses.

The crate-specific wire and safety contracts are documented in
[`transaction-view/README.md`](transaction-view/README.md). Keep its version
prefix, signed message range, size limits, sanitizer, Engine composer, and SVM
trace-limit override synchronized.

## VM account mapping

Account data is always mapped directly into the SBF VM. Do not restore the
removed `virtual_address_space_adjustments` or `account_data_direct_mapping`
branches that copied account data through serialized program input.

Serialization retains loader ABI metadata:

- Deprecated-loader accounts use ABI v0.
- Loader-v2 and loader-v3 accounts use ABI v1.
- ABI v1 optionally includes direct account pointers.

The serialized input contains metadata, lamports, lengths, owners, instruction
data, and program id. Account data resides in separate `MemoryRegion`s.
Deprecated-loader regions reserve the current length; newer loaders also reserve
`MAX_PERMITTED_DATA_INCREASE`. Deserialization reads mutable metadata but does
not copy account bytes back from the input buffer.

## Access-violation growth

Writable borrowed or shared-owned account data may initially be mapped
read-only. The first VM store enters the transaction-context handler, which:

- handles stores only and requires an account-index region payload;
- rejects accesses outside the account's reserved address range;
- records touch and resize deltas before growing data;
- grows only to the requested access length; and
- replaces the region host pointer, length, and writability.

Keep serialization, `TransactionContext::access_violation_handler`, and VM error
mapping synchronized. They jointly map growth failures to account-specific
readonly, size, and realloc errors.

## CPI synchronization

`CallerAccount::serialized_data` remains empty. CPI entry and exit synchronize
lamports, owner, and data length, while account bytes remain directly mapped.
When storage can move, CPI replaces the caller `MemoryRegion` with one created
from the current account.

Strict syscall parameter-address checks are always enforced. CPI rejects
`AccountInfo` fields whose key, owner, lamports, data, or data-length pointers do
not reference the canonical VM locations for the passed account. This is required
because account bytes are mapped directly into the VM and cannot be protected by
copy-back serialization.

Inner-instruction growth uses the caller's original length plus the permitted
increase. Deprecated loaders reserve only the original length. Any account-region
layout change must update CPI pointer checks, region replacement, and VM access
handling together.

## Maintenance constraints

- Preserve direct account-region mapping as the only runtime path.
- Preserve ABI v0 and ABI v1 metadata compatibility.
- Keep borrowed layout changes synchronized across account, transaction-context,
  serialization, and mapping code.
- Preserve full compact-u16 parsing and checked `u32` transaction framing.
- Keep Magicblock construction and execution policy synchronized with
  `agave-transaction-view`.
- Do not enable address lookup resolution without revisiting ingress,
  sanitization, scheduling, and simulation together.
- Treat dirty markers and touched flags as the caller's writeback signal.
- Keep persistence, consensus, validator fee policy, and batch commit decisions
  outside these runtime crates.
