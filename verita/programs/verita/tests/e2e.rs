//! End-to-end tests for the Tahan/Verita retention vault, run against `litesvm`.
//!
//! Requires the SBF artifact to exist: run `anchor build` first (which is what
//! `anchor test` does before invoking `cargo test`).
//!
//! The suite drives the demo money-shot in order:
//!   fund -> contractor cannot claw back -> early claim rejected
//!         -> advance_clock -> sub claims alone -> close (rent back to Verita)
//! plus regression tests for each bug found in the pre-deploy code review.

use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{AnchorDeserialize, InstructionData, Space, ToAccountMetas};
use litesvm::LiteSVM;
use solana_keypair::Keypair;
use solana_message::Message;
use solana_signer::Signer;
use solana_transaction::Transaction;
use std::path::PathBuf;

use verita::instructions::project_seed;
use verita::state::{RetentionVault, STATUS_CPC_RELEASED, STATUS_FUNDED, VAULT_SEED};

// Anchor error codes, in declaration order from `VeritaError`.
const E_UNAUTHORIZED: u32 = 6001;
const E_BACKSTOP_NOT_REACHED: u32 = 6002;
const E_ALREADY_ATTESTED: u32 = 6004;
const E_NONZERO_RESIDUAL: u32 = 6005;
const E_INVALID_AMOUNT: u32 = 6009;

const SOL: u64 = 1_000_000_000;
const RETENTION: u64 = 120 * SOL; // stands in for RM120k in the demo narrative
const BPS_HALF: u16 = 5_000;

// Far-future completion so the backstop is unreachable at the simulator's clock,
// letting us prove "early claim rejected" before rolling time forward.
const PC_TS: i64 = 2_000_000_000; // 2033-05-18
const DLP_DAYS: u32 = 365;
const GRACE_DAYS: u32 = 14;
/// Offset large enough that `now + offset` is past the backstop.
const OFFSET_PAST_BACKSTOP: i64 = 2_100_000_000;

fn program_so() -> Vec<u8> {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // programs/
    path.pop(); // workspace root
    path.push("target/deploy/verita.so");
    std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "could not read {} ({e}). Run `anchor build` before `cargo test`.",
            path.display()
        )
    })
}

/// Assert a transaction failed with a specific Anchor custom error code.
fn assert_custom_err<T, E: std::fmt::Debug>(res: Result<T, E>, code: u32) {
    match res {
        Ok(_) => panic!("expected failure with custom error {code}, but the tx succeeded"),
        Err(e) => {
            let rendered = format!("{e:?}");
            assert!(
                rendered.contains(&format!("Custom({code})")),
                "expected custom error {code}, got: {rendered}"
            );
        }
    }
}

struct Fixture {
    svm: LiteSVM,
    program_id: Pubkey,
    contractor: Keypair,
    verita: Keypair, // rent_payer + demo_authority (merged platform key)
    sub: Keypair,
    certifier: Keypair,
    adjudicator: Pubkey,
    vault: Pubkey,
    project_id: String,
    rent: u64,
}

use anchor_lang::prelude::Pubkey;

impl Fixture {
    fn new(project_id: &str) -> Self {
        let mut svm = LiteSVM::new();
        let program_id = verita::ID;
        svm.add_program(program_id, &program_so()).unwrap();

        let contractor = Keypair::new();
        let verita_kp = Keypair::new();
        let sub = Keypair::new();
        let certifier = Keypair::new();
        let adjudicator = Pubkey::new_unique();

        for kp in [&contractor, &verita_kp, &sub, &certifier] {
            svm.airdrop(&kp.pubkey(), 500 * SOL).unwrap();
        }

        let (vault, _) = Pubkey::find_program_address(
            &[
                VAULT_SEED,
                &project_seed(project_id),
                sub.pubkey().as_ref(),
                contractor.pubkey().as_ref(),
            ],
            &program_id,
        );

        let rent = svm.minimum_balance_for_rent_exemption(8 + RetentionVault::INIT_SPACE);

        Self {
            svm,
            program_id,
            contractor,
            verita: verita_kp,
            sub,
            certifier,
            adjudicator,
            vault,
            project_id: project_id.to_string(),
            rent,
        }
    }

    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> litesvm::types::TransactionResult {
        let blockhash = self.svm.latest_blockhash();
        let payer = signers[0].pubkey();
        let msg = Message::new_with_blockhash(ixs, Some(&payer), &blockhash);
        let tx = Transaction::new(&signers.to_vec(), msg, blockhash);
        self.svm.send_transaction(tx)
    }

    fn fund_ix(&self, amount: u64, bps: u16) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: verita::accounts::FundVault {
                main_contractor: self.contractor.pubkey(),
                rent_payer: self.verita.pubkey(),
                vault: self.vault,
                system_program: anchor_lang::system_program::ID,
            }
            .to_account_metas(None),
            data: verita::instruction::FundVault {
                args: verita::instructions::FundVaultArgs {
                    project_id: self.project_id.clone(),
                    amount,
                    practical_completion_ts: PC_TS,
                    dlp_days: DLP_DAYS,
                    grace_days: GRACE_DAYS,
                    release_schedule_bps: bps,
                    subcontractor: self.sub.pubkey(),
                    certifier: self.certifier.pubkey(),
                    adjudicator: self.adjudicator,
                    demo_authority: self.verita.pubkey(),
                },
            }
            .data(),
        }
    }

    fn fund(&mut self) -> litesvm::types::TransactionResult {
        let ix = self.fund_ix(RETENTION, BPS_HALF);
        let contractor = self.contractor.insecure_clone();
        let verita = self.verita.insecure_clone();
        self.send(&[ix], &[&contractor, &verita])
    }

    /// `claim_release` signed by `signer` in the subcontractor slot.
    fn claim_ix_as(&self, signer: Pubkey) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: verita::accounts::ClaimRelease {
                subcontractor: signer,
                vault: self.vault,
                rent_payer: self.verita.pubkey(),
            }
            .to_account_metas(None),
            data: verita::instruction::ClaimRelease {}.data(),
        }
    }

    fn claim_ix(&self) -> Instruction {
        self.claim_ix_as(self.sub.pubkey())
    }

    fn attest_ix_with_sub(&self, sub_account: Pubkey) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: verita::accounts::AttestCpc {
                certifier: self.certifier.pubkey(),
                vault: self.vault,
                subcontractor: sub_account,
            }
            .to_account_metas(None),
            data: verita::instruction::AttestCpc {}.data(),
        }
    }

    fn attest_ix(&self) -> Instruction {
        self.attest_ix_with_sub(self.sub.pubkey())
    }

    fn close_ix_to(&self, rent_dest: Pubkey) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: verita::accounts::CloseVault {
                rent_payer: rent_dest,
                vault: self.vault,
            }
            .to_account_metas(None),
            data: verita::instruction::CloseVault {}.data(),
        }
    }

    fn close_ix(&self) -> Instruction {
        self.close_ix_to(self.verita.pubkey())
    }

    fn advance_clock_ix_as(&self, authority: Pubkey, offset: i64) -> Instruction {
        Instruction {
            program_id: self.program_id,
            accounts: verita::accounts::AdvanceClock {
                demo_authority: authority,
                vault: self.vault,
            }
            .to_account_metas(None),
            data: verita::instruction::AdvanceClock {
                args: verita::instructions::AdvanceClockArgs { new_offset: offset },
            }
            .data(),
        }
    }

    fn roll_time_past_backstop(&mut self) {
        let ix = self.advance_clock_ix_as(self.verita.pubkey(), OFFSET_PAST_BACKSTOP);
        let verita = self.verita.insecure_clone();
        self.send(&[ix], &[&verita]).unwrap();
    }

    fn vault_state(&self) -> RetentionVault {
        let data = self.svm.get_account(&self.vault).expect("vault exists").data;
        RetentionVault::deserialize(&mut &data[8..]).unwrap()
    }

    fn vault_lamports(&self) -> u64 {
        self.svm.get_balance(&self.vault).unwrap_or(0)
    }

    fn balance(&self, key: &Pubkey) -> u64 {
        self.svm.get_balance(key).unwrap_or(0)
    }
}

// ============================================================================
// fund_vault
// ============================================================================

/// Regression: `fund_vault` previously credited the PDA without debiting the
/// contractor, which the runtime rejects — no vault could ever be funded.
#[test]
fn funding_moves_real_lamports_into_the_pda() {
    let mut f = Fixture::new("DEMO-01");
    let contractor_before = f.balance(&f.contractor.pubkey());
    let verita_before = f.balance(&f.verita.pubkey());

    f.fund().unwrap();

    // Retention left the contractor and now sits in the program-owned PDA.
    assert_eq!(f.vault_lamports(), RETENTION + f.rent);
    // The contractor is also the fee payer here, so allow for the signature fee.
    let contractor_debit = contractor_before - f.balance(&f.contractor.pubkey());
    assert!(
        (RETENTION..RETENTION + 100_000).contains(&contractor_debit),
        "contractor should be debited the retention plus only a tx fee; debited {contractor_debit}"
    );
    // Verita fronted only the rent, not the retention.
    assert!(verita_before - f.balance(&f.verita.pubkey()) >= f.rent);

    let v = f.vault_state();
    assert_eq!(v.main_contractor, f.contractor.pubkey());
    assert_eq!(v.subcontractor, f.sub.pubkey());
    assert_eq!(v.certifier, f.certifier.pubkey());
    assert_eq!(v.adjudicator, f.adjudicator);
    assert_eq!(v.demo_authority, f.verita.pubkey());
    assert_eq!(v.rent_payer, f.verita.pubkey());
    assert_eq!(v.mint, Pubkey::default());
    assert_eq!(v.amount, RETENTION);
    assert_eq!(v.released_cumulative, 0);
    assert_eq!(v.status, STATUS_FUNDED);
    assert!(!v.cpc_attested);
}

/// Regression: the account was under-allocated by 24 bytes, so any `project_id`
/// longer than 40 bytes failed to serialize.
#[test]
fn funding_accepts_a_max_length_project_id() {
    let long_id = "x".repeat(64);
    let mut f = Fixture::new(&long_id);
    f.fund().unwrap();
    assert_eq!(f.vault_state().project_id, long_id);
}

#[test]
fn funding_rejects_zero_amount() {
    let mut f = Fixture::new("DEMO-ZERO");
    let ix = f.fund_ix(0, BPS_HALF);
    let contractor = f.contractor.insecure_clone();
    let verita = f.verita.insecure_clone();
    assert_custom_err(f.send(&[ix], &[&contractor, &verita]), E_INVALID_AMOUNT);
}

#[test]
fn a_vault_cannot_be_funded_twice() {
    let mut f = Fixture::new("DEMO-DOUBLE");
    f.fund().unwrap();
    assert!(f.fund().is_err(), "re-initialising the same PDA must fail");
}

// ============================================================================
// Beat 1 — the contractor cannot claw the retention back
// ============================================================================

#[test]
fn contractor_has_no_withdrawal_path() {
    let mut f = Fixture::new("DEMO-REFUSAL");
    f.fund().unwrap();
    let before = f.vault_lamports();

    // (a) Contractor tries to claim as though it were the subcontractor.
    let ix = f.claim_ix_as(f.contractor.pubkey());
    let contractor = f.contractor.insecure_clone();
    assert_custom_err(f.send(&[ix], &[&contractor]), E_UNAUTHORIZED);

    // (b) Contractor tries to close the vault and collect the balance.
    let ix = f.close_ix_to(f.contractor.pubkey());
    let contractor = f.contractor.insecure_clone();
    assert!(f.send(&[ix], &[&contractor]).is_err());

    // (c) Contractor tries the certifier's release path, paying itself.
    let contractor = f.contractor.insecure_clone();
    let ix = Instruction {
        program_id: f.program_id,
        accounts: verita::accounts::AttestCpc {
            certifier: contractor.pubkey(),
            vault: f.vault,
            subcontractor: contractor.pubkey(),
        }
        .to_account_metas(None),
        data: verita::instruction::AttestCpc {}.data(),
    };
    assert_custom_err(f.send(&[ix], &[&contractor]), E_UNAUTHORIZED);

    // Not one lamport of retention moved.
    assert_eq!(f.vault_lamports(), before);
    assert_eq!(f.vault_state().released_cumulative, 0);
}

/// Even a legitimate close is refused while retention remains.
#[test]
fn close_is_refused_while_retention_remains() {
    let mut f = Fixture::new("DEMO-RESIDUAL");
    f.fund().unwrap();
    let ix = f.close_ix();
    let verita = f.verita.insecure_clone();
    assert_custom_err(f.send(&[ix], &[&verita]), E_NONZERO_RESIDUAL);
}

// ============================================================================
// Beat 2 — time-backstop, then the sub releases alone
// ============================================================================

#[test]
fn claim_before_the_backstop_is_rejected() {
    let mut f = Fixture::new("DEMO-EARLY");
    f.fund().unwrap();

    let ix = f.claim_ix();
    let sub = f.sub.insecure_clone();
    assert_custom_err(f.send(&[ix], &[&sub]), E_BACKSTOP_NOT_REACHED);
    assert_eq!(f.vault_lamports(), RETENTION + f.rent);
}

#[test]
fn only_the_demo_authority_can_roll_the_clock() {
    let mut f = Fixture::new("DEMO-CLOCK-AUTH");
    f.fund().unwrap();

    for impostor in [f.contractor.insecure_clone(), f.sub.insecure_clone()] {
        let ix = f.advance_clock_ix_as(impostor.pubkey(), OFFSET_PAST_BACKSTOP);
        assert_custom_err(f.send(&[ix], &[&impostor]), E_UNAUTHORIZED);
    }
    assert_eq!(f.vault_state().clock_offset, 0);
}

/// The money shot: after the backstop the sub extracts everything undisputed
/// with **only its own signature**, and the vault closes in the same transaction
/// with the rent going back to Verita.
#[test]
fn sub_claims_alone_after_backstop_and_vault_closes() {
    let mut f = Fixture::new("DEMO-MONEYSHOT");
    f.fund().unwrap();
    f.roll_time_past_backstop();

    let sub_before = f.balance(&f.sub.pubkey());
    let verita_before = f.balance(&f.verita.pubkey());

    // One transaction, one signature — the subcontractor's.
    let ixs = [f.claim_ix(), f.close_ix()];
    let sub = f.sub.insecure_clone();
    f.send(&ixs, &[&sub]).unwrap();

    // Sub received the full retention (minus the tx fee it paid as payer).
    let sub_gain = f.balance(&f.sub.pubkey()) + 10_000 - sub_before;
    assert!(
        sub_gain >= RETENTION,
        "sub should net the full retention; gained {sub_gain}"
    );

    // Vault is gone and Verita has its rent back.
    assert!(
        f.svm.get_account(&f.vault).is_none_or(|a| a.lamports == 0),
        "vault account should be closed"
    );
    assert_eq!(f.balance(&f.verita.pubkey()), verita_before + f.rent);
}

#[test]
fn claim_alone_leaves_only_the_rent_reserve() {
    let mut f = Fixture::new("DEMO-RENTFLOOR");
    f.fund().unwrap();
    f.roll_time_past_backstop();

    let ix = f.claim_ix();
    let sub = f.sub.insecure_clone();
    f.send(&[ix], &[&sub]).unwrap();

    // The rent reserve is never paid out as retention.
    assert_eq!(f.vault_lamports(), f.rent);
    let v = f.vault_state();
    assert_eq!(v.released_cumulative, v.amount);
}

#[test]
fn second_claim_after_full_release_is_rejected() {
    let mut f = Fixture::new("DEMO-DOUBLECLAIM");
    f.fund().unwrap();
    f.roll_time_past_backstop();

    let ix = f.claim_ix();
    let sub = f.sub.insecure_clone();
    f.send(&[ix], &[&sub]).unwrap();

    let ix = f.claim_ix();
    let sub = f.sub.insecure_clone();
    assert!(
        f.send(&[ix], &[&sub]).is_err(),
        "nothing should remain to claim"
    );
}

// ============================================================================
// Certificate path (attest_cpc) + hybrid release
// ============================================================================

#[test]
fn attest_cpc_releases_only_the_first_moiety_and_keeps_the_vault_open() {
    let mut f = Fixture::new("DEMO-CPC");
    f.fund().unwrap();

    let sub_before = f.balance(&f.sub.pubkey());
    let ix = f.attest_ix();
    let certifier = f.certifier.insecure_clone();
    f.send(&[ix], &[&certifier]).unwrap();

    let half = RETENTION / 2;
    assert_eq!(f.balance(&f.sub.pubkey()), sub_before + half);
    assert_eq!(f.vault_lamports(), RETENTION - half + f.rent);

    let v = f.vault_state();
    assert_eq!(v.released_cumulative, half);
    assert_eq!(v.status, STATUS_CPC_RELEASED);
    assert!(v.cpc_attested);
}

/// The hybrid design: certificate frees the first half, the time-backstop frees
/// the rest. There is no `attest_cmgd` in the core slice.
#[test]
fn certificate_then_backstop_releases_everything() {
    let mut f = Fixture::new("DEMO-HYBRID");
    f.fund().unwrap();

    let ix = f.attest_ix();
    let certifier = f.certifier.insecure_clone();
    f.send(&[ix], &[&certifier]).unwrap();

    // Second moiety is not claimable until the backstop.
    let ix = f.claim_ix();
    let sub = f.sub.insecure_clone();
    assert_custom_err(f.send(&[ix], &[&sub]), E_BACKSTOP_NOT_REACHED);

    f.roll_time_past_backstop();

    let ixs = [f.claim_ix(), f.close_ix()];
    let sub = f.sub.insecure_clone();
    f.send(&ixs, &[&sub]).unwrap();

    assert!(f.svm.get_account(&f.vault).is_none_or(|a| a.lamports == 0));
}

#[test]
fn attest_cpc_cannot_be_replayed() {
    let mut f = Fixture::new("DEMO-CPC-REPLAY");
    f.fund().unwrap();

    let ix = f.attest_ix();
    let certifier = f.certifier.insecure_clone();
    f.send(&[ix], &[&certifier]).unwrap();

    f.svm.expire_blockhash();
    let ix = f.attest_ix();
    let certifier = f.certifier.insecure_clone();
    assert_custom_err(f.send(&[ix], &[&certifier]), E_ALREADY_ATTESTED);
}

/// Regression: `attest_cpc` did not bind the passed `subcontractor` account to
/// `vault.subcontractor`, so the moiety could be paid to any address.
#[test]
fn attest_cpc_cannot_redirect_the_moiety() {
    let mut f = Fixture::new("DEMO-CPC-REDIRECT");
    f.fund().unwrap();

    let thief = Pubkey::new_unique();
    let ix = f.attest_ix_with_sub(thief);
    let certifier = f.certifier.insecure_clone();
    assert_custom_err(f.send(&[ix], &[&certifier]), E_UNAUTHORIZED);
    assert_eq!(f.balance(&thief), 0);
}

#[test]
fn attest_cpc_requires_the_named_certifier() {
    let mut f = Fixture::new("DEMO-CPC-AUTH");
    f.fund().unwrap();

    let impostor = f.contractor.insecure_clone();
    let ix = Instruction {
        program_id: f.program_id,
        accounts: verita::accounts::AttestCpc {
            certifier: impostor.pubkey(),
            vault: f.vault,
            subcontractor: f.sub.pubkey(),
        }
        .to_account_metas(None),
        data: verita::instruction::AttestCpc {}.data(),
    };
    assert_custom_err(f.send(&[ix], &[&impostor]), E_UNAUTHORIZED);
}

// ============================================================================
// close_vault
// ============================================================================

/// Regression: `rent_payer` was unvalidated, so a caller could redirect the rent
/// refund to themselves.
#[test]
fn close_cannot_redirect_the_rent_refund() {
    let mut f = Fixture::new("DEMO-RENT-REDIRECT");
    f.fund().unwrap();
    f.roll_time_past_backstop();

    let ix = f.claim_ix();
    let sub = f.sub.insecure_clone();
    f.send(&[ix], &[&sub]).unwrap();

    let thief_kp = Keypair::new();
    f.svm.airdrop(&thief_kp.pubkey(), SOL).unwrap();
    let ix = f.close_ix_to(thief_kp.pubkey());
    assert_custom_err(f.send(&[ix], &[&thief_kp]), E_UNAUTHORIZED);

    // Vault survives with its rent intact.
    assert_eq!(f.vault_lamports(), f.rent);
}

/// Closing needs no contractor and no subcontractor authority — anyone can crank
/// it, and the rent can only go to the party that fronted it.
#[test]
fn close_is_permissionless_but_pays_only_verita() {
    let mut f = Fixture::new("DEMO-PERMISSIONLESS-CLOSE");
    f.fund().unwrap();
    f.roll_time_past_backstop();

    let ix = f.claim_ix();
    let sub = f.sub.insecure_clone();
    f.send(&[ix], &[&sub]).unwrap();

    let verita_before = f.balance(&f.verita.pubkey());

    // A wholly unrelated third party pays the fee and cranks the close.
    let stranger = Keypair::new();
    f.svm.airdrop(&stranger.pubkey(), SOL).unwrap();
    let ix = f.close_ix();
    f.send(&[ix], &[&stranger]).unwrap();

    assert!(f.svm.get_account(&f.vault).is_none_or(|a| a.lamports == 0));
    assert_eq!(f.balance(&f.verita.pubkey()), verita_before + f.rent);
}
