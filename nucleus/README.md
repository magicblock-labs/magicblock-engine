# Nucleus

Nucleus defines the types and service primitives shared across Engine crates.
Execution requests, signed history records, and shutdown coordination live here
so their producers and consumers can share contracts without depending on one
another's implementations. Scheduling and storage remain in their owning crates.

## Shared boundaries

[Runtime types](src/runtime.rs) carry transaction requests and execution results
between Engine and Processor, along with quiescence barriers used for state boundaries.
[Ledger types](src/ledger.rs) define signed blocks, superblock seals, resets,
checkpoints, and replication positions. Record signatures authenticate both kind
and payload; encoding must agree across storage and replication. The signing
representation requires little-endian targets and padding-free payloads.

[Configuration](src/config.rs) distinguishes local identity from optional upstream
authority and describes storage and pacing. Authority configuration contains the
local private key and must be redacted before exposing serialized configuration.

[Shutdown coordination](src/shutdown.rs) orders service termination and retains
failures reported during draining. Dropping the manager requests cancellation but
doesn't wait for durable completion. Request-owned execution replies remain local
observations, independent of that service lifecycle.

## Feature boundaries

Features keep dependencies aligned with each consumer's role. `config`, `ledger`,
`runtime`, and `shutdown` expose the corresponding modules. `notifier`, `metrics`,
and `tls` provide one-shot notification, observability helpers, and thread-local
context for privileged runtime operations. `service` combines metrics and shutdown;
`testkit` supplies transaction, block, and temporary-directory fixtures.

Changes here affect both sides of a crate boundary. Signed-record changes also
need to preserve compatibility with retained data and replication peers.
