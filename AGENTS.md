# Engine repository
## Boundaries

Use the root `README.md` workspace layout to locate behavior owners.

Before changing `solana/*`, read `solana/README.md` and preserve its maintenance
constraints. These runtime forks execute caller-loaded transactions and return
account changes; persistence, commit decisions, and deployment policy belong
outside them.

## Crate documentation

Crate READMEs also serve as crate-level docs through
`#![doc = include_str!("../README.md")]`. Update them only when explicitly
requested or when existing reader guidance would become misleading or materially
incomplete. Keep implementation details in source or API docs.
