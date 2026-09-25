# `solana-account`

Engine account values, lifecycle rules, and copy-on-write access to owned or
borrowed data. This fork lets execution work directly with external storage while
tracking changes for transactional writeback. It does not decide how state is
persisted or whether an external account image is trustworthy.

Equality compares account state and data, not storage representation or dirty
markers. The `testkit` feature provides account and borrowed-storage fixtures.

## Account lifecycle

Delegated and Magic accounts permit user mutation. Transient state remains
engine-authoritative but immutable; closed state is removed by the storage layer.
A transition to transient or closed revokes mutation immediately, including
across CPI. Transaction-final writeback may accept that transition without
authorizing further writes.

Mode and slot are validated together:

| From | Same or newer slot | Strictly newer slot |
| --- | --- | --- |
| Uninit | ReadOnly, System, Delegated, Magic, Closed | Uninit |
| ReadOnly | Delegated, Magic, Closed | ReadOnly, Uninit |
| System | — | System |
| Delegated | Transient | — |
| Magic | Closed | — |
| Transient | ReadOnly, Uninit | Delegated |
| Closed | — | — |

Unlisted transitions and slot regressions are rejected without changing state or
dirty markers. Authoritative accounts cannot be replaced in the same mode, even
at a newer slot; ordinary transaction mutations are unaffected.

Magic represents authoritative state created inside the ER. It must be closed
before another account image can occupy its key. The host validates creation and
closure eligibility. Empty token balances do not invalidate Magic accounts, and
this crate does not interpret token data.

Flags are supplied as a complete value during privileged finalization, which
does not change lamports. They are not evidence of source freshness. Producers
and consumers must agree on lifecycle semantics as well as binary encoding.

## Borrowed storage

Borrowed buffers must satisfy the account layout and alignment requirements,
remain live, and provide unique mutable access for the borrow's duration. Mutation
uses a shadow image; commit publishes it, reset abandons it, and rollback is valid
only after commit. Growth beyond borrowed capacity promotes data to owned storage.

Changes to this representation must remain compatible with transaction context,
VM mapping, and persistence. See the [runtime-fork contracts](../README.md) for
cross-crate maintenance constraints; follow individual unsafe APIs for exact
buffer and lifetime requirements.
