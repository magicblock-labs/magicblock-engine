# Engine runtime differences from Agave

These forks adapt Agave execution to Engine's account model: lifecycle-aware
accounts, borrowed storage, and direct VM mapping. The SVM executes caller-loaded
transactions and returns account changes; persistence, commit decisions, and
deployment policy remain outside the runtime. Validator consensus and fork-management
machinery aren't part of this boundary.

The baseline is Agave **4.2.2**, with SDK account **4.3.1**. The
[upgrade disposition](UPSTREAM-4.2.2.md) records upstream ports and intentional omissions.
The sections below describe the intentional runtime deviations and their
cross-crate compatibility constraints; individual crate READMEs explain each layer.

## Runtime layers

| Crate | Responsibility |
| :-- | :-- |
| [account](account/README.md) | Account representation, lifecycle, and copy-on-write storage. |
| [transaction-view](transaction-view/README.md) | Transaction framing and structural sanitization. |
| [svm](svm/README.md) | Transaction account loading, program loading, and execution. |
| [transaction-context](transaction-context/README.md) | Per-transaction borrowing, instruction state, and mutation accounting. |
| [program-runtime](program-runtime/README.md) | Invocation, SBF VM setup, serialization, and CPI. |

`solana-svm` loads through caller callbacks and returns execution results and
mutated accounts. The caller supplies normalized executable data.
Loader headers and indirection are resolved into raw ELF bytes outside the SVM;
only programs required by the transaction are loaded. Deprecated SBF programs
retain their loader owner for ABI v0. Other normalized SBF programs use loader-v4
as nominal owner and ABI v1; native programs retain native-loader ownership.

## Account lifecycle and writeback

`solana-account` supports copy-on-write owned data and an 8-byte-aligned borrowed
view with active/shadow images. Mutation copies into the shadow; commit publishes
it, reset abandons it, and rollback is valid only after commit. Growth beyond
borrowed capacity promotes to owned storage; shared owned data uses `Arc::make_mut`.
This representation lets execution borrow account data from storage while retaining
transactional writeback.

Engine mutation requires a permitted lifecycle mode in addition to Solana access
checks. Delegated and Magic accounts allow user writes. Transient state remains
authoritative and persisted but immutable; closed state is removed by the caller.
Transitions to either revoke mutation immediately, including across CPI. Final
writeback can accept a dirty transition without authorizing further writes.

The [account crate](account/README.md) owns those rules and the active/shadow layout.
Dirty markers track data, owner, lamports, slot, mode, and flags; touched flags track
transaction access. Both are caller writeback signals, not persistence operations.
Complete-account patches cover non-flag fields, and MagicRoot finalization installs
the complete flags without changing lamports. Source freshness remains the host's
responsibility. `rent_epoch` isn't stored; compatibility APIs return a masked value
or ignore writes.

Rent-state and lamport-balance checks remain part of execution. SIMD-0392 rent
relaxation follows supplied runtime features without activating Engine features.
Nonzero Magic accounts are rent-exempt; Magic resizing remains restricted to the builtin.

## Transaction context

`solana-transaction-context` stores accounts in `UnsafeCell`s guarded by explicit
borrow counters. This allows account data to be remapped during execution while
runtime borrow rules remain enforced. `AccountRef` and `AccountRefMut` release
their counters on drop.

`TransactionAccounts` tracks touched accounts, total account-data resize, and
instruction lamport deltas. `ExecutionRecord` returns keyed accounts, return data,
touched count, and resize delta for caller-owned writeback. All account references
must be released before context deconstruction; failed `Rc::try_unwrap` indicates
a lifetime bug. The [context README](transaction-context/README.md) covers borrowing
and instruction-level mutation checks.

## Direct VM mapping

Account data is mapped from its backing storage, avoiding serialized copies and
copy-back after execution. Program input still contains loader ABI metadata,
lamports, lengths, owners, instruction data, and program id. Data bytes occupy
separate `MemoryRegion`s; deserialization updates metadata, not account bytes.

Serialization preserves the loader ABI metadata even though account bytes are
mapped separately:

| Loader ABI | Account-region reservation |
| :-- | :-- |
| Deprecated loader: ABI v0 | Current account-data length. |
| Loader-v2 and loader-v3: ABI v1 | Current length plus `MAX_PERMITTED_DATA_INCREASE`. |

ABI v1 optionally includes direct account pointers. Direct mapping is the only
runtime path; the removed `virtual_address_space_adjustments` and
`account_data_direct_mapping` copy-based branches must not be restored.

Borrowed or shared-owned writable data can initially be mapped read-only. A VM
store enters `TransactionContext::access_violation_handler` to obtain mutable
backing data and update the region pointer, length, and writability. The handler accepts only
stores with an account-index region payload, rejects access beyond the reserved
range, and records touch and resize deltas before growing to the requested length.
Serialization, the handler, and VM error mapping jointly preserve account-specific
readonly, size, and realloc errors.

CPI synchronizes lamports, owner, and length while bytes remain directly mapped;
`CallerAccount::serialized_data` stays empty. Storage movement requires replacing
the caller region from the current account. Strict syscall parameter-address checks
are always enforced: CPI rejects `AccountInfo` key, owner, lamports, data, and
data-length pointers that don't reference canonical VM locations for the passed
account. Without copy-back serialization, those checks protect the mapped storage.
Inner-call growth uses the caller's original length plus the permitted increase;
deprecated loaders reserve only the original length.

## Private transaction framing

Engine composes account operations as atomic Magicblock transactions: private version
127, using V1-shaped framing with a distinct prefix and a 16 MiB size limit. This
allows chunked account patches within one transaction without widening standard policy.
Standard Legacy, v0, and V1 versions accept up to `u16::MAX` bytes. Construction
sets the final prefix before signing the exact message and requires the first
static account to be the configured authority.

Magicblock permits 255 instruction trace entries; CPI remains limited to 64 and
reserves room for top-level instructions. V1-shaped address counts remain limited
to 255. The standard instruction-sysvar encoding isn't enlarged: if a requested
sysvar can't represent the transaction, loading fails with
`MaxLoadedAccountsDataSizeExceeded`, never an empty replacement sysvar.

[`agave-transaction-view`](transaction-view/README.md) validates `u32` framing and canonical
compact-u16 lengths before unchecked access. Sanitization rejects duplicate static
keys and nonempty address lookups (`AddressLookupMismatch`); empty v0 lookup lists
remain valid. Lookup resolution would require coordinated ingress, sanitization,
scheduling, and simulation changes.

## Privileged invocation

Builtin CPI into [MagicRoot](../programs/magic-root-program/README.md) requires
`InvokeContext::native_invoke_magic_root` plus MagicRoot's authority-payer,
builtin-caller, and recursion checks. The builtin must construct or validate the
operation, not authorize arbitrary forwarded payloads. Authorization applies only
to the exact child; descendants and post-finalize actions don't inherit it.
Ordinary native CPI and logical caller provenance confer no such access. This
authorization doesn't change top-level authority operations or instruction serialization.
The SVM's transaction-wide final-writeback exemption still requires all top-level
instructions to target MagicRoot; authorized CPI doesn't grant that exemption.

## Maintenance boundaries

Borrowed account layout, transaction-context borrowing, serialization, VM regions,
and CPI pointer checks must evolve together while preserving ABI v0/v1 metadata
and direct account-region mapping as the only runtime path. Account-region changes
must update CPI pointer checks, region replacement, and VM access handling together.
Exact buffer and lifetime invariants belong to the unsafe APIs.

Private construction, parsing, sanitization, and SVM trace policy must likewise
stay synchronized. Preserve full compact-u16 parsing, checked `u32` framing, and
caller-owned writeback when porting upstream changes.
