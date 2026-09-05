//! Cost of `TransactionNotifierImpl::notify_transaction` per transaction shape.
//!
//! Each shape is measured three ways: a plugin that wants transaction
//! notifications (conversion + dispatch), a plugin that does not (whatever work
//! happens before the per-plugin check), and no plugins at all. A counting
//! allocator reports allocations per call before the timing runs.
#![allow(clippy::arithmetic_side_effects)]

use {
    agave_geyser_plugin_interface::geyser_plugin_interface::{
        GeyserPlugin, ReplicaTransactionInfoVersions, Result as PluginResult,
    },
    arc_swap::ArcSwap,
    criterion::{BenchmarkId, Criterion},
    libloading::Library,
    solana_account_decoder_client_types::token::UiTokenAmount,
    solana_clock::{BankId, Slot},
    solana_geyser_plugin_manager::{
        geyser_plugin_manager::{GeyserPluginManager, LoadedGeyserPlugin},
        transaction_notifier::TransactionNotifierImpl,
    },
    solana_hash::Hash,
    solana_message::{compiled_instruction::CompiledInstruction, v0::LoadedAddresses},
    solana_pubkey::Pubkey,
    solana_reward_info::RewardType,
    solana_rpc::transaction_notifier_interface::TransactionNotifier,
    solana_signature::Signature,
    solana_transaction::versioned::VersionedTransaction,
    solana_transaction_context::transaction::TransactionReturnData,
    solana_transaction_status::{
        InnerInstruction, InnerInstructions, Reward, TransactionStatusMeta, TransactionTokenBalance,
    },
    std::{
        alloc::{GlobalAlloc, Layout, System},
        hint::black_box,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    },
};

// ---------------------------------------------------------------------------
// Counting allocator
// ---------------------------------------------------------------------------

struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations_of(mut f: impl FnMut()) -> (usize, usize) {
    let a0 = ALLOCS.load(Ordering::Relaxed);
    let b0 = BYTES.load(Ordering::Relaxed);
    f();
    (
        ALLOCS.load(Ordering::Relaxed) - a0,
        BYTES.load(Ordering::Relaxed) - b0,
    )
}

// ---------------------------------------------------------------------------
// Plugin
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct NoopPlugin {
    enabled: bool,
    seen: AtomicUsize,
}

impl GeyserPlugin for NoopPlugin {
    fn name(&self) -> &'static str {
        "bench-noop"
    }

    fn transaction_notifications_enabled(&self) -> bool {
        self.enabled
    }

    fn notify_transaction_for_bank(
        &self,
        transaction_info: ReplicaTransactionInfoVersions,
        _slot: Slot,
        _bank_id: BankId,
    ) -> PluginResult<()> {
        let ReplicaTransactionInfoVersions::V0_0_4(info) = transaction_info;
        black_box(info.transaction_status_meta);
        self.seen.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn notifier(plugins: Vec<NoopPlugin>) -> TransactionNotifierImpl {
    let plugins = plugins
        .into_iter()
        .map(|plugin| {
            #[cfg(unix)]
            let library = libloading::os::unix::Library::this();
            #[cfg(windows)]
            let library = libloading::os::windows::Library::this().unwrap();
            Arc::new(LoadedGeyserPlugin::new(
                Library::from(library),
                Box::new(plugin),
                None,
            ))
        })
        .collect();
    TransactionNotifierImpl::new(Arc::new(ArcSwap::from(Arc::new(GeyserPluginManager {
        plugins,
    }))))
}

// ---------------------------------------------------------------------------
// Transaction shapes
// ---------------------------------------------------------------------------

struct Shape {
    name: &'static str,
    inner_groups: usize,
    inner_per_group: usize,
    logs: usize,
    token_balances: usize,
    rewards: usize,
    loaded_addresses: usize,
    return_data: usize,
}

const SHAPES: [Shape; 3] = [
    Shape {
        name: "vote",
        inner_groups: 0,
        inner_per_group: 0,
        logs: 0,
        token_balances: 0,
        rewards: 0,
        loaded_addresses: 0,
        return_data: 0,
    },
    Shape {
        name: "swap",
        inner_groups: 6,
        inner_per_group: 4,
        logs: 12,
        token_balances: 4,
        rewards: 0,
        loaded_addresses: 8,
        return_data: 32,
    },
    Shape {
        name: "heavy",
        inner_groups: 30,
        inner_per_group: 8,
        logs: 64,
        token_balances: 16,
        rewards: 2,
        loaded_addresses: 16,
        return_data: 256,
    },
];

fn token_balance(i: usize) -> TransactionTokenBalance {
    TransactionTokenBalance {
        account_index: i as u8,
        mint: Pubkey::new_unique().to_string(),
        ui_token_amount: UiTokenAmount {
            ui_amount: Some(1.5),
            decimals: 6,
            amount: "1500000".to_string(),
            ui_amount_string: "1.5".to_string(),
        },
        owner: Pubkey::new_unique().to_string(),
        program_id: Pubkey::new_unique().to_string(),
    }
}

fn build_meta(shape: &Shape) -> TransactionStatusMeta {
    let some = |n: usize| n > 0;
    TransactionStatusMeta {
        status: Ok(()),
        fee: 5000,
        pre_balances: vec![1_000_000; 4 + shape.loaded_addresses],
        post_balances: vec![995_000; 4 + shape.loaded_addresses],
        inner_instructions: some(shape.inner_groups).then(|| {
            (0..shape.inner_groups)
                .map(|g| InnerInstructions {
                    index: g as u8,
                    instructions: (0..shape.inner_per_group)
                        .map(|i| InnerInstruction {
                            instruction: CompiledInstruction {
                                program_id_index: (i % 4) as u8,
                                accounts: vec![0, 1, 2],
                                data: vec![i as u8; 16],
                            },
                            stack_height: Some(2),
                        })
                        .collect(),
                })
                .collect()
        }),
        log_messages: some(shape.logs).then(|| {
            (0..shape.logs)
                .map(|i| format!("Program log: instruction {i} consumed 12345 of 200000 units"))
                .collect()
        }),
        pre_token_balances: some(shape.token_balances)
            .then(|| (0..shape.token_balances).map(token_balance).collect()),
        post_token_balances: some(shape.token_balances)
            .then(|| (0..shape.token_balances).map(token_balance).collect()),
        rewards: some(shape.rewards).then(|| {
            (0..shape.rewards)
                .map(|i| Reward {
                    pubkey: Pubkey::new_unique().to_string(),
                    lamports: i as i64,
                    post_balance: 10,
                    reward_type: Some(RewardType::Rent),
                    commission: None,
                    commission_bps: None,
                })
                .collect()
        }),
        loaded_addresses: LoadedAddresses {
            writable: (0..shape.loaded_addresses / 2)
                .map(|_| Pubkey::new_unique())
                .collect(),
            readonly: (0..shape.loaded_addresses / 2)
                .map(|_| Pubkey::new_unique())
                .collect(),
        },
        return_data: some(shape.return_data).then(|| TransactionReturnData {
            program_id: Pubkey::new_unique(),
            data: vec![7; shape.return_data],
        }),
        compute_units_consumed: Some(150_000),
        cost_units: Some(2_000),
    }
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

const SCENARIOS: [(&str, Option<bool>); 3] = [
    ("plugin_enabled", Some(true)),
    ("plugin_disabled", Some(false)),
    ("no_plugins", None),
];

fn notifier_for(scenario: Option<bool>) -> TransactionNotifierImpl {
    match scenario {
        Some(enabled) => notifier(vec![NoopPlugin {
            enabled,
            seen: AtomicUsize::new(0),
        }]),
        None => notifier(vec![]),
    }
}

fn notify(n: &TransactionNotifierImpl, meta: &TransactionStatusMeta, tx: &VersionedTransaction) {
    n.notify_transaction(
        42,
        9,
        3,
        black_box(&Signature::default()),
        black_box(&Hash::default()),
        false,
        black_box(meta),
        black_box(tx),
    );
}

fn print_allocation_report() {
    const ITERS: usize = 1000;
    println!("\nallocations per notify_transaction call (averaged over {ITERS} calls)");
    println!(
        "{:<8} {:<16} {:>8} {:>10}",
        "shape", "scenario", "allocs", "bytes"
    );
    for shape in &SHAPES {
        let meta = build_meta(shape);
        let tx = VersionedTransaction::default();
        for (name, scenario) in &SCENARIOS {
            let n = notifier_for(*scenario);
            // warm anything lazily initialized
            notify(&n, &meta, &tx);
            let (allocs, bytes) = allocations_of(|| {
                for _ in 0..ITERS {
                    notify(&n, &meta, &tx);
                }
            });
            println!(
                "{:<8} {:<16} {:>8.1} {:>10.0}",
                shape.name,
                name,
                allocs as f64 / ITERS as f64,
                bytes as f64 / ITERS as f64
            );
        }
    }
    println!();
}

fn bench_notify_transaction(c: &mut Criterion) {
    let mut group = c.benchmark_group("notify_transaction");
    for shape in &SHAPES {
        let meta = build_meta(shape);
        let tx = VersionedTransaction::default();
        for (name, scenario) in &SCENARIOS {
            let n = notifier_for(*scenario);
            group.bench_with_input(
                BenchmarkId::new(shape.name, name),
                &(&n, &meta, &tx),
                |b, (n, meta, tx)| b.iter(|| notify(n, meta, tx)),
            );
        }
    }
    group.finish();
}

fn main() {
    print_allocation_report();
    let mut criterion = Criterion::default().configure_from_args();
    bench_notify_transaction(&mut criterion);
    criterion.final_summary();
}
