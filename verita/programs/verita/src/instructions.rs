use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer, Transfer};

use crate::error::VeritaError;
use crate::state::{
    RetentionVault, MAX_PROJECT_ID_LEN, STATUS_CLOSED, STATUS_CPC_RELEASED, STATUS_FUNDED,
    VAULT_SEED,
};

/// sha256 of the project id, used as a fixed-32-byte PDA seed component.
pub fn project_seed(project_id: &str) -> [u8; 32] {
    solana_sha256_hasher::hash(project_id.as_bytes()).to_bytes()
}

/// Move `lamports` of retention out of the program-owned vault PDA to `recipient`.
///
/// The vault is program-owned, so lamports are moved by direct arithmetic rather
/// than a system CPI. Guards that the vault never drops below its rent-exempt
/// minimum, so the rent reserve (owned by `rent_payer`) is never paid out as
/// retention.
fn release_from_vault<'info>(
    vault: &Account<'info, RetentionVault>,
    recipient: &AccountInfo<'info>,
    lamports: u64,
) -> Result<()> {
    let vault_ai = vault.to_account_info();
    let rent_minimum = Rent::get()?.minimum_balance(vault_ai.data_len());

    let vault_post = vault_ai
        .lamports()
        .checked_sub(lamports)
        .ok_or(VeritaError::MathOverflow)?;
    require!(vault_post >= rent_minimum, VeritaError::RentExemptViolation);

    let recipient_post = recipient
        .lamports()
        .checked_add(lamports)
        .ok_or(VeritaError::MathOverflow)?;

    **vault_ai.try_borrow_mut_lamports()? = vault_post;
    **recipient.try_borrow_mut_lamports()? = recipient_post;

    Ok(())
}

// ============================================================================
// fund_vault — core
// ============================================================================

#[derive(Accounts)]
#[instruction(args: FundVaultArgs)]
pub struct FundVault<'info> {
    /// Deposits the retention `amount`. Has no withdrawal path after this.
    #[account(mut)]
    pub main_contractor: Signer<'info>,

    /// Verita/platform: funds the account rent, receives it back on close.
    #[account(mut)]
    pub rent_payer: Signer<'info>,

    #[account(
        init,
        payer = rent_payer,
        space = 8 + RetentionVault::INIT_SPACE,
        seeds = [
            VAULT_SEED,
            &project_seed(&args.project_id),
            args.subcontractor.as_ref(),
            main_contractor.key().as_ref(),
        ],
        bump
    )]
    pub vault: Account<'info, RetentionVault>,

    pub system_program: Program<'info, System>,
}

#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct FundVaultArgs {
    pub project_id: String,
    pub amount: u64,
    pub practical_completion_ts: i64,
    pub dlp_days: u32,
    pub grace_days: u32,
    pub release_schedule_bps: u16,
    pub subcontractor: Pubkey,
    pub certifier: Pubkey,
    pub adjudicator: Pubkey,
    pub demo_authority: Pubkey,
}

pub fn handle_fund_vault(ctx: Context<FundVault>, args: FundVaultArgs) -> Result<()> {
    require!(
        args.project_id.len() <= MAX_PROJECT_ID_LEN,
        VeritaError::ProjectIdTooLong
    );
    require!(args.amount > 0, VeritaError::InvalidAmount);
    require!(
        args.release_schedule_bps > 0 && (args.release_schedule_bps as u64) <= 10_000,
        VeritaError::InvalidReleaseSchedule
    );

    let bump = ctx.bumps.vault;

    // Move the retention into the program-owned PDA. This must be a system CPI:
    // the contractor is a system-owned account, so the program cannot debit it
    // directly. From here the funds are outside the contractor's control.
    transfer(
        CpiContext::new(
            ctx.accounts.system_program.key(),
            Transfer {
                from: ctx.accounts.main_contractor.to_account_info(),
                to: ctx.accounts.vault.to_account_info(),
            },
        ),
        args.amount,
    )?;

    let vault = &mut ctx.accounts.vault;
    vault.main_contractor = ctx.accounts.main_contractor.key();
    vault.subcontractor = args.subcontractor;
    vault.certifier = args.certifier;
    vault.adjudicator = args.adjudicator;
    vault.demo_authority = args.demo_authority;
    vault.rent_payer = ctx.accounts.rent_payer.key();
    vault.mint = Pubkey::default(); // native SOL in the demo; kept swappable
    vault.project_id = args.project_id;
    vault.amount = args.amount;
    vault.released_cumulative = 0;
    vault.practical_completion_ts = args.practical_completion_ts;
    vault.dlp_days = args.dlp_days;
    vault.grace_days = args.grace_days;
    vault.release_schedule_bps = args.release_schedule_bps;
    vault.cpc_attested = false;
    vault.aggregate_frozen = 0; // STRETCH
    vault.active_freeze_count = 0; // STRETCH
    vault.aggregate_freeze_cap = 0; // STRETCH
    vault.max_active_freezes = 0; // STRETCH
    vault.clock_offset = 0;
    vault.status = STATUS_FUNDED;
    vault.bump = bump;

    Ok(())
}

// ============================================================================
// claim_release — core (subcontractor only, backstop-gated)
// ============================================================================

#[derive(Accounts)]
pub struct ClaimRelease<'info> {
    #[account(mut)]
    pub subcontractor: Signer<'info>,

    #[account(
        mut,
        has_one = subcontractor @ VeritaError::Unauthorized,
        has_one = rent_payer @ VeritaError::Unauthorized,
        seeds = [
            VAULT_SEED,
            &project_seed(&vault.project_id),
            vault.subcontractor.as_ref(),
            vault.main_contractor.as_ref(),
        ],
        bump = vault.bump
    )]
    pub vault: Account<'info, RetentionVault>,

    /// Verita/platform. Not a signer and not paid here; bound so that the
    /// permissionless `close_vault` finalizer can only refund rent to the
    /// party that actually fronted it.
    pub rent_payer: SystemAccount<'info>,
}

pub fn handle_claim_release(ctx: Context<ClaimRelease>) -> Result<()> {
    let vault = &ctx.accounts.vault;

    require!(
        vault.status == STATUS_FUNDED || vault.status == STATUS_CPC_RELEASED,
        VeritaError::VaultNotClaimable
    );

    // Time-backstop: the sub may extract everything undisputed once the DLP plus
    // grace has elapsed — with no counterparty signature.
    require!(
        vault.effective_ts()? >= vault.backstop_ts()?,
        VeritaError::BackstopNotReached
    );

    let claimable = vault.claimable_now()?;
    require!(claimable > 0, VeritaError::NothingToClaim);

    release_from_vault(
        &ctx.accounts.vault,
        &ctx.accounts.subcontractor.to_account_info(),
        claimable,
    )?;

    let vault = &mut ctx.accounts.vault;
    vault.released_cumulative = vault
        .released_cumulative
        .checked_add(claimable)
        .ok_or(VeritaError::MathOverflow)?;

    Ok(())
}

// ============================================================================
// attest_cpc — core (certifier releases the first moiety)
// ============================================================================

#[derive(Accounts)]
pub struct AttestCpc<'info> {
    pub certifier: Signer<'info>,

    #[account(
        mut,
        has_one = certifier @ VeritaError::Unauthorized,
        has_one = subcontractor @ VeritaError::Unauthorized,
        seeds = [
            VAULT_SEED,
            &project_seed(&vault.project_id),
            vault.subcontractor.as_ref(),
            vault.main_contractor.as_ref(),
        ],
        bump = vault.bump
    )]
    pub vault: Account<'info, RetentionVault>,

    /// Receives the first moiety. Bound to `vault.subcontractor` above.
    #[account(mut)]
    pub subcontractor: SystemAccount<'info>,
}

pub fn handle_attest_cpc(ctx: Context<AttestCpc>) -> Result<()> {
    let vault = &ctx.accounts.vault;

    require!(vault.status == STATUS_FUNDED, VeritaError::AlreadyAttested);
    require!(!vault.cpc_attested, VeritaError::AlreadyAttested);

    // Release only the first moiety. There is no CMGD instruction in the core
    // slice — the remainder leaves via the backstop `claim_release`.
    let moiety = vault.first_moiety()?;
    require!(moiety > 0, VeritaError::NothingToClaim);
    require!(
        moiety <= vault.claimable_now()?,
        VeritaError::InvalidReleaseSchedule
    );

    release_from_vault(
        &ctx.accounts.vault,
        &ctx.accounts.subcontractor.to_account_info(),
        moiety,
    )?;

    let vault = &mut ctx.accounts.vault;
    vault.released_cumulative = vault
        .released_cumulative
        .checked_add(moiety)
        .ok_or(VeritaError::MathOverflow)?;
    vault.cpc_attested = true;
    vault.status = STATUS_CPC_RELEASED;

    Ok(())
}

// ============================================================================
// close_vault — core (permissionless finalizer; rent back to Verita)
// ============================================================================

#[derive(Accounts)]
pub struct CloseVault<'info> {
    /// Verita/platform receives the rent reserve. Bound to `vault.rent_payer`,
    /// so a caller cannot redirect the refund.
    #[account(mut)]
    pub rent_payer: SystemAccount<'info>,

    #[account(
        mut,
        close = rent_payer,
        has_one = rent_payer @ VeritaError::Unauthorized,
        constraint = vault.status != STATUS_CLOSED @ VeritaError::AlreadyClosed,
        constraint = vault.is_fully_released() @ VeritaError::NonzeroResidual,
        constraint = vault.aggregate_frozen == 0 @ VeritaError::NonzeroResidual,
        seeds = [
            VAULT_SEED,
            &project_seed(&vault.project_id),
            vault.subcontractor.as_ref(),
            vault.main_contractor.as_ref(),
        ],
        bump = vault.bump
    )]
    pub vault: Account<'info, RetentionVault>,
}

pub fn handle_close_vault(_ctx: Context<CloseVault>) -> Result<()> {
    // Closure and the rent refund are performed by the `close = rent_payer`
    // constraint. No contractor or subcontractor signature is required.
    Ok(())
}

// ============================================================================
// advance_clock — DEMO BUILDS ONLY
// ============================================================================
// Compiled out entirely (instruction, accounts and handler) unless the `demo`
// feature is enabled, so a production build has no way to shift time.

#[cfg(feature = "demo")]
#[derive(Accounts)]
pub struct AdvanceClock<'info> {
    pub demo_authority: Signer<'info>,

    #[account(
        mut,
        has_one = demo_authority @ VeritaError::Unauthorized,
        seeds = [
            VAULT_SEED,
            &project_seed(&vault.project_id),
            vault.subcontractor.as_ref(),
            vault.main_contractor.as_ref(),
        ],
        bump = vault.bump
    )]
    pub vault: Account<'info, RetentionVault>,
}

#[cfg(feature = "demo")]
#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct AdvanceClockArgs {
    /// Absolute offset: `effective_ts = Clock::unix_timestamp + new_offset`.
    pub new_offset: i64,
}

#[cfg(feature = "demo")]
pub fn handle_advance_clock(ctx: Context<AdvanceClock>, args: AdvanceClockArgs) -> Result<()> {
    ctx.accounts.vault.clock_offset = args.new_offset;
    Ok(())
}
