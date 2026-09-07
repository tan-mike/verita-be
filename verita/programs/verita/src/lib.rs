pub mod error;
pub mod instructions;
pub mod state;

use anchor_lang::prelude::*;

pub use error::*;
pub use instructions::*;
pub use state::*;

declare_id!("2BjVNW3YL7LigwWvRCWgYrxqZjWYwp8RJmLt2S1k9e5c");

#[program]
pub mod verita {
    use super::*;

    /// Main contractor deposits the retention; Verita fronts the account rent.
    /// From this point the funds sit in a program-owned PDA — the contractor has
    /// no withdrawal path.
    pub fn fund_vault(ctx: Context<FundVault>, args: FundVaultArgs) -> Result<()> {
        handle_fund_vault(ctx, args)
    }

    /// Subcontractor unilaterally releases everything undisputed once the
    /// time-backstop has passed. Requires no counterparty signature.
    pub fn claim_release(ctx: Context<ClaimRelease>) -> Result<()> {
        handle_claim_release(ctx)
    }

    /// Certifier (architect/CA) releases the first moiety — the normal fast path.
    pub fn attest_cpc(ctx: Context<AttestCpc>) -> Result<()> {
        handle_attest_cpc(ctx)
    }

    /// Permissionless finalizer once all retention has been released. Refunds the
    /// rent reserve to the stored `rent_payer` (Verita), never the contractor.
    pub fn close_vault(ctx: Context<CloseVault>) -> Result<()> {
        handle_close_vault(ctx)
    }

    /// Demo-only: roll the effective clock forward. Absent from production builds
    /// (`--no-default-features`).
    #[cfg(feature = "demo")]
    pub fn advance_clock(ctx: Context<AdvanceClock>, args: AdvanceClockArgs) -> Result<()> {
        handle_advance_clock(ctx, args)
    }
}
