#![cfg(feature = "agave-unstable-api")]
//! Owner address and binary format for runtime-managed epoch stakes accounts.
//!
//! No program is deployed at this address. The runtime publishes and closes
//! the accounts at epoch boundaries.

pub mod state;

solana_pubkey::declare_id!("EpochStakes11111111111111111111111111111111");
