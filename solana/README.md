# Engine Runtime Differences from Agave

This directory contains the Agave runtime forks required by the engine. These
crates execute caller-loaded transactions and return account changes; they do
not own consensus, fork choice, confirmation, persistence, or validator commit
policy.

The current upstream baseline is Agave **4.2.2**, with SDK account **4.3.1**.
See [the upgrade disposition](UPSTREAM-4.2.2.md) for ports and intentional omissions.

The differences below are intentional compatibility constraints for account
representation, transaction context, serialization, VM mapping, and CPI.

## Runtime boundary

- `solana-svm` loads accounts through a caller callback and returns execution
  results and mutated accounts.
- Persistence, commit decisions, and deployment policy remain outside the fork.
- Program loading is limited to programs required by the transaction. Callers
  supply executable SBF account data as raw ELF bytes; decoding loader-specific
  headers or indirection remains outside the runtime.
- Deprecated SBF programs retain their loader owner for ABI v0. Other normalized
  SBF programs use loader-v4 as their nominal owner and ABI v1. Native programs
  retain native-loader ownership.
- Rent-state and lamport-balance checks remain part of execution.
- SIMD-0392 rent-transition relaxation follows the supplied runtime feature set;
  it does not activate new Engine features. Nonzero ephemeral accounts retain
  their rent exemption. Ephemeral resizing remains restricted to the builtin.
- If a requested instruction sysvar cannot represent the transaction,
  account loading fails with `MaxLoadedAccountsDataSizeExceeded`; it never
  substitutes an empty instruction sysvar. Private transaction size limits do
  not enlarge the standard instruction-sysvar encoding.

## Account representation

`solana-account` uses copy-on-write owned data or an 8-byte-aligned borrowed
view with active/shadow images. Mutation copies into the shadow; commit publishes
it, reset abandons it, and rollback is valid only after commit. Growth beyond
borrowed capacity promotes to owned storage; shared owned data uses
`Arc::make_mut`. See [layout and lifecycle rules](account/README.md).

Only delegated and ephemeral accounts are user-mutable. Transient and closed
revoke mutation immediately, including across CPI. The transaction-final guard
accepts dirty transitions into those modes for writeback, not further writes.
Transient remains authoritative and persisted; the caller removes closed state.

Dirty markers track data, owner, lamports, slot, mode, and flags. Complete-account
patches cover non-flag fields; MagicRoot finalization installs all supplied flags
without changing lamports. Freshness remains the caller's responsibility.
`rent_epoch` is not stored; compatibility APIs return or ignore its masked value.

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

`agave-transaction-view` supports Legacy, v0, V1, and private Magicblock version
127. Standard versions accept up to `u16::MAX` bytes; Magicblock uses the V1
layout with a distinct prefix and a 16 MiB limit. Full canonical compact-u16
parsing and checked `u32` framing precede unchecked views.

Engine composes account operations as Magicblock transactions, signs the exact
message after setting the prefix, and requires the first static account to be
the configured authority. The private format permits atomic chunked account
patches without widening standard policy: instruction traces allow 255 entries,
CPI remains limited to 64 and reserves space for top-level instructions, and
V1-shaped address counts remain limited to 255.

Address lookup entries fail sanitization with `AddressLookupMismatch`; empty v0
lookup lists remain valid. Static account keys must be unique. See the
[wire and safety contracts](transaction-view/README.md); keep parsing, sanitizing,
Engine composition, and SVM trace limits synchronized.

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

Builtin CPI into MagicRoot requires `InvokeContext::native_invoke_magic_root`,
in addition to MagicRoot's authority-payer, builtin-caller, and recursion checks.
Only its exact child instruction is authorized; descendants and post-finalize
actions do not inherit authorization. Builtins must construct or validate the
privileged operation, not elevate arbitrary forwarded payloads. Ordinary native
CPI and logical caller provenance do not grant this access. Top-level authority
operations and instruction serialization remain unchanged.

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
