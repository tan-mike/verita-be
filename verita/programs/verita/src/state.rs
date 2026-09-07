use anchor_lang::prelude::*;

use crate::error::VeritaError;

/// PDA seed prefix for `RetentionVault`.
pub const VAULT_SEED: &[u8] = b"vault";

/// Maximum stored `project_id` length in bytes (account sizing bound).
pub const MAX_PROJECT_ID_LEN: usize = 64;

pub const SECONDS_PER_DAY: i64 = 86_400;
pub const BPS_DENOMINATOR: u64 = 10_000;

// Vault status discriminants. Kept as `u8` so the account layout is stable and
// the frontend can map values directly (see API-CONTRACT-retention.md §2.1).
pub const STATUS_FUNDED: u8 = 0;
pub const STATUS_CPC_RELEASED: u8 = 1;
pub const STATUS_DISPUTED: u8 = 2;
pub const STATUS_NEUTRAL_LOCKED: u8 = 3;
pub const STATUS_CLOSED: u8 = 4;

/// PDA: one retention vault per (project, subcontractor, main contractor).
///
/// Seeds: `["vault", sha256(project_id), subcontractor, main_contractor]`
///
/// `project_id` is hashed to a fixed 32 bytes for the seed so any-length project
/// name fits the 32-byte-per-seed limit, and the raw string is stored here for
/// display/lookup.
#[account]
#[derive(Debug, InitSpace)]
pub struct RetentionVault {
    /// Main contractor who deposited the retention. Cannot withdraw after funding.
    pub main_contractor: Pubkey,
    /// Subcontractor — sole caller of `claim_release`.
    pub subcontractor: Pubkey,
    /// CA/architect authority for `attest_cpc`.
    pub certifier: Pubkey,
    /// Named adjudicator (STRETCH — unused in the core slice).
    pub adjudicator: Pubkey,
    /// Demo authority — sole caller of `advance_clock` (demo builds only).
    pub demo_authority: Pubkey,
    /// Verita/platform: fronts the account rent and receives the refund on close.
    pub rent_payer: Pubkey,
    /// Mint, kept swappable. `Pubkey::default()` (unused) in the native-SOL demo.
    pub mint: Pubkey,
    /// Project identifier, stored for display/lookup (its sha256 is the PDA seed).
    #[max_len(64)]
    pub project_id: String,
    /// Total retention deposited, lamports.
    pub amount: u64,
    /// Running total released — a cumulative sum, never a boolean.
    pub released_cumulative: u64,
    /// Unix timestamp of practical completion.
    pub practical_completion_ts: i64,
    /// Defects-liability period length, days.
    pub dlp_days: u32,
    /// Grace days added after the DLP before the sub backstop opens.
    pub grace_days: u32,
    /// First-moiety share in basis points (e.g. 5000 = 50%).
    pub release_schedule_bps: u16,
    /// First-moiety certificate attested.
    pub cpc_attested: bool,
    /// Sum of active freezes (STRETCH; always 0 in the core slice).
    pub aggregate_frozen: u64,
    /// Count of active freezes (STRETCH; always 0 in the core slice).
    pub active_freeze_count: u16,
    /// Aggregate freeze cap (STRETCH).
    pub aggregate_freeze_cap: u64,
    /// Active-freeze count bound (STRETCH).
    pub max_active_freezes: u16,
    /// Demo-only clock offset. Read **only** in demo builds — see `effective_ts`.
    pub clock_offset: i64,
    /// Current status; see the `STATUS_*` constants.
    pub status: u8,
    /// PDA bump.
    pub bump: u8,
}

impl RetentionVault {
    /// Effective "now" used for every time comparison in the program.
    ///
    /// In **demo** builds the stored `clock_offset` is added so an operator can
    /// roll time forward on stage. In **production** builds (`--no-default-features`)
    /// the offset is never read, so production cannot be made to trust a stored
    /// offset even though the field remains in the layout.
    pub fn effective_ts(&self) -> Result<i64> {
        let now = Clock::get()?.unix_timestamp;

        #[cfg(feature = "demo")]
        let ts = now
            .checked_add(self.clock_offset)
            .ok_or(VeritaError::MathOverflow)?;

        #[cfg(not(feature = "demo"))]
        let ts = now;

        Ok(ts)
    }

    /// Timestamp at which the subcontractor's unilateral backstop opens:
    /// `practical_completion_ts + dlp_days + grace_days`.
    pub fn backstop_ts(&self) -> Result<i64> {
        let dlp = (self.dlp_days as i64)
            .checked_mul(SECONDS_PER_DAY)
            .ok_or(VeritaError::MathOverflow)?;
        let grace = (self.grace_days as i64)
            .checked_mul(SECONDS_PER_DAY)
            .ok_or(VeritaError::MathOverflow)?;

        self.practical_completion_ts
            .checked_add(dlp)
            .and_then(|t| t.checked_add(grace))
            .ok_or_else(|| VeritaError::MathOverflow.into())
    }

    /// Undisputed retention currently releasable:
    /// `amount − released_cumulative − aggregate_frozen`.
    pub fn claimable_now(&self) -> Result<u64> {
        self.amount
            .checked_sub(self.released_cumulative)
            .and_then(|r| r.checked_sub(self.aggregate_frozen))
            .ok_or_else(|| VeritaError::MathOverflow.into())
    }

    /// First moiety per the release schedule.
    pub fn first_moiety(&self) -> Result<u64> {
        self.amount
            .checked_mul(self.release_schedule_bps as u64)
            .ok_or(VeritaError::MathOverflow)?
            .checked_div(BPS_DENOMINATOR)
            .ok_or_else(|| VeritaError::DivisionByZero.into())
    }

    /// True once every lamport of retention has been released.
    pub fn is_fully_released(&self) -> bool {
        self.released_cumulative == self.amount
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault_with(amount: u64, bps: u16, project_id: &str) -> RetentionVault {
        RetentionVault {
            main_contractor: Pubkey::new_unique(),
            subcontractor: Pubkey::new_unique(),
            certifier: Pubkey::new_unique(),
            adjudicator: Pubkey::new_unique(),
            demo_authority: Pubkey::new_unique(),
            rent_payer: Pubkey::new_unique(),
            mint: Pubkey::default(),
            project_id: project_id.to_string(),
            amount,
            released_cumulative: 0,
            practical_completion_ts: 1_700_000_000,
            dlp_days: 365,
            grace_days: 14,
            release_schedule_bps: bps,
            cpc_attested: false,
            aggregate_frozen: 0,
            active_freeze_count: 0,
            aggregate_freeze_cap: 0,
            max_active_freezes: 0,
            clock_offset: 0,
            status: STATUS_FUNDED,
            bump: 255,
        }
    }

    /// Regression test for the under-allocation bug: the hand-summed `space`
    /// literal previously counted only 6 of the 7 pubkeys (341 vs 365 bytes),
    /// so any `project_id` longer than 40 bytes failed to serialize.
    #[test]
    fn init_space_fits_a_max_length_project_id() {
        // 7 pubkeys + (4 + 64) string + 65 bytes of scalars
        assert_eq!(RetentionVault::INIT_SPACE, 224 + 68 + 65);

        let max_id = "x".repeat(MAX_PROJECT_ID_LEN);
        let vault = vault_with(1_000_000_000, 5_000, &max_id);

        let mut serialized = Vec::new();
        vault.serialize(&mut serialized).unwrap();
        assert!(
            serialized.len() <= RetentionVault::INIT_SPACE,
            "serialized {} bytes exceeds INIT_SPACE {}",
            serialized.len(),
            RetentionVault::INIT_SPACE
        );
    }

    #[test]
    fn backstop_is_pc_plus_dlp_plus_grace() {
        let vault = vault_with(1_000_000_000, 5_000, "demo");
        let expected = 1_700_000_000 + (365 * SECONDS_PER_DAY) + (14 * SECONDS_PER_DAY);
        assert_eq!(vault.backstop_ts().unwrap(), expected);
    }

    #[test]
    fn first_moiety_and_claimable_track_released_cumulative() {
        let mut vault = vault_with(1_000_000_000, 5_000, "demo");

        assert_eq!(vault.first_moiety().unwrap(), 500_000_000);
        assert_eq!(vault.claimable_now().unwrap(), 1_000_000_000);
        assert!(!vault.is_fully_released());

        // after the CPC moiety leaves, only the remainder is claimable
        vault.released_cumulative = 500_000_000;
        assert_eq!(vault.claimable_now().unwrap(), 500_000_000);
        assert!(!vault.is_fully_released());

        vault.released_cumulative = 1_000_000_000;
        assert_eq!(vault.claimable_now().unwrap(), 0);
        assert!(vault.is_fully_released());
    }

    #[test]
    fn frozen_slice_is_excluded_from_claimable() {
        let mut vault = vault_with(1_000_000_000, 5_000, "demo");
        vault.aggregate_frozen = 250_000_000;
        assert_eq!(vault.claimable_now().unwrap(), 750_000_000);
    }
}
