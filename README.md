# Uniswap-v2-style AMM in Miden Assembly (Miden v0.15)

A constant-product AMM with LP tokens, implemented as a **Miden network account**:
users interact with the pool exclusively through **network notes** that the network
transaction builder executes against the AMM account.

## Design

- **Network account** — the AMM is a public account whose auth component is
  `AuthNetworkAccount`. Its note-script allowlist contains exactly the swap,
  add-liquidity and remove-liquidity note scripts (and the deploy tx script), all fixed
  at account creation. Notes carry the `NetworkAccountTarget` attachment so the network
  transaction builder picks them up.
- **LP tokens** — the AMM account is itself the LP-token faucet. The liquidity component
  mints/burns LP via the protocol-level faucet syscalls
  (`miden::protocol::faucet::{create_fungible_asset, mint, burn}`) and tracks total
  supply in the named storage slot `miden_amm::amm::lp_supply`. The standard
  `FungibleFaucet` component is deliberately **not** installed: its public
  `mint_and_send` + allow-all policy would let anyone mint LP for free.
- **Uniswap-v2 math** —
  - swap: `dy = dx·(D−fee)·y / (x·D + dx·(D−fee))` with a configurable fee (basis
    points) stored in `miden_amm::amm::config`;
  - first deposit mints `sqrt(dx·dy) − 1000` LP with `MINIMUM_LIQUIDITY = 1000`
    permanently locked (integer sqrt via a deterministic Newton iteration in MASM —
    no advice-provider input, so the network transaction builder can execute it);
  - later deposits mint `min(dx·S/x, dy·S/y)`; burns pay out pro-rata shares of both
    reserves.
- **Payouts** — the AMM creates the payout note (swap output / minted LP / burned-LP
  proceeds) in the same transaction that consumes the input note. Payout notes are
  **private**: a public output note created in MASM would require the executing host to
  know the full note details behind the recipient digest, which the network transaction
  builder does not have. Recipients reconstruct the note (they chose its serial number)
  and consume it as an unauthenticated input note.
- **Sender-bound liquidity** — deposit and withdrawal payouts are bound to the sender of
  the note: `liquidity.masm` derives the P2ID recipient and tag in-VM from
  `active_note::get_sender` (P2ID script root injected at build time), so a liquidity
  note cannot redirect minted LP or withdrawn assets to a third party. Note storage only
  carries the payout serial number and slippage bounds. Swap payouts remain
  caller-directed (the swapper encodes a recipient digest), matching Uniswap's `to`
  parameter.

## Layout

```
masm/accounts/amm.masm         swap + fee math + pool config slots
masm/accounts/liquidity.masm   add/remove liquidity, LP mint/burn, integer sqrt
masm/notes/*.masm              thin @note_script wrappers calling the account procedures
masm/scripts/deploy_script.masm
src/common.rs                  account/note builders, client helpers, reference math
tests/amm_formula_test.rs      pure-Rust mirrors of the MASM formulas
tests/mock_chain_tests.rs      offline kernel-executed tests: lifecycle, multi-actor
                               fee accrual, sender-binding + non-depositor invariants
tests/amm_swap_ntx.rs          live-testnet e2e (network account + network notes)
```

## Running tests

Offline (MockChain executes the real MASM against the real transaction kernel):

```
cargo test
```

Live testnet e2e (deploys the AMM, adds liquidity, swaps, removes liquidity — takes
several minutes while the network transaction builder processes the notes):

```
cargo test --test amm_swap_ntx -- --ignored --nocapture
```

## Versions

Built against the Miden v0.15 testnet stack: `miden-client 0.15`,
`miden-protocol / miden-standards / miden-testing 0.15.3` (0.15.0–0.15.2 are yanked),
assembly 0.23, Rust 1.93.
