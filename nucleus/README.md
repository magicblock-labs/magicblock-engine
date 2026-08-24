# `magicblock-engine-nucleus`

Nucleus contains shared engine types that do not own storage or execution
policy. It also exposes byte-size constants and a Unix-time helper that returns
zero when the system clock predates the epoch. Its default feature set is empty.

## Features

- `config`: serializable authority, accountsdb, blockstore, and ledger
  configuration types. Authority serialization includes the complete local
  keypair; consumers must redact it before exposing serialized output.
- `shutdown`: ordered cancellation, service handles, and termination reporting.
  The pacemaker quiesces execution and terminally syncs the ledger before the
  sequencer and appender tier; remaining backing services stop afterward.
  Dropping the manager cancels every tier without waiting for services to stop.
- `notifier`: the one-shot, non-resetting `EventNotifier` latch.
- `ledger`: shared block-boundary metadata, including each block's locally
  computed hash and parent, plus snapshot checksum/transaction seals and
  blockstore positions.
- `metrics`: Prometheus metric construction, `engine_`-namespaced registration,
  labels, and timers.
- `service`: the `metrics` and `shutdown` feature bundle.
- `runtime`: transaction views, execution messages, sequencer handles, and
  quiescence barriers, including atomic block-checkpoint pauses; it also enables
  `ledger`, `service`, and `tls`.
- `tls`: thread-local MagicRoot authority and encoded service-message state.
- `testkit`: engine-independent fixtures, temporary directories, Legacy/V0/V1
  transaction encoding, v42 instructions, transaction views, and tracing setup
  used by downstream test targets. It enables `runtime` because `signed_view`
  returns the runtime transaction view.

Keeper-specific harnesses remain in `keeper::testkit`.
