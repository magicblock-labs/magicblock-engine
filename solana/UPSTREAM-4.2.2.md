# Agave 4.2.2 port

Baseline: Agave `v4.1.1` (`19e19df564c7`) to `v4.2.2` (`c9c6f3287e26`).
SDK account comparison: `account@v4.3.0` to `account@v4.3.1`. Engine already
advertised account 4.3.1; the upstream delta only changes frozen-ABI metadata,
which this account implementation does not use. Later SDK account APIs are not
part of this port.

## Disposition

| Upstream change | Engine disposition |
| --- | --- |
| Memory aliasing fixes (`9e77c42e5d`), syscall output `MaybeUninit` (`527b534509`), optimized memcmp (`be8588816f`) | Port the runtime slice-translation macro's raw-pointer contract and unsafe mutable CPI helper. The syscall implementation comes from `solana-syscalls` 4.2.2. Keep direct mapping and canonical CPI pointer checks. |
| Shared C/Rust signer translation (`95864a0002`) | One `VmSlice` implementation; preserve old free-function names as aliases. Remove the trait method no longer implemented by upstream syscalls. Retain Engine's validation of untrusted instruction metadata. |
| Invocation push ordering (`2e6926904a`) | Push the transaction frame before both the memory stack and Engine's additional syscall-context stack. |
| SIMD-0392 rent adaptations (`dfb9c2d576`) | Port pre/post balance, size, and owner checks under the supplied feature gate. Preserve Magic rent exemption, zero-lamport classification, incinerator exemption, and existing feature activation. Validator fee-payer/nonce rent policy remains absent. |
| Fallible instruction-sysvar encoding (`e58a412cdb`) | Return `MaxLoadedAccountsDataSizeExceeded` from loading rather than replacing an encoding failure with empty data. Private transaction framing does not expand this sysvar's encoding. |
| SlotHashes wincode decoding (`4e523f2a7e`) | Decode the cached object with wincode; retain the original complete account bytes. Keeper's compatible persisted encoding is unchanged. |
| Callback separation (`5f356cd550`) | Consume the independent upstream traits; remove Engine's now-unnecessary load-callback invoke implementation. Preserve the existing early loader drop and separate invocation callback. |
| Modular-exponentiation removal (`3b1ca8ded3`) | Consume the syscall removal and remove its two execution-cost fields, required by `solana-compute-budget` 4.2.2 struct construction. |
| Oversized V1 framing (`257e2639b8`) | Already covered by checked `u32` offsets and version-specific size validation. Do not restore upstream `u16` offsets or narrow private transactions. Existing V1 serializers already include the prefix correctly. |
| Configurable sanitization (`d392b1b105`) | Omit the API-only refactor; preserve Engine's sanitizer API, heap limits, account limits, disabled address lookups, and private trace policy. |
| Per-account touched flags (`7f70cf81eb`) | Omit validator writeback plumbing. Engine keeps its execution-record interface, account dirty markers, touched accounting, and higher-layer commit ownership. |
| CPI accounts scratchpad (`9c6b418c2b`) | Omit unused ABI-v2 frame expansion. Engine retains ABI v0/v1 mapping and its existing top-level/CPI trace accounting. |
| Redundant resize owner check (`1662ba8396`) | Retain the check: Engine adds an Magic resize restriction here and preserves its existing error ordering. |
| Program owner/cache lookup, pruning, and epoch preparation (`302704480f`, `ac2392c100`, `0efe8bfc8c`, `2832b413b1`, `b8df5e67ce`) | Omit validator cache machinery absent from Engine's cache. Programs remain caller-normalized raw ELF accounts; no fork graph, deployment-slot lookup, or upcoming-epoch preparation is introduced. |
| Conformance harnesses, validator nonce filtering, upstream-only tests, edition/lint/docs changes, mock-helper and memory-pool API cleanup | Omit unrelated infrastructure and API-only churn. Preserve local crate documentation, compatibility features, existing tests, and workspace conventions. |

## Dependency and compatibility boundary

Agave-owned dependencies and forks use 4.2.2. The lockfile follows the release
versions for SDK message 4.2.3, transaction 4.1.4, SVM transaction 4.2.0,
SBPF 0.21.1, and stake interface 4.3.0. Independently versioned SDK crates are not
renumbered to 4.2.2. All five runtime patches remain local.

Public compatibility changes are limited to the syscall trait's removed signer
method, the two removed modular-exponentiation cost fields, the slice macro's
raw-pointer return contract, and the now-unsafe mutable CPI slice helper.
Borrowed-account storage and persistence layouts do not change. MBV consumer
integration remains tracked separately in MBV #1660.

Existing tests are retained and adapted; no new tests or conformance suite are
introduced. In particular, passing the existing suite does not establish
exhaustive coverage of newly ported SIMD-0392 transitions or instruction-sysvar
overflow rejection.
