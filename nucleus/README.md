# `magicblock-engine-nucleus`

Shared vocabulary and utilities for Engine crates: configuration, execution
requests, signed ledger records, service lifecycle, and observability. This crate
does not execute transactions or decide how account state is persisted.

## Features

No features are enabled by default. Enable the capabilities your crate needs:

| Feature | Purpose |
| :-- | :-- |
| `config` | Authority and storage configuration. |
| `ledger` | Signed records and replication positions. |
| `runtime` | Transaction requests, execution results, and quiescence barriers. |
| `shutdown` | Coordinated service cancellation and termination reporting. |
| `notifier` | One-shot event notification. |
| `metrics` | Shared metric conventions and timing helpers. |
| `service` | Metrics and shutdown support together. |
| `tls` | Thread-local context for privileged runtime operations. |
| `testkit` | Shared transaction and account fixtures. |

## Contracts

Authority configuration contains the local private key; redact it before exposing
serialized configuration. Signed records authenticate their kind and payload,
and their encoding must agree across storage and replication participants.
The signing representation requires little-endian targets and padding-free
payloads.

Coordinated shutdown waits for service completion and retains failures reported
during draining. Dropping the manager requests cancellation but does not wait for
durable completion. Request-owned execution replies are local observations, not
part of persisted or replicated transaction data.
