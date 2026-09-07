# verita — Solana retention vault program

Anchor program that escrows construction retention in a program-owned PDA: the main contractor cannot claw it back, and the subcontractor can release everything undisputed once the defects-liability period plus grace has elapsed — with no counterparty signature.

Workspace root: `verita/`. Interface spec for the frontend: [`../API-CONTRACT-retention.md`](../API-CONTRACT-retention.md).

## Prerequisites

- [Solana CLI](https://docs.solana.com/cli/install-solana-cli-tools)
- [Anchor CLI](https://book.anchor-lang.com/getting_started/installation.html) (built against Anchor 1.1.x / `anchor-lang` 1.2)
- Rust toolchain (pinned by `rust-toolchain.toml`)

## Build

```bash
cd verita
anchor build
```

The default build **includes the `demo` feature**, which is required for the
`advance_clock` instruction used to roll time forward on stage. Verify it is present:

```bash
python3 -c "import json;d=json.load(open('target/idl/verita.json'));print([i['name'] for i in d['instructions']])"
# expect: ['advance_clock', 'attest_cpc', 'claim_release', 'close_vault', 'fund_vault']
```

### Production build (no demo clock)

```bash
cargo build --release --manifest-path programs/verita/Cargo.toml --no-default-features
```

With `demo` off, `advance_clock` is compiled out entirely and `effective_ts()` reads
the on-chain `Clock` directly — production never consults the stored `clock_offset`.

## Test

The end-to-end tests execute the real SBF artifact, so **`anchor build` must run first**
(`anchor test` does both):

```bash
anchor build
cd programs/verita && cargo test
```

22 tests, all passing:

- **Unit** (4, in `src/state.rs`) — account sizing (`INIT_SPACE`), backstop arithmetic, moiety/claimable accounting, frozen-slice exclusion.
- **End-to-end** (18, in `tests/e2e.rs`, via `litesvm`) — the full demo path plus a regression test for every bug found in the pre-deploy review:

| Area | Covered |
|---|---|
| `fund_vault` | real lamports move contractor → PDA; 64-byte `project_id` fits; zero amount rejected; double-fund rejected |
| Beat 1 — refusal | contractor cannot claim, close-to-self, or use the certifier path; vault balance provably unchanged; close refused while retention remains |
| Beat 2 — backstop | early claim rejected; only `demo_authority` can roll the clock; **sub claims with only its own signature** and the vault closes in the same tx; rent reserve never paid out as retention; double-claim rejected |
| Certificate path | `attest_cpc` releases exactly the first moiety and leaves the vault open; certificate-then-backstop releases everything; replay rejected; moiety cannot be redirected; wrong certifier rejected |
| `close_vault` | rent refund cannot be redirected; close is permissionless yet pays only Verita |

## Deploy to devnet

`Anchor.toml` already targets devnet.

```bash
cd verita

# 1. Fund the deploy keypair (see the DEMO-PLAN keypair set for the other four)
solana airdrop 2 --url devnet

# 2. Deploy
anchor deploy --provider.cluster devnet

# 3. Sync the declared program ID with the deployed one, then rebuild.
#    The ID appears in programs/verita/src/lib.rs (declare_id!) and Anchor.toml.
anchor keys sync
anchor build
anchor deploy --provider.cluster devnet

# 4. Publish the IDL for the frontend
anchor idl build --out ../../verita.idl.json
```

Before the final judged recording, consider revoking upgrade authority so the
program is immutable:

```bash
solana program set-upgrade-authority <PROGRAM_ID> --final --url devnet
```

## Program overview

### PDA `retention_vault`

Seeds: `["vault", sha256(project_id), subcontractor, main_contractor]`

`project_id` is hashed to a fixed 32 bytes so any-length project name fits the
32-byte-per-seed limit; the raw string (≤64 bytes) is stored on the account for
display. Because the seed is a hash, **Anchor cannot express it in the IDL** — the
frontend must derive the address explicitly (snippet in the API contract §1).

### Instructions

| Instruction | Signer | Effect |
|---|---|---|
| `fund_vault` | `main_contractor` + `rent_payer` | Contractor transfers the retention in via system CPI; Verita funds the account rent. |
| `claim_release` | `subcontractor` | Releases everything undisputed once `effective_ts >= backstop_ts`. No contractor signature. |
| `attest_cpc` | `certifier` | Releases the first moiety per `release_schedule_bps`. Does not close the account. |
| `close_vault` | *(permissionless)* | Once fully released, closes and refunds the rent reserve to the stored `rent_payer`. |
| `advance_clock` | `demo_authority` | Demo builds only: sets the absolute `clock_offset`. |

There is no `attest_cmgd` in the core slice — the second moiety leaves via the
backstop `claim_release`. Frontend should bundle `claim_release` + `close_vault` in
one transaction on the final claim.

### Security invariants

- **No contractor withdrawal path.** Retention lives in a program-owned PDA; no instruction pays the contractor.
- **Backstop gating.** `claim_release` requires `effective_ts >= practical_completion_ts + dlp_days + grace_days`.
- **Authority binding.** `has_one` constraints tie `subcontractor`, `certifier`, `rent_payer` and `demo_authority` to the values stored at funding, so none can be substituted at call time.
- **Rent floor.** Every release asserts the PDA stays at or above its rent-exempt minimum, so Verita's reserve is never paid out as retention.
- **Rent refund cannot be redirected.** `close_vault` pays the *stored* `rent_payer`.
- **Exact lamports, no dust.** Native-SOL transfers are integral; `close_vault` asserts `released_cumulative == amount` before closing.
- **Demo clock is feature-gated.** `advance_clock` does not exist in a production build.
- **Checked arithmetic** throughout; no `unwrap()` on sysvars.

## Outstanding

- Not yet deployed to devnet; the program ID in `declare_id!` is still the scaffold placeholder. Run `anchor keys sync` after the first deploy.
- Defect/adjudication path (`raise_defect_claim`, `resolve_defect`, `neutral_locked`) is designed but not built — STRETCH per the demo plan. The `aggregate_frozen` / `max_active_freezes` fields exist and are read by `claimable_now()`, but are always zero in the core slice.

## Layout

```
verita/
├── programs/verita/
│   ├── src/
│   │   ├── lib.rs          # program module, instruction entrypoints
│   │   ├── state.rs        # RetentionVault, seeds/status consts, derived helpers, unit tests
│   │   ├── instructions.rs # account contexts + handlers
│   │   └── error.rs        # VeritaError
│   └── Cargo.toml          # `demo` feature (default on)
├── Anchor.toml             # devnet provider + program IDs
└── Cargo.toml
```
