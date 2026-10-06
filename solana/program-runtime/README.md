# Program runtime

Program runtime implements invocation, SBF VM setup, CPI, sysvars, logging, and
program-cache support. The SVM owns transaction-level account loading and execution
policy; this crate handles program execution within that transaction context.

## Direct account mapping

Engine maps account data into the VM rather than copying it through serialized
program input. Serialization retains loader ABI metadata, while account bytes
remain in their backing storage throughout execution and CPI.

Account growth or copy-on-write can move that storage, so region replacement and
canonical pointer validation must remain consistent with transaction borrowing.
Unsafe mutable translation requires unique references and live backing storage.
The [runtime overview](../README.md#direct-vm-mapping) defines the cross-crate mapping
and ABI constraints.

## Privileged invocation

Builtins can explicitly authorize a [MagicRoot](../../programs/magic-root-program/README.md)
child invocation after constructing or validating the operation. Ordinary native
CPI and logical caller provenance don't grant that access. This keeps builtin
lifecycle operations distinct from arbitrary forwarded instructions.

Authorization grants no extra signers and applies only to that exact child.
Descendants, later siblings, and post-finalize actions don't inherit it. Builtins
must not use the entrypoint to elevate untrusted payloads.

[Invocation state](src/invoke_context.rs) connects [VM setup](src/vm.rs),
[serialization](src/serialization.rs), and [CPI](src/cpi.rs). `frozen-abi` remains
a compatibility stub.
