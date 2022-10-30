use crate::{
    truncate_path, CheckpointError, CheckpointMetrics, PageMapType, PersistenceError,
    NUMBER_OF_CHECKPOINT_THREADS,
};
use ic_base_types::CanisterId;
use ic_logger::ReplicaLogger;
use ic_registry_subnet_type::SubnetType;
use ic_replicated_state::Memory;
use ic_replicated_state::{
    bitcoin_state::{BitcoinState, UtxoSet},
    canister_state::execution_state::WasmBinary,
    page_map::PageMap,
    CanisterMetrics, CanisterState, ExecutionState, NumWasmPages, ReplicatedState, SchedulerState,
    SystemState,
};
use ic_state_layout::{
    BitcoinStateBits, BitcoinStateLayout, CanisterLayout, CanisterStateBits, CheckpointLayout,
    ExecutionStateBits, ReadOnly, ReadPolicy, RwPolicy, StateLayout,
};
use ic_types::time::UNIX_EPOCH;
use ic_types::{CanisterTimer, Height, LongExecutionMode, Time};
use ic_utils::fs::defrag_file_partially;
use ic_utils::thread::parallel_map;
use rand::prelude::SliceRandom;
use rand::{seq::IteratorRandom, Rng, SeedableRng};
use rand_chacha::ChaChaRng;
use std::collections::BTreeMap;
use std::os::unix::prelude::MetadataExt;
use std::time::{Duration, Instant};
use std::{
    convert::{From, TryFrom},
    path::{Path, PathBuf},
};

const DEFRAG_SIZE: u64 = 1 << 29; // 500 MB
const DEFRAG_SAMPLE: usize = 100;

/// Creates a checkpoint of the node state using specified directory
/// layout. Returns a new state that is equivalent to the given one
/// and a result of the operation.
///
/// This function uses the provided thread-pool to parallelize expensive
/// operations.
///
/// If the result is `Ok`, the returned state is "rebased" to use
/// files from the newly created checkpoint. If the result is `Err`,
/// the returned state is exactly the one that was passed as argument.
pub fn make_checkpoint(
    state: &ReplicatedState,
    height: Height,
    layout: &StateLayout,
    log: &ReplicaLogger,
    metrics: &CheckpointMetrics,
    thread_pool: &mut scoped_threadpool::Pool,
) -> Result<ReplicatedState, CheckpointError> {
    let tip = layout.tip(height)?;

    {
        let _timer = metrics
            .make_checkpoint_step_duration
            .with_label_values(&["serialize_to_tip"])
            .start_timer();
        serialize_to_tip(log, state, &tip, thread_pool)?;
    }

    {
        let _timer = metrics
            .make_checkpoint_step_duration
            .with_label_values(&["defrag_tip"])
            .start_timer();
        defrag_tip(
            &tip,
            &PageMapType::list_all(state),
            DEFRAG_SIZE,
            DEFRAG_SAMPLE,
            height.get(),
        )?;
    }

    {
        let _timer = metrics
            .make_checkpoint_step_duration
            .with_label_values(&["filter_canisters"])
            .start_timer();
        layout.filter_tip_canisters(height, &state.canister_states.keys().collect())?;
    }

    let cp = {
        let _timer = metrics
            .make_checkpoint_step_duration
            .with_label_values(&["tip_to_checkpoint"])
            .start_timer();
        layout.tip_to_checkpoint(tip, Some(thread_pool))?
    };

    let state = {
        let _timer = metrics
            .make_checkpoint_step_duration
            .with_label_values(&["load"])
            .start_timer();
        load_checkpoint(
            &cp,
            state.metadata.own_subnet_type,
            metrics,
            Some(thread_pool),
        )?
    };

    Ok(state)
}

fn serialize_to_tip(
    log: &ReplicaLogger,
    state: &ReplicatedState,
    tip: &CheckpointLayout<RwPolicy>,
    thread_pool: &mut scoped_threadpool::Pool,
) -> Result<(), CheckpointError> {
    tip.system_metadata()
        .serialize(state.system_metadata().into())?;

    tip.subnet_queues()
        .serialize((state.subnet_queues()).into())?;

    let results = parallel_map(thread_pool, state.canisters_iter(), |canister_state| {
        serialize_canister_to_tip(log, canister_state, tip)
    });

    for result in results.into_iter() {
        result?;
    }

    serialize_bitcoin_state_to_tip(state.bitcoin(), &tip.bitcoin()?)?;

    Ok(())
}

fn serialize_canister_to_tip(
    log: &ReplicaLogger,
    canister_state: &CanisterState,
    tip: &CheckpointLayout<RwPolicy>,
) -> Result<(), CheckpointError> {
    let canister_layout = tip.canister(&canister_state.canister_id())?;
    canister_layout
        .queues()
        .serialize(canister_state.system_state.queues().into())?;

    let execution_state_bits = match &canister_state.execution_state {
        Some(execution_state) => {
            let wasm_binary = &execution_state.wasm_binary.binary;
            match wasm_binary.file() {
                Some(path) => {
                    let wasm = canister_layout.wasm();
                    if !wasm.raw_path().exists() {
                        ic_state_layout::utils::do_copy(log, path, wasm.raw_path()).map_err(
                            |io_err| CheckpointError::IoError {
                                path: path.to_path_buf(),
                                message: "failed to copy Wasm file".to_string(),
                                io_err: io_err.to_string(),
                            },
                        )?;
                    }
                }
                None => {
                    // Canister was installed/upgraded. Persist the new wasm binary.
                    canister_layout
                        .wasm()
                        .serialize(&execution_state.wasm_binary.binary)?;
                }
            }
            execution_state
                .wasm_memory
                .page_map
                .persist_delta(&canister_layout.vmemory_0())?;
            execution_state
                .stable_memory
                .page_map
                .persist_delta(&canister_layout.stable_memory_blob())?;

            Some(ExecutionStateBits {
                exported_globals: execution_state.exported_globals.clone(),
                heap_size: execution_state.wasm_memory.size,
                exports: execution_state.exports.clone(),
                last_executed_round: execution_state.last_executed_round,
                metadata: execution_state.metadata.clone(),
                binary_hash: Some(execution_state.wasm_binary.binary.module_hash().into()),
            })
        }
        None => {
            truncate_path(log, &canister_layout.vmemory_0());
            truncate_path(log, &canister_layout.stable_memory_blob());
            None
        }
    };
    // Priority credit must be zero at this point
    assert_eq!(canister_state.scheduler_state.priority_credit.value(), 0);
    canister_layout
        .canister()
        .serialize(
            CanisterStateBits {
                controllers: canister_state.system_state.controllers.clone(),
                last_full_execution_round: canister_state.scheduler_state.last_full_execution_round,
                call_context_manager: canister_state.system_state.call_context_manager().cloned(),
                compute_allocation: canister_state.scheduler_state.compute_allocation,
                accumulated_priority: canister_state.scheduler_state.accumulated_priority,
                memory_allocation: canister_state.system_state.memory_allocation,
                freeze_threshold: canister_state.system_state.freeze_threshold,
                cycles_balance: canister_state.system_state.balance(),
                cycles_debit: canister_state.system_state.cycles_debit(),
                execution_state_bits,
                status: canister_state.system_state.status.clone(),
                scheduled_as_first: canister_state
                    .system_state
                    .canister_metrics
                    .scheduled_as_first,
                skipped_round_due_to_no_messages: canister_state
                    .system_state
                    .canister_metrics
                    .skipped_round_due_to_no_messages,
                executed: canister_state.system_state.canister_metrics.executed,
                interruped_during_execution: canister_state
                    .system_state
                    .canister_metrics
                    .interruped_during_execution,
                certified_data: canister_state.system_state.certified_data.clone(),
                consumed_cycles_since_replica_started: canister_state
                    .system_state
                    .canister_metrics
                    .consumed_cycles_since_replica_started,
                stable_memory_size: canister_state
                    .execution_state
                    .as_ref()
                    .map(|es| es.stable_memory.size)
                    .unwrap_or_else(|| NumWasmPages::from(0)),
                heap_delta_debit: canister_state.scheduler_state.heap_delta_debit,
                install_code_debit: canister_state.scheduler_state.install_code_debit,
                time_of_last_allocation_charge_nanos: Some(
                    canister_state
                        .scheduler_state
                        .time_of_last_allocation_charge
                        .as_nanos_since_unix_epoch(),
                ),
                task_queue: canister_state
                    .system_state
                    .task_queue
                    .clone()
                    .into_iter()
                    .collect(),
                global_timer_nanos: canister_state
                    .system_state
                    .global_timer
                    .to_nanos_since_unix_epoch(),
            }
            .into(),
        )
        .map_err(CheckpointError::from)
}

fn serialize_bitcoin_state_to_tip(
    state: &BitcoinState,
    layout: &BitcoinStateLayout<RwPolicy>,
) -> Result<(), CheckpointError> {
    state
        .utxo_set
        .utxos_small
        .persist_delta(&layout.utxos_small())?;

    state
        .utxo_set
        .utxos_medium
        .persist_delta(&layout.utxos_medium())?;

    state
        .utxo_set
        .address_outpoints
        .persist_delta(&layout.address_outpoints())?;

    layout
        .bitcoin_state()
        .serialize(
            // TODO(EXC-1076): Remove unnecessary clone.
            (&BitcoinStateBits {
                adapter_queues: state.adapter_queues.clone(),
                unstable_blocks: state.unstable_blocks.clone(),
                stable_height: state.stable_height,
                network: state.utxo_set.network,
                utxos_large: state.utxo_set.utxos_large.clone(),
            })
                .into(),
        )
        .map_err(CheckpointError::from)
}

/// Defragments part of the tip directory.
///
/// The way we use PageMap files in the tip, namely by having a
/// long-living file, that we alternatively write to in small 4KB
/// pages and reflink copy to the checkpoint folder, the files end up
/// fragmented on disk. In particular, the metadata the file system
/// keeps on which pages are shared between files and which pages are
/// unique to a file grows quite complicated, which noticebly slows
/// down reflink copying of those files. It can therefore be
/// beneficial to defragment files, especially in situations where a
/// file had a lot of writes in the past but is mostly being read now.
///
/// The current defragmentation strategy is to pseudorandomly choose a
/// chunk of size max_size among the eligble files, read it to memory,
/// and write it back to the file. The effect is that this chunk is
/// definitely unique to the tip at the end of defragmentation. For
/// now, only the bitcoin PageMap files are being considered.
fn defrag_tip(
    tip: &CheckpointLayout<RwPolicy>,
    page_maps: &[PageMapType],
    max_size: u64,
    max_files: usize,
    seed: u64,
) -> Result<(), CheckpointError> {
    let mut rng = ChaChaRng::seed_from_u64(seed);

    // We sample the set of page maps down in order to avoid reading
    // the metadata of each file. This is a compromise between
    // weighting the probabilities by size and picking a uniformly
    // random file.  The former (without subsampling) would be
    // unnecessarily expensive, the latter would perform poorly in a
    // situation with many empty files and a few large ones, doing
    // no-ops on empty files with high probability.
    let page_map_subset = page_maps.iter().choose_multiple(&mut rng, max_files);

    let path_with_sizes: Vec<(PathBuf, u64)> = page_map_subset
        .iter()
        .filter_map(|entry| {
            let path = entry.path(tip).ok()?;
            let size = path.metadata().ok()?.size();
            Some((path, size))
        })
        .collect();

    // We choose a file weighted by its size. This way, every bit in
    // the state has (roughly) the same probability of being
    // defragmented. If we chose the file uniformaly at random, we
    // would end up defragmenting the smallest file too often. The choice
    // failing is not an error, as it will happen if all files are
    // empty
    if let Ok((path, size)) = path_with_sizes.choose_weighted(&mut rng, |entry| entry.1) {
        let write_size = size.min(&max_size);
        let offset = rng.gen_range(0..=size - write_size);

        defrag_file_partially(path, offset, write_size.to_owned() as usize).map_err(|err| {
            CheckpointError::IoError {
                path: path.to_path_buf(),
                message: "failed to defrag file".into(),
                io_err: err.to_string(),
            }
        })?;
    }
    Ok(())
}

/// Calls [load_checkpoint] with a newly created thread pool.
/// See [load_checkpoint] for further details.
pub fn load_checkpoint_parallel<P: ReadPolicy + Send + Sync>(
    checkpoint_layout: &CheckpointLayout<P>,
    own_subnet_type: SubnetType,
    metrics: &CheckpointMetrics,
) -> Result<ReplicatedState, CheckpointError> {
    let mut thread_pool = scoped_threadpool::Pool::new(NUMBER_OF_CHECKPOINT_THREADS);

    load_checkpoint(
        checkpoint_layout,
        own_subnet_type,
        metrics,
        Some(&mut thread_pool),
    )
}

/// loads the node state heighted with `height` using the specified
/// directory layout.
pub fn load_checkpoint<P: ReadPolicy + Send + Sync>(
    checkpoint_layout: &CheckpointLayout<P>,
    own_subnet_type: SubnetType,
    metrics: &CheckpointMetrics,
    thread_pool: Option<&mut scoped_threadpool::Pool>,
) -> Result<ReplicatedState, CheckpointError> {
    let into_checkpoint_error =
        |field: String, err: ic_protobuf::proxy::ProxyDecodeError| CheckpointError::ProtoError {
            path: checkpoint_layout.raw_path().into(),
            field,
            proto_err: err.to_string(),
        };

    let metadata = {
        let _timer = metrics
            .load_checkpoint_step_duration
            .with_label_values(&["system_metadata"])
            .start_timer();

        let mut metadata = ic_replicated_state::SystemMetadata::try_from(
            checkpoint_layout.system_metadata().deserialize()?,
        )
        .map_err(|err| into_checkpoint_error("SystemMetadata".into(), err))?;
        metadata.own_subnet_type = own_subnet_type;
        metadata
    };

    let subnet_queues = {
        let _timer = metrics
            .load_checkpoint_step_duration
            .with_label_values(&["subnet_queues"])
            .start_timer();

        ic_replicated_state::CanisterQueues::try_from(
            checkpoint_layout.subnet_queues().deserialize()?,
        )
        .map_err(|err| into_checkpoint_error("CanisterQueues".into(), err))?
    };

    let canister_states = {
        let _timer = metrics
            .load_checkpoint_step_duration
            .with_label_values(&["canister_states"])
            .start_timer();

        let mut canister_states = BTreeMap::new();
        let canister_ids = checkpoint_layout.canister_ids()?;
        match thread_pool {
            Some(thread_pool) => {
                let results = parallel_map(thread_pool, canister_ids.iter(), |canister_id| {
                    load_canister_state_from_checkpoint(checkpoint_layout, canister_id)
                });

                for canister_state in results.into_iter() {
                    let (canister_state, durations) = canister_state?;
                    canister_states
                        .insert(canister_state.system_state.canister_id(), canister_state);

                    durations.apply(metrics);
                }
            }
            None => {
                for canister_id in canister_ids.iter() {
                    let (canister_state, durations) =
                        load_canister_state_from_checkpoint(checkpoint_layout, canister_id)?;
                    canister_states
                        .insert(canister_state.system_state.canister_id(), canister_state);

                    durations.apply(metrics);
                }
            }
        }

        canister_states
    };

    let bitcoin = {
        let _timer = metrics
            .load_checkpoint_step_duration
            .with_label_values(&["bitcoin"])
            .start_timer();

        load_bitcoin_state(checkpoint_layout)?
    };

    let state = ReplicatedState::new_from_checkpoint(
        canister_states,
        metadata,
        subnet_queues,
        // Consensus queue needs to be empty at the end of every round.
        Vec::new(),
        bitcoin,
    );

    Ok(state)
}

#[derive(Default)]
pub struct LoadCanisterMetrics {
    durations: BTreeMap<&'static str, Duration>,
}

impl LoadCanisterMetrics {
    pub fn apply(&self, metrics: &CheckpointMetrics) {
        for (key, duration) in &self.durations {
            metrics
                .load_canister_step_duration
                .with_label_values(&[key])
                .observe(duration.as_secs_f64());
        }
    }
}

pub fn load_canister_state<P: ReadPolicy>(
    canister_layout: &CanisterLayout<P>,
    canister_id: &CanisterId,
    height: Height,
) -> Result<(CanisterState, LoadCanisterMetrics), CheckpointError> {
    let mut durations = BTreeMap::<&str, Duration>::default();

    let into_checkpoint_error =
        |field: String, err: ic_protobuf::proxy::ProxyDecodeError| CheckpointError::ProtoError {
            path: canister_layout.raw_path(),
            field,
            proto_err: err.to_string(),
        };

    let starting_time = Instant::now();
    let canister_state_bits: CanisterStateBits =
        CanisterStateBits::try_from(canister_layout.canister().deserialize()?).map_err(|err| {
            into_checkpoint_error(
                format!("canister_states[{}]::canister_state_bits", canister_id),
                err,
            )
        })?;
    durations.insert("canister_state_bits", starting_time.elapsed());

    let session_nonce = None;

    let execution_state = match canister_state_bits.execution_state_bits {
        Some(execution_state_bits) => {
            let starting_time = Instant::now();
            let wasm_memory = Memory::new(
                PageMap::open(&canister_layout.vmemory_0(), height)?,
                execution_state_bits.heap_size,
            );
            durations.insert("wasm_memory", starting_time.elapsed());

            let starting_time = Instant::now();
            let stable_memory = Memory::new(
                PageMap::open(&canister_layout.stable_memory_blob(), height)?,
                canister_state_bits.stable_memory_size,
            );
            durations.insert("stable_memory", starting_time.elapsed());

            let starting_time = Instant::now();
            let wasm_binary = WasmBinary::new(
                canister_layout
                    .wasm()
                    .deserialize(execution_state_bits.binary_hash)?,
            );
            durations.insert("wasm_binary", starting_time.elapsed());

            let canister_root = CheckpointLayout::<ReadOnly>::new("NOT_USED".into(), height)?
                .canister(canister_id)?
                .raw_path();
            Some(ExecutionState {
                canister_root,
                session_nonce,
                wasm_binary,
                wasm_memory,
                stable_memory,
                exported_globals: execution_state_bits.exported_globals,
                exports: execution_state_bits.exports,
                metadata: execution_state_bits.metadata,
                last_executed_round: execution_state_bits.last_executed_round,
            })
        }
        None => None,
    };

    let starting_time = Instant::now();
    let queues =
        ic_replicated_state::CanisterQueues::try_from(canister_layout.queues().deserialize()?)
            .map_err(|err| {
                into_checkpoint_error(
                    format!("canister_states[{}]::system_state::queues", canister_id),
                    err,
                )
            })?;
    durations.insert("canister_queues", starting_time.elapsed());

    let canister_metrics = CanisterMetrics {
        scheduled_as_first: canister_state_bits.scheduled_as_first,
        skipped_round_due_to_no_messages: canister_state_bits.skipped_round_due_to_no_messages,
        executed: canister_state_bits.executed,
        interruped_during_execution: canister_state_bits.interruped_during_execution,
        consumed_cycles_since_replica_started: canister_state_bits
            .consumed_cycles_since_replica_started,
    };
    let system_state = SystemState::new_from_checkpoint(
        canister_state_bits.controllers,
        *canister_id,
        queues,
        canister_state_bits.memory_allocation,
        canister_state_bits.freeze_threshold,
        canister_state_bits.status,
        canister_state_bits.certified_data,
        canister_metrics,
        canister_state_bits.cycles_balance,
        canister_state_bits.cycles_debit,
        canister_state_bits.task_queue.into_iter().collect(),
        CanisterTimer::from_nanos_since_unix_epoch(canister_state_bits.global_timer_nanos),
    );

    let canister_state = CanisterState {
        system_state,
        execution_state,
        scheduler_state: SchedulerState {
            last_full_execution_round: canister_state_bits.last_full_execution_round,
            compute_allocation: canister_state_bits.compute_allocation,
            accumulated_priority: canister_state_bits.accumulated_priority,
            // Longs executions get aborted at the checkpoint,
            // so both the credit and the execution mode below are set to their defaults.
            priority_credit: 0.into(),
            long_execution_mode: LongExecutionMode::default(),
            heap_delta_debit: canister_state_bits.heap_delta_debit,
            install_code_debit: canister_state_bits.install_code_debit,
            // TODO(EXC-1214): Ensure field is always set to some value.
            time_of_last_allocation_charge: canister_state_bits
                .time_of_last_allocation_charge_nanos
                .map_or(UNIX_EPOCH, |time_nanos| {
                    Time::from_nanos_since_unix_epoch(time_nanos)
                }),
        },
    };

    let metrics = LoadCanisterMetrics { durations };

    Ok((canister_state, metrics))
}

fn load_canister_state_from_checkpoint<P: ReadPolicy>(
    checkpoint_layout: &CheckpointLayout<P>,
    canister_id: &CanisterId,
) -> Result<(CanisterState, LoadCanisterMetrics), CheckpointError> {
    let canister_layout = checkpoint_layout.canister(canister_id)?;
    load_canister_state::<P>(&canister_layout, canister_id, checkpoint_layout.height())
}

fn load_bitcoin_state<P: ReadPolicy>(
    checkpoint_layout: &CheckpointLayout<P>,
) -> Result<BitcoinState, CheckpointError> {
    let layout = checkpoint_layout.bitcoin()?;
    let height = checkpoint_layout.height();

    let into_checkpoint_error =
        |field: String, err: ic_protobuf::proxy::ProxyDecodeError| CheckpointError::ProtoError {
            path: layout.raw_path(),
            field,
            proto_err: err.to_string(),
        };

    let bitcoin_state_proto = layout.bitcoin_state().deserialize_opt()?;

    let bitcoin_state_bits: BitcoinStateBits =
        BitcoinStateBits::try_from(bitcoin_state_proto.unwrap_or_default())
            .map_err(|err| into_checkpoint_error(String::from("BitcoinStateBits"), err))?;

    let utxos_small = load_or_create_pagemap(&layout.utxos_small(), height)?;
    let utxos_medium = load_or_create_pagemap(&layout.utxos_medium(), height)?;
    let address_outpoints = load_or_create_pagemap(&layout.address_outpoints(), height)?;

    Ok(BitcoinState {
        adapter_queues: bitcoin_state_bits.adapter_queues,
        unstable_blocks: bitcoin_state_bits.unstable_blocks,
        stable_height: bitcoin_state_bits.stable_height,
        utxo_set: UtxoSet {
            network: bitcoin_state_bits.network,
            utxos_small,
            utxos_medium,
            utxos_large: bitcoin_state_bits.utxos_large,
            address_outpoints,
        },
        fee_percentiles_cache: None,
    })
}

fn load_or_create_pagemap(path: &Path, height: Height) -> Result<PageMap, PersistenceError> {
    if path.exists() {
        PageMap::open(path, height)
    } else {
        Ok(PageMap::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BitcoinPageMap, NUMBER_OF_CHECKPOINT_THREADS};
    use ic_base_types::NumSeconds;
    use ic_ic00_types::CanisterStatusType;
    use ic_registry_subnet_type::SubnetType;
    use ic_replicated_state::{
        canister_state::execution_state::WasmBinary, canister_state::execution_state::WasmMetadata,
        page_map, testing::ReplicatedStateTesting, CallContextManager, CanisterStatus,
        ExecutionState, ExportedFunctions, NumWasmPages, PageIndex,
    };
    use ic_sys::PAGE_SIZE;
    use ic_test_utilities::{
        state::{canister_ids, new_canister_state},
        types::{
            ids::{canister_test_id, message_test_id, subnet_test_id, user_test_id},
            messages::IngressBuilder,
        },
    };
    use ic_test_utilities_logger::with_test_replica_logger;
    use ic_test_utilities_tmpdir::tmpdir;
    use ic_types::messages::StopCanisterContext;
    use ic_types::{CanisterId, Cycles, ExecutionRound, Height};
    use ic_wasm_types::CanisterModule;
    use std::collections::BTreeSet;

    const INITIAL_CYCLES: Cycles = Cycles::new(1 << 36);

    fn checkpoint_metrics() -> CheckpointMetrics {
        let metrics_registry = ic_metrics::MetricsRegistry::new();
        CheckpointMetrics::new(&metrics_registry)
    }

    fn thread_pool() -> scoped_threadpool::Pool {
        scoped_threadpool::Pool::new(NUMBER_OF_CHECKPOINT_THREADS)
    }

    fn empty_wasm() -> CanisterModule {
        CanisterModule::new(vec![
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x08, 0x04, 0x6e, 0x61, 0x6d,
            0x65, 0x02, 0x01, 0x00,
        ])
    }

    fn one_page_of(byte: u8) -> Memory {
        let contents = [byte; PAGE_SIZE];
        let delta = &[(PageIndex::from(0), &contents)];
        let mut page_map = PageMap::new();
        page_map.update(delta);
        Memory::new(page_map, NumWasmPages::from(1))
    }

    fn mark_readonly(path: &std::path::Path) -> std::io::Result<()> {
        let mut permissions = path.metadata()?.permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(path, permissions)
    }

    fn make_checkpoint_and_get_state(
        log: &ReplicaLogger,
        state: &ReplicatedState,
        height: Height,
        layout: &StateLayout,
    ) -> ReplicatedState {
        make_checkpoint(
            state,
            height,
            layout,
            log,
            &checkpoint_metrics(),
            &mut thread_pool(),
        )
        .unwrap_or_else(|err| panic!("Expected make_checkpoint to succeed, got {:?}", err))
    }

    #[test]
    fn can_make_a_checkpoint() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root.clone()).unwrap();

            const HEIGHT: Height = Height::new(42);
            let canister_id = canister_test_id(10);

            let mut state = ReplicatedState::new(subnet_test_id(1), SubnetType::Application);
            state.put_canister_state(new_canister_state(
                canister_id,
                user_test_id(24).get(),
                INITIAL_CYCLES,
                NumSeconds::from(100_000),
            ));

            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            // Ensure that checkpoint data is now available via layout API.
            assert_eq!(layout.checkpoint_heights().unwrap(), vec![HEIGHT]);
            let checkpoint = layout.checkpoint(HEIGHT).unwrap();
            assert_eq!(checkpoint.canister_ids().unwrap(), vec![canister_id]);
            assert!(checkpoint
                .canister(&canister_id)
                .unwrap()
                .queues()
                .deserialize()
                .is_ok());

            // Ensure the expected paths actually exist.
            let checkpoint_path = root.join("checkpoints").join("000000000000002a");
            let canister_path = checkpoint_path
                .join("canister_states")
                .join("000000000000000a0101");

            let expected_paths = vec![
                checkpoint_path.join("system_metadata.pbuf"),
                canister_path.join("queues.pbuf"),
                canister_path.join("canister.pbuf"),
            ];

            for path in expected_paths {
                assert!(path.exists(), "Expected path {} to exist", path.display());
                assert!(
                    path.metadata().unwrap().permissions().readonly(),
                    "Expected path {} to be readonly",
                    path.display()
                );
            }
        });
    }

    #[test]
    fn scratchpad_dir_is_deleted_if_checkpointing_failed() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let checkpoints_dir = root.join("checkpoints");
            let layout = StateLayout::try_new(log.clone(), root.clone()).unwrap();

            const HEIGHT: Height = Height::new(42);
            let canister_id = canister_test_id(10);
            let mut state = ReplicatedState::new(subnet_test_id(1), SubnetType::Application);
            state.put_canister_state(new_canister_state(
                canister_id,
                user_test_id(24).get(),
                INITIAL_CYCLES,
                NumSeconds::from(100_000),
            ));

            mark_readonly(&checkpoints_dir).unwrap();

            // Scratchpad directory is "tmp/scatchpad_{hex(height)}"
            let expected_scratchpad_dir = root.join("tmp").join("scratchpad_000000000000002a");

            let replicated_state = make_checkpoint(
                &state,
                HEIGHT,
                &layout,
                &log,
                &checkpoint_metrics(),
                &mut thread_pool(),
            );

            match replicated_state {
                Err(_) => assert!(
                    !expected_scratchpad_dir.exists(),
                    "Expected incomplete scratchpad to be deleted"
                ),
                Ok(_) => panic!("Expected checkpointing to fail"),
            }
        });
    }

    #[test]
    fn can_recover_from_a_checkpoint() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);
            let canister_id: CanisterId = canister_test_id(10);

            let wasm = empty_wasm();
            let wasm_memory = one_page_of(1);

            let mut canister_state = new_canister_state(
                canister_id,
                user_test_id(24).get(),
                INITIAL_CYCLES,
                NumSeconds::from(100_000),
            );
            let page_map = PageMap::from(&[1, 2, 3, 4][..]);
            let stable_memory = Memory::new(page_map, NumWasmPages::new(1));
            let execution_state = ExecutionState {
                canister_root: "NOT_USED".into(),
                session_nonce: None,
                wasm_binary: WasmBinary::new(wasm.clone()),
                wasm_memory: wasm_memory.clone(),
                stable_memory,
                exported_globals: vec![],
                exports: ExportedFunctions::new(BTreeSet::new()),
                metadata: WasmMetadata::default(),
                last_executed_round: ExecutionRound::from(0),
            };
            canister_state.execution_state = Some(execution_state);

            let own_subnet_type = SubnetType::Application;
            let mut state = ReplicatedState::new(subnet_test_id(1), own_subnet_type);
            state.put_canister_state(canister_state);
            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            let recovered_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();

            assert_eq!(canister_ids(&recovered_state), vec![canister_id]);

            let canister = recovered_state.canister_state(&canister_id).unwrap();
            assert_eq!(
                canister
                    .execution_state
                    .as_ref()
                    .unwrap()
                    .wasm_binary
                    .binary
                    .as_slice(),
                wasm.as_slice()
            );
            assert_eq!(
                canister.execution_state.as_ref().unwrap().wasm_memory,
                wasm_memory
            );
            assert_eq!(
                canister
                    .execution_state
                    .as_ref()
                    .unwrap()
                    .stable_memory
                    .size,
                NumWasmPages::new(1)
            );

            // Verify that the deserialized stable memory is correctly retrieved.
            let mut data = vec![0, 0, 0, 0];
            let buf = page_map::Buffer::new(
                canister
                    .execution_state
                    .as_ref()
                    .unwrap()
                    .stable_memory
                    .page_map
                    .clone(),
            );
            buf.read(&mut data[..], 0);
            assert_eq!(data, vec![1, 2, 3, 4]);
        });
    }

    #[test]
    fn can_recover_an_empty_state() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);
            let own_subnet_type = SubnetType::Application;

            let _state = make_checkpoint_and_get_state(
                &log,
                &ReplicatedState::new(subnet_test_id(1), own_subnet_type),
                HEIGHT,
                &layout,
            );

            let recovered_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();
            assert!(recovered_state.canisters_iter().next().is_none());
        });
    }

    #[test]
    fn returns_not_found_for_missing_checkpoints() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log, root).unwrap();

            const MISSING_HEIGHT: Height = Height::new(42);
            match layout
                .checkpoint(MISSING_HEIGHT)
                .map_err(CheckpointError::from)
                .and_then(|c| {
                    load_checkpoint(
                        &c,
                        SubnetType::Application,
                        &checkpoint_metrics(),
                        Some(&mut thread_pool()),
                    )
                }) {
                Err(CheckpointError::NotFound(_)) => (),
                Err(err) => panic!("Expected to get NotFound error, got {:?}", err),
                Ok(_) => panic!("Expected to get an error, got state!"),
            }
        });
    }

    #[test]
    fn reports_an_error_on_misconfiguration() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint_reports_an_error_on_misconfiguration");
            let root = tmp.path().to_path_buf();

            mark_readonly(&root).unwrap();

            let layout = StateLayout::try_new(log, root);

            assert!(layout.is_err());
            let err_msg = layout.err().unwrap().to_string();
            assert!(
                err_msg.contains("Permission denied"),
                "Expected a permission error, got {}",
                err_msg
            );
        });
    }

    #[test]
    fn can_recover_a_stopping_canister() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);
            let canister_id: CanisterId = canister_test_id(10);
            let controller = user_test_id(24).get();

            let mut canister_state = CanisterState {
                system_state: SystemState::new_stopping(
                    canister_id,
                    controller,
                    INITIAL_CYCLES,
                    NumSeconds::from(100_000),
                ),
                execution_state: None,
                scheduler_state: Default::default(),
            };

            let stop_context = StopCanisterContext::Ingress {
                sender: user_test_id(0),
                message_id: message_test_id(0),
            };
            canister_state
                .system_state
                .add_stop_context(stop_context.clone());

            let own_subnet_type = SubnetType::Application;
            let mut state = ReplicatedState::new(subnet_test_id(1), own_subnet_type);
            state.put_canister_state(canister_state);
            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            let recovered_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();

            assert_eq!(canister_ids(&recovered_state), vec![canister_id]);

            let canister = recovered_state.canister_state(&canister_id).unwrap();
            assert_eq!(
                canister.system_state.status,
                CanisterStatus::Stopping {
                    stop_contexts: vec![stop_context],
                    call_context_manager: CallContextManager::default(),
                }
            );
        });
    }

    #[test]
    fn can_recover_a_stopped_canister() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);
            let canister_id: CanisterId = canister_test_id(10);
            let controller = user_test_id(24).get();

            let canister_state = CanisterState {
                system_state: SystemState::new_stopped(
                    canister_id,
                    controller,
                    INITIAL_CYCLES,
                    NumSeconds::from(100_000),
                ),
                execution_state: None,
                scheduler_state: Default::default(),
            };

            let own_subnet_type = SubnetType::Application;
            let mut state = ReplicatedState::new(subnet_test_id(1), own_subnet_type);
            state.put_canister_state(canister_state);
            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            let loaded_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();

            assert_eq!(canister_ids(&loaded_state), vec![canister_id]);

            let canister = loaded_state.canister_state(&canister_id).unwrap();
            assert_eq!(canister.status(), CanisterStatusType::Stopped);
        });
    }

    #[test]
    fn can_recover_a_running_canister() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);
            let canister_id: CanisterId = canister_test_id(10);
            let controller = user_test_id(24).get();

            let canister_state = CanisterState {
                system_state: SystemState::new_running(
                    canister_id,
                    controller,
                    INITIAL_CYCLES,
                    NumSeconds::from(100_000),
                ),
                execution_state: None,
                scheduler_state: Default::default(),
            };

            let own_subnet_type = SubnetType::Application;
            let mut state = ReplicatedState::new(subnet_test_id(1), own_subnet_type);
            state.put_canister_state(canister_state);
            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            let recovered_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();

            assert_eq!(canister_ids(&recovered_state), vec![canister_id]);

            let canister = recovered_state.canister_state(&canister_id).unwrap();
            assert_eq!(canister.status(), CanisterStatusType::Running)
        });
    }

    #[test]
    fn can_recover_subnet_queues() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);

            let own_subnet_type = SubnetType::Application;
            let subnet_id = subnet_test_id(1);
            let subnet_id_as_canister_id = CanisterId::from(subnet_id);
            let mut state = ReplicatedState::new(subnet_id, own_subnet_type);

            // Add an ingress message to the subnet queues to later verify
            // it gets recovered.
            state.subnet_queues_mut().push_ingress(
                IngressBuilder::new()
                    .receiver(subnet_id_as_canister_id)
                    .build(),
            );

            let original_state = state.clone();
            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            let recovered_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();

            assert_eq!(
                original_state.subnet_queues(),
                recovered_state.subnet_queues()
            );
        });
    }

    #[test]
    fn can_recover_bitcoin_state() {
        use ic_btc_types::Network as BitcoinNetwork;
        use ic_btc_types_internal::{BitcoinAdapterRequestWrapper, GetSuccessorsRequest};
        use ic_registry_subnet_features::{BitcoinFeature, BitcoinFeatureStatus};

        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);

            let own_subnet_type = SubnetType::Application;
            let subnet_id = subnet_test_id(1);
            let mut state = ReplicatedState::new(subnet_id, own_subnet_type);

            // Enable the bitcoin feature to be able to mutate its state.
            state.metadata.own_subnet_features.bitcoin = Some(BitcoinFeature {
                network: BitcoinNetwork::Testnet,
                status: BitcoinFeatureStatus::Enabled,
            });

            // Make some change in the Bitcoin state to later verify that it gets recovered.
            state
                .push_request_bitcoin(BitcoinAdapterRequestWrapper::GetSuccessorsRequest(
                    GetSuccessorsRequest {
                        processed_block_hashes: vec![],
                        anchor: vec![],
                    },
                ))
                .unwrap();

            let original_state = state.clone();
            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            let recovered_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();

            assert_eq!(recovered_state.bitcoin(), original_state.bitcoin(),);
        });
    }

    #[test]
    fn can_recover_bitcoin_page_maps() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let layout = StateLayout::try_new(log.clone(), root).unwrap();

            const HEIGHT: Height = Height::new(42);

            let own_subnet_type = SubnetType::Application;
            let subnet_id = subnet_test_id(1);
            let mut state = ReplicatedState::new(subnet_id, own_subnet_type);

            // Make some change in the Bitcoin page maps to later verify they get recovered.
            state.bitcoin_mut().utxo_set.utxos_small = PageMap::from(&[1, 2, 3, 4][..]);
            state.bitcoin_mut().utxo_set.utxos_medium = PageMap::from(&[5, 6, 7, 8][..]);
            state.bitcoin_mut().utxo_set.address_outpoints = PageMap::from(&[9, 10, 11, 12][..]);

            let original_state = state.clone();
            let _state = make_checkpoint_and_get_state(&log, &state, HEIGHT, &layout);

            let recovered_state = load_checkpoint(
                &layout.checkpoint(HEIGHT).unwrap(),
                own_subnet_type,
                &checkpoint_metrics(),
                Some(&mut thread_pool()),
            )
            .unwrap();

            assert_eq!(recovered_state.bitcoin(), original_state.bitcoin());
        });
    }

    #[test]
    fn defrag_is_safe() {
        with_test_replica_logger(|log| {
            let tmp = tmpdir("checkpoint");
            let root = tmp.path().to_path_buf();
            let tip = StateLayout::try_new(log, root)
                .unwrap()
                .tip(Height::new(42))
                .unwrap();

            let defrag_size = 1 << 20; // 1MB

            let page_maps: Vec<PageMapType> = vec![
                PageMapType::Bitcoin(BitcoinPageMap::AddressOutpoints),
                PageMapType::Bitcoin(BitcoinPageMap::UtxosSmall),
                PageMapType::Bitcoin(BitcoinPageMap::UtxosMedium),
                PageMapType::StableMemory(canister_test_id(100)),
                PageMapType::WasmMemory(canister_test_id(100)),
            ];

            let paths: Vec<PathBuf> = page_maps
                .iter()
                .map(|page_map_type| page_map_type.path(&tip).unwrap())
                .collect();

            for path in &paths {
                assert!(!path.exists());
            }

            defrag_tip(&tip, &page_maps, defrag_size, 100, 0).unwrap();

            for path in &paths {
                assert!(!path.exists());
            }

            for factor in 1..3 {
                let short_data: Vec<u8> = vec![42; (defrag_size / factor) as usize];
                let long_data: Vec<u8> = vec![43; (defrag_size * factor) as usize];
                let empty: &[u8] = &[];

                std::fs::write(&paths[0], &short_data).unwrap();
                std::fs::write(&paths[1], &long_data).unwrap();
                // third file is an empty file
                std::fs::write(&paths[2], empty).unwrap();

                let check_files = || {
                    assert_eq!(std::fs::read(&paths[0]).unwrap(), short_data);
                    assert_eq!(std::fs::read(&paths[1]).unwrap(), long_data);
                    assert!(paths[2].exists());
                    assert_eq!(std::fs::read(&paths[2]).unwrap(), empty);
                };

                check_files();

                for i in 0..100 {
                    defrag_tip(&tip, &page_maps, defrag_size, i as usize, i).unwrap();
                    check_files();
                }
            }
        });
    }
}
