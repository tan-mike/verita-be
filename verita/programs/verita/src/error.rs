use anchor_lang::prelude::*;

#[error_code]
pub enum VeritaError {
    #[msg("This vault is already funded")]
    AlreadyFunded,

    #[msg("Your wallet isn't authorized for this action")]
    Unauthorized,

    #[msg("Retention isn't claimable yet — DLP hasn't expired")]
    BackstopNotReached,

    #[msg("Nothing left to claim")]
    NothingToClaim,

    #[msg("First moiety already certified")]
    AlreadyAttested,

    #[msg("Vault still holds funds; can't close")]
    NonzeroResidual,

    #[msg("Vault is already closed")]
    AlreadyClosed,

    #[msg("Vault cannot be claimed in current state")]
    VaultNotClaimable,

    #[msg("Project ID exceeds maximum length of 64 bytes")]
    ProjectIdTooLong,

    #[msg("Retention amount must be greater than zero")]
    InvalidAmount,

    #[msg("Release schedule must be between 1 and 10000 basis points")]
    InvalidReleaseSchedule,

    #[msg("Release would strand the vault below its rent-exempt minimum")]
    RentExemptViolation,

    #[msg("Arithmetic overflow or underflow")]
    MathOverflow,

    #[msg("Division by zero")]
    DivisionByZero,

    // ---- STRETCH (defect/adjudication path, not implemented in the core slice) ----
    #[msg("Defect window has closed")]
    FreezeAfterCutoff,

    #[msg("Freeze exceeds the aggregate cap")]
    AggregateCapExceeded,

    #[msg("Too many active freezes")]
    TooManyFreezes,
}
