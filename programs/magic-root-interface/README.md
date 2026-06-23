# `magic-root-interface`

This crate defines the MagicRoot program id and `MagicRootInstruction` wire
schema shared by `magic-root-program` and `engine`. Instruction builders can
depend on the interface without linking the native program runtime.

`MagicRootInstruction::compose` prepends the writable target account and
serializes the instruction with wincode. `PostFinalize` also appends each
follow-up program id and its account metas. Signer bits are cleared in the outer
instruction; the native program supplies the declared follow-up signers when it
invokes each action.

`MagicRootInstruction::compose_account` builds the ordered patch sequence for a
complete account's non-flag fields and appends a finalization instruction that
installs its complete flag value without changing lamports. The underlying
patch sequence applies mode before slot so the program can validate
lifecycle-aware slot progression. Callers are responsible for composing current
account state. Internal create composition appends `PostFinalize` immediately
after finalization; MagicRoot relies on this ordering after authenticating the
engine authority and caller.
