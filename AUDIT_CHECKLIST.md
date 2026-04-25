# Soroban Contract Audit Checklist

This checklist is used to prepare `clips_nft` for external professional review.

## 1) Scope And Versioning

- [x] Contract scope documented (`clips_nft/src/lib.rs`)
- [x] Contract version exposed (`version()`)
- [x] Upgrade path documented (`upgrade`)

## 2) Access Control And Privileged Functions

Privileged functions (admin-only) are explicitly marked in contract docs and enforce admin authorization:

- [x] `set_signer`
- [x] `upgrade`
- [x] `pause`
- [x] `unpause`
- [x] `blacklist_clip`
- [x] `set_name`
- [x] `set_symbol`
- [x] `set_royalty`

Additional access checks:

- [x] One-time initialization guard in `init`
- [x] `init` requires `admin` authorization
- [x] Owner checks enforced for `transfer`, `burn`
- [x] Approval checks enforced for `transfer_from`, `approve`

## 3) Input Validation And Invariants

- [x] `sale_price > 0` checks in royalty paths
- [x] Royalty basis points capped at `<= 10_000`
- [x] Duplicate mint prevention by `ClipIdMinted`
- [x] Blacklisted clip IDs cannot be minted
- [x] Signature verification required for minting

## 4) Arithmetic Safety

- [x] Overflow checks in royalty math (`calculate_royalty`)
- [x] Saturating arithmetic only used where intentional
- [x] Rounding behavior documented and deterministic

## 5) Pause / Emergency Controls

- [x] `pause` blocks mint/transfer flows
- [x] `unpause` restores functionality
- [x] Pause state query available (`is_paused`)

## 6) Event Coverage

- [x] Mint event
- [x] Transfer event
- [x] Burn event
- [x] Approval events
- [x] Blacklist event
- [x] Royalty events
- [x] Upgrade event

## 7) Documentation Quality

- [x] Contract-level docs include storage and signature model
- [x] Public functions include rustdoc comments
- [x] Privileged functions explicitly marked

## 8) Test-Only Code Removal

- [x] Removed synthetic gas-tracking state and APIs from production contract
- [x] Removed tests coupled to synthetic gas tracking
- [x] Kept functional unit tests for runtime behavior

## 9) Build And Test Gates

- [x] `cargo check` passes
- [x] `cargo test` passes

## 10) Pre-Audit Delivery Artifacts

- [ ] Share this checklist with auditor
- [ ] Share deployment configuration and target network details
- [ ] Share threat model / assumptions
- [ ] Freeze release candidate commit hash
