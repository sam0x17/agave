#![cfg(feature = "agave-unstable-api")]
//! Leader Schedule native program.
//!
//! This program owns the on-chain leader schedule accounts but rejects all
//! instructions. The accounts are updated exclusively by the runtime at epoch
//! boundaries.

use solana_program_runtime::declare_process_instruction;

solana_pubkey::declare_id!("8WDMPtpgHx5Xj3AJR2LMyXoD6ci2xDcT6GpJtAHZJwdW");

pub const DEFAULT_COMPUTE_UNITS: u64 = 150;

declare_process_instruction!(Entrypoint, DEFAULT_COMPUTE_UNITS, |_invoke_context| {
    // The leader schedule program rejects all instructions.
    // Accounts are updated only by the runtime at epoch boundaries.
    Err(solana_program_runtime::__private::InstructionError::InvalidInstructionData)
});
