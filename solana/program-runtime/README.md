# `solana-program-runtime`

The Engine fork of Agave's program execution runtime: invocation state, SBF VM
setup, cross-program invocation, sysvars, logging, and program-cache support.
Transaction-level account loading and execution policy remain in the SVM.

## Direct account access

Programs access account data through direct VM mappings. Serialization, account
growth, and CPI must preserve compatible regions and enforce canonical account
pointers; changes to these paths must be coordinated with transaction context.
Unsafe mutable translation requires unique references and live backing storage.
See the [runtime-fork contracts](../README.md) for ABI and mapping constraints.

## Privileged invocation

Builtins may explicitly authorize a MagicRoot child invocation after constructing
or validating the operation. This authorization is not a mechanism for forwarding
untrusted instructions, grants no extra signers, and is not inherited by nested
calls, later siblings, or follow-up actions. Ordinary native CPI and caller
provenance do not grant MagicRoot access.

The `frozen-abi` feature is a compatibility stub, not frozen-ABI validation.
