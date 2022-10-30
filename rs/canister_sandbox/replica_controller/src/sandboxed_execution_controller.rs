use ic_canister_sandbox_common::controller_launcher_service::ControllerLauncherService;
use ic_canister_sandbox_common::launcher_service::LauncherService;
use ic_canister_sandbox_common::protocol::id::{ExecId, MemoryId, WasmId};
use ic_canister_sandbox_common::protocol::sbxsvc::MemorySerialization;
use ic_canister_sandbox_common::protocol::structs::{SandboxExecInput, SandboxExecOutput};
use ic_canister_sandbox_common::sandbox_service::SandboxService;
use ic_canister_sandbox_common::{protocol, rpc};
use ic_config::embedders::Config as EmbeddersConfig;
use ic_embedders::wasm_executor::{
    get_wasm_reserved_pages, wasm_execution_error, CanisterStateChanges, PausedWasmExecution,
    WasmExecutionResult, WasmExecutor,
};
use ic_embedders::{
    wasm_utils::WasmImportsDetails, CompilationCache, CompilationResult, WasmExecutionInput,
};
use ic_interfaces::execution_environment::{HypervisorError, HypervisorResult};
#[cfg(target_os = "linux")]
use ic_logger::warn;
use ic_logger::{error, ReplicaLogger};
use ic_metrics::buckets::decimal_buckets_with_zero;
use ic_metrics::MetricsRegistry;
use ic_replicated_state::canister_state::execution_state::{
    SandboxMemory, SandboxMemoryHandle, SandboxMemoryOwner, WasmBinary,
};
use ic_replicated_state::{EmbedderCache, ExecutionState, ExportedFunctions, Memory, PageMap};
use ic_types::{CanisterId, NumInstructions};
use ic_wasm_types::CanisterModule;
#[cfg(target_os = "linux")]
use prometheus::IntGauge;
use prometheus::{Histogram, HistogramVec, IntCounter, IntCounterVec};
use std::collections::{HashMap, VecDeque};
#[cfg(target_os = "linux")]
use std::convert::TryInto;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::Weak;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::active_execution_state_registry::{ActiveExecutionStateRegistry, CompletionResult};
use crate::controller_service_impl::ControllerServiceImpl;
use crate::launch_as_process::{create_sandbox_process, spawn_launcher_process};
use crate::process_exe_and_args::{create_launcher_argv, create_sandbox_argv};
#[cfg(target_os = "linux")]
use crate::process_os_metrics;

const SANDBOX_PROCESS_INACTIVE_TIME_BEFORE_EVICTION: Duration = Duration::from_secs(60);
const SANDBOX_PROCESS_UPDATE_INTERVAL: Duration = Duration::from_secs(10);

const SANDBOXED_EXECUTION_INVALID_MEMORY_SIZE: &str = "sandboxed_execution_invalid_memory_size";

// Metric labels for the different outcomes of a wasm cache lookup. Stored in
// the metric
// [`SandboxedExecutionMetrics::sandboxed_execution_replica_cache_lookups`].
const EMBEDDER_CACHE_HIT_SUCCESS: &str = "embedder_cache_hit_success";
const EMBEDDER_CACHE_HIT_SANDBOX_EVICTED: &str = "embedder_cache_hit_sandbox_evicted";
const EMBEDDER_CACHE_HIT_COMPILATION_ERROR: &str = "embedder_cache_hit_compilation_error";
const COMPILATION_CACHE_HIT: &str = "compilation_cache_hit";
const COMPILATION_CACHE_HIT_COMPILATION_ERROR: &str = "compilation_cache_hit_compilation_error";
const CACHE_MISS: &str = "cache_miss";

struct SandboxedExecutionMetrics {
    sandboxed_execution_replica_execute_duration: HistogramVec,
    sandboxed_execution_replica_execute_prepare_duration: HistogramVec,
    sandboxed_execution_replica_execute_wait_duration: HistogramVec,
    sandboxed_execution_replica_execute_finish_duration: HistogramVec,
    sandboxed_execution_sandbox_execute_duration: HistogramVec,
    sandboxed_execution_sandbox_execute_run_duration: HistogramVec,
    sandboxed_execution_spawn_process: Histogram,
    #[cfg(target_os = "linux")]
    sandboxed_execution_subprocess_anon_rss_total: IntGauge,
    #[cfg(target_os = "linux")]
    sandboxed_execution_subprocess_memfd_rss_total: IntGauge,
    #[cfg(target_os = "linux")]
    sandboxed_execution_subprocess_anon_rss: Histogram,
    #[cfg(target_os = "linux")]
    sandboxed_execution_subprocess_memfd_rss: Histogram,
    #[cfg(target_os = "linux")]
    sandboxed_execution_subprocess_rss: Histogram,
    sandboxed_execution_subprocess_active_last_used: Histogram,
    sandboxed_execution_subprocess_evicted_last_used: Histogram,
    sandboxed_execution_critical_error_invalid_memory_size: IntCounter,
    sandboxed_execution_replica_create_exe_state_duration: Histogram,
    sandboxed_execution_replica_create_exe_state_wait_compile_duration: Histogram,
    sandboxed_execution_replica_create_exe_state_wait_deserialize_duration: Histogram,
    sandboxed_execution_replica_create_exe_state_finish_duration: Histogram,
    sandboxed_execution_sandbox_create_exe_state_deserialize_duration: Histogram,
    sandboxed_execution_sandbox_create_exe_state_deserialize_total_duration: Histogram,
    sandboxed_execution_replica_cache_lookups: IntCounterVec,
    // TODO(EXC-365): Remove these metrics once we confirm that no module imports these IC0 methods
    // anymore.
    sandboxed_execution_wasm_imports_call_simple: IntCounter,
    sandboxed_execution_wasm_imports_controller_size: IntCounter,
    sandboxed_execution_wasm_imports_controller_copy: IntCounter,
    // TODO(EXC-376): Remove these metrics once we confirm that no module imports these IC0 methods
    // anymore.
    sandboxed_execution_wasm_imports_call_cycles_add: IntCounter,
    sandboxed_execution_wasm_imports_canister_cycle_balance: IntCounter,
    sandboxed_execution_wasm_imports_msg_cycles_available: IntCounter,
    sandboxed_execution_wasm_imports_msg_cycles_refunded: IntCounter,
    sandboxed_execution_wasm_imports_msg_cycles_accept: IntCounter,
    sandboxed_execution_wasm_imports_mint_cycles: IntCounter,
}

impl SandboxedExecutionMetrics {
    fn new(metrics_registry: &MetricsRegistry) -> Self {
        Self {
            sandboxed_execution_replica_execute_duration: metrics_registry.histogram_vec(
                "sandboxed_execution_replica_execute_duration_seconds",
                "The total message execution duration in the replica controller",
                decimal_buckets_with_zero(-4, 1),
                &["api_type"],
            ),
            sandboxed_execution_replica_execute_prepare_duration: metrics_registry.histogram_vec(
                "sandboxed_execution_replica_execute_prepare_duration_seconds",
                "The time until sending an execution request to the sandbox process",
                decimal_buckets_with_zero(-4, 1),
                &["api_type"],
            ),
            sandboxed_execution_replica_execute_wait_duration: metrics_registry.histogram_vec(
                "sandboxed_execution_replica_execute_wait_duration_seconds",
                "The time from sending an execution request to receiving response",
                decimal_buckets_with_zero(-4, 1),
                &["api_type"],
            ),
            sandboxed_execution_replica_execute_finish_duration: metrics_registry.histogram_vec(
                "sandboxed_execution_replica_execute_finish_duration_seconds",
                "The time to finalize execution in the replica controller",
                decimal_buckets_with_zero(-4, 1),
                &["api_type"],
            ),
            sandboxed_execution_sandbox_execute_duration: metrics_registry.histogram_vec(
                "sandboxed_execution_sandbox_execute_duration_seconds",
                "The time from receiving an execution request to finishing execution",
                decimal_buckets_with_zero(-4, 1),
                &["api_type"],
            ),

            sandboxed_execution_sandbox_execute_run_duration: metrics_registry.histogram_vec(
                "sandboxed_execution_sandbox_execute_run_duration_seconds",
                "The time spent in the sandbox's worker thread responsible for actually performing the executions",
                decimal_buckets_with_zero(-4, 1),
                &["api_type"],
            ),
            sandboxed_execution_spawn_process: metrics_registry.histogram(
                "sandboxed_execution_spawn_process_duration_seconds",
                "The time to spawn a sandbox process",
                decimal_buckets_with_zero(-4, 1),
            ),
            #[cfg(target_os = "linux")]
            sandboxed_execution_subprocess_anon_rss_total: metrics_registry.int_gauge(
                "sandboxed_execution_subprocess_anon_rss_total_kib",
                "The resident anonymous memory for all canister sandbox processes in KiB",
            ),
            #[cfg(target_os = "linux")]
            sandboxed_execution_subprocess_memfd_rss_total: metrics_registry.int_gauge(
                "sandboxed_execution_subprocess_memfd_rss_total_kib",
                "The resident shared memory for all canister sandbox processes in KiB"
            ),
            #[cfg(target_os = "linux")]
            sandboxed_execution_subprocess_anon_rss: metrics_registry.histogram(
                "sandboxed_execution_subprocess_anon_rss_kib",
                "The resident anonymous memory for a canister sandbox process in KiB",
                decimal_buckets_with_zero(1, 7), // 10KiB - 50GiB.
            ),
            #[cfg(target_os = "linux")]
            sandboxed_execution_subprocess_memfd_rss: metrics_registry.histogram(
                "sandboxed_execution_subprocess_memfd_rss_kib",
                "The resident shared memory for a canister sandbox process in KiB",
                decimal_buckets_with_zero(1, 7), // 10KiB - 50GiB.
            ),
            #[cfg(target_os = "linux")]
            sandboxed_execution_subprocess_rss: metrics_registry.histogram(
                "sandboxed_execution_subprocess_rss_kib",
                "The resident memory of a canister sandbox process in KiB",
                decimal_buckets_with_zero(1, 7), // 10KiB - 50GiB.
            ),
            sandboxed_execution_subprocess_active_last_used: metrics_registry.histogram(
                "sandboxed_execution_subprocess_active_last_used_duration_seconds",
                "Time since the last usage of an active sandbox process in seconds",
                decimal_buckets_with_zero(-1, 4), // 0.1s - 13h.
            ),
            sandboxed_execution_subprocess_evicted_last_used: metrics_registry.histogram(
                "sandboxed_execution_subprocess_evicted_last_used_duration_seconds",
                "Time since the last usage of an evicted sandbox process in seconds",
                decimal_buckets_with_zero(-1, 4), // 0.1s - 13h.
            ),
            sandboxed_execution_critical_error_invalid_memory_size: metrics_registry.error_counter(
                SANDBOXED_EXECUTION_INVALID_MEMORY_SIZE),
            sandboxed_execution_replica_create_exe_state_duration: metrics_registry.histogram(
                "sandboxed_execution_replica_create_exe_state_duration_seconds",
                "The total create execution state duration in the replica controller",
                decimal_buckets_with_zero(-4, 1),
            ),
            sandboxed_execution_replica_create_exe_state_wait_compile_duration: metrics_registry.histogram(
                "sandboxed_execution_replica_create_exe_state_wait_compile_duration_seconds",
                "Time taken to send a create execution state request and get a response when compiling",
                decimal_buckets_with_zero(-4, 1),
            ),
            sandboxed_execution_replica_create_exe_state_wait_deserialize_duration: metrics_registry.histogram(
                "sandboxed_execution_replica_create_exe_state_wait_deserialize_duration_seconds",
                "Time taken to send a create execution state request and get a response when deserializing",
                decimal_buckets_with_zero(-4, 1),
            ),
            sandboxed_execution_replica_create_exe_state_finish_duration: metrics_registry.histogram(
                "sandboxed_execution_replica_create_exe_finish_duration_seconds",
                "Time to create an execution state after getting the response from the sandbox",
                decimal_buckets_with_zero(-4, 1),
            ),
            sandboxed_execution_sandbox_create_exe_state_deserialize_duration: metrics_registry.histogram(
                "sandboxed_execution_sandbox_create_exe_state_deserialize_duration_seconds",
                "Time taken to deserialize a wasm module when creating the execution state from a serialized module",
                decimal_buckets_with_zero(-4, 1),
            ),
            sandboxed_execution_sandbox_create_exe_state_deserialize_total_duration: metrics_registry.histogram(
                "sandboxed_execution_sandbox_create_exe_state_deserialize_total_duration_seconds",
                "Total time spent in the sandbox when creating an execution state from a serialized module",
                decimal_buckets_with_zero(-4, 1),
            ),
            sandboxed_execution_replica_cache_lookups: metrics_registry.int_counter_vec(
                "sandboxed_execution_replica_cache_lookups", 
                "Results from looking up a wasm module in the embedder cache or compilation cache", 
                &["lookup_result"]),
            sandboxed_execution_wasm_imports_call_simple: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_call_simple_total",
                "The number of Wasm modules that import ic0.call_simple",
            ),
            sandboxed_execution_wasm_imports_controller_size: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_controller_size_total",
                "The number of Wasm modules that import ic0.controller_size",
            ),
            sandboxed_execution_wasm_imports_controller_copy: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_controller_copy_total",
                "The number of Wasm modules that import ic0.controller_copy",
            ),
            sandboxed_execution_wasm_imports_call_cycles_add: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_call_cycles_add",
                "The number of Wasm modules that import ic0.call_cycles_add",
            ),
            sandboxed_execution_wasm_imports_canister_cycle_balance: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_canister_cycle_balance",
                "The number of Wasm modules that import ic0.canister_cycle_balance",
            ),
            sandboxed_execution_wasm_imports_msg_cycles_available: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_msg_cycles_available",
                "The number of Wasm modules that import ic0.msg_cycles_available",
            ),
            sandboxed_execution_wasm_imports_msg_cycles_refunded: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_msg_cycles_refunded",
                "The number of Wasm modules that import ic0.msg_cycles_refunded",
            ),
            sandboxed_execution_wasm_imports_msg_cycles_accept: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_msg_cycles_accept",
                "The number of Wasm modules that import ic0.msg_cycles_accept",
            ),
            sandboxed_execution_wasm_imports_mint_cycles: metrics_registry.int_counter(
                "sandboxed_execution_wasm_imports_mint_cycles",
                "The number of Wasm modules that import ic0.mint_cycles",
            ),
        }
    }

    fn inc_cache_lookup(&self, label: &str) {
        self.sandboxed_execution_replica_cache_lookups
            .with_label_values(&[label])
            .inc();
    }
}

/// Keeps history of the N most recent calls made to the sandbox backend
/// process. It will normally not be logged, but in case of an
/// unexpected sandbox process crash we can replay and log the history
/// to get a better idea of what led to this situation.
/// This is purely a debugging aid. Nothing functionally depends on it.
struct SandboxProcessRequestHistory {
    entries: Mutex<VecDeque<String>>,
    limit: usize,
}

impl SandboxProcessRequestHistory {
    fn new() -> Self {
        Self {
            entries: Default::default(),
            limit: 20,
        }
    }

    /// Records an entry of an action performed on a sandbox process.
    fn record(&self, msg: String) {
        let mut guard = self.entries.lock().unwrap();
        guard.push_back(msg);
        if guard.len() > self.limit {
            guard.pop_front();
        }
    }

    /// Replays the last actions recorded for this sandbox process to
    /// the given logger.
    fn replay(&self, logger: &ReplicaLogger, canister_id: CanisterId, pid: u32) {
        let guard = self.entries.lock().unwrap();
        for entry in &*guard {
            error!(
                logger,
                "History for canister {} with pid {}: {}", canister_id, pid, entry
            );
        }
    }
}

pub struct SandboxProcess {
    /// Registry for all executions that are currently running on
    /// this backend process.
    execution_states: Arc<ActiveExecutionStateRegistry>,

    /// Handle for IPC down to sandbox.
    sandbox_service: Arc<dyn SandboxService>,

    /// Process id of the backend process.
    pid: u32,

    /// History of operations sent to sandbox process (for crash
    /// diagnostics).
    history: SandboxProcessRequestHistory,
}

impl Drop for SandboxProcess {
    fn drop(&mut self) {
        self.history.record("Terminate()".to_string());
        self.sandbox_service
            .terminate(protocol::sbxsvc::TerminateRequest {})
            .on_completion(|_| {});
    }
}

/// Manages the lifetime of a remote compiled Wasm and provides its id.
///
/// It keeps a weak reference to the sandbox service to allow early
/// termination of the sandbox process when it becomes inactive.
pub struct OpenedWasm {
    sandbox_process: Weak<SandboxProcess>,
    wasm_id: WasmId,
}

impl OpenedWasm {
    fn new(sandbox_process: Weak<SandboxProcess>, wasm_id: WasmId) -> Self {
        Self {
            sandbox_process,
            wasm_id,
        }
    }
}

impl Drop for OpenedWasm {
    fn drop(&mut self) {
        if let Some(sandbox_process) = self.sandbox_process.upgrade() {
            sandbox_process
                .history
                .record(format!("CloseWasm(wasm_id={})", self.wasm_id));
            sandbox_process
                .sandbox_service
                .close_wasm(protocol::sbxsvc::CloseWasmRequest {
                    wasm_id: self.wasm_id,
                })
                .on_completion(|_| {});
        }
    }
}

impl std::fmt::Debug for OpenedWasm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedWasm")
            .field("wasm_id", &self.wasm_id)
            .finish()
    }
}

/// Manages the lifetime of a remote sandbox memory and provides its id.
pub struct OpenedMemory {
    sandbox_process: Arc<SandboxProcess>,
    memory_id: MemoryId,
}

impl OpenedMemory {
    fn new(sandbox_process: Arc<SandboxProcess>, memory_id: MemoryId) -> Self {
        Self {
            sandbox_process,
            memory_id,
        }
    }
}

impl SandboxMemoryOwner for OpenedMemory {
    fn get_id(&self) -> usize {
        self.memory_id.as_usize()
    }
}

impl Drop for OpenedMemory {
    fn drop(&mut self) {
        self.sandbox_process
            .history
            .record(format!("CloseMemory(memory_id={})", self.memory_id));
        self.sandbox_process
            .sandbox_service
            .close_memory(protocol::sbxsvc::CloseMemoryRequest {
                memory_id: self.memory_id,
            })
            .on_completion(|_| {});
    }
}

impl std::fmt::Debug for OpenedMemory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenedMemory")
            .field("memory_id", &self.memory_id)
            .finish()
    }
}

enum Backend {
    Active {
        sandbox_process: Arc<SandboxProcess>,
        last_used: std::time::Instant,
    },
    Evicted {
        sandbox_process: Weak<SandboxProcess>,
        last_used: std::time::Instant,
    },
    Empty,
}

enum SandboxProcessStatus {
    Active,
    Evicted,
}

struct SandboxProcessStats {
    time_since_last_usage: std::time::Duration,
    status: SandboxProcessStatus,
}

// Represent a paused sandbox execution.
struct PausedSandboxExecution {
    canister_id: CanisterId,
    sandbox_process: Arc<SandboxProcess>,
    exec_id: ExecId,
    next_wasm_memory_id: MemoryId,
    next_stable_memory_id: MemoryId,
    message_instruction_limit: NumInstructions,
    api_type_label: &'static str,
    controller: Arc<SandboxedExecutionController>,
}

impl std::fmt::Debug for PausedSandboxExecution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PausedSandboxExecution")
            .field("canister_id", &self.canister_id)
            .field("exec_id", &self.exec_id)
            .field("api_type_label", &self.api_type_label)
            .finish()
    }
}

impl PausedWasmExecution for PausedSandboxExecution {
    fn resume(self: Box<Self>, execution_state: &ExecutionState) -> WasmExecutionResult {
        // Create channel through which we will receive the execution
        // output from closure (running by IPC thread at end of
        // execution).
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let sandbox_process = Arc::clone(&self.sandbox_process);
        self.sandbox_process
            .execution_states
            .register_execution_with_id(self.exec_id, move |exec_id, result| {
                sandbox_process
                    .history
                    .record(format!("Completion(exec_id={})", exec_id));
                tx.send(result).unwrap();
            });

        self.sandbox_process
            .history
            .record(format!("ResumeExecution(exec_id={}", self.exec_id,));
        self.sandbox_process
            .sandbox_service
            .resume_execution(protocol::sbxsvc::ResumeExecutionRequest {
                exec_id: self.exec_id,
            })
            .on_completion(|_| {});
        // Wait for completion.
        let result = rx.recv().unwrap();
        SandboxedExecutionController::process_completion(
            self.controller,
            self.exec_id,
            self.canister_id,
            execution_state,
            result,
            self.next_wasm_memory_id,
            self.next_stable_memory_id,
            self.message_instruction_limit,
            self.api_type_label,
            self.sandbox_process,
        )
    }

    fn abort(self: Box<Self>) {
        self.sandbox_process
            .history
            .record(format!("AbortExecution(exec_id={}", self.exec_id,));
        self.sandbox_process
            .sandbox_service
            .abort_execution(protocol::sbxsvc::AbortExecutionRequest {
                exec_id: self.exec_id,
            })
            .on_completion(|_| {});
    }
}

/// Manages sandboxed processes, forwards requests to the appropriate
/// process.
pub struct SandboxedExecutionController {
    backends: Arc<Mutex<HashMap<CanisterId, Backend>>>,
    logger: ReplicaLogger,
    /// Executable and arguments to be passed to `canister_sandbox` which are
    /// the same for all canisters.
    sandbox_exec_argv: Vec<String>,
    metrics: Arc<SandboxedExecutionMetrics>,
    launcher_service: Box<dyn LauncherService>,
}

impl WasmExecutor for SandboxedExecutionController {
    fn execute(
        self: Arc<Self>,
        WasmExecutionInput {
            api_type,
            sandbox_safe_system_state,
            canister_current_memory_usage,
            execution_parameters,
            subnet_available_memory,
            func_ref,
            compilation_cache,
        }: WasmExecutionInput,
        execution_state: &ExecutionState,
    ) -> (Option<CompilationResult>, WasmExecutionResult) {
        let message_instruction_limit = execution_parameters.instruction_limits.message();
        let api_type_label = api_type.as_str();
        let _execute_timer = self
            .metrics
            .sandboxed_execution_replica_execute_duration
            .with_label_values(&[api_type_label])
            .start_timer();
        let prepare_timer = self
            .metrics
            .sandboxed_execution_replica_execute_prepare_duration
            .with_label_values(&[api_type_label])
            .start_timer();

        // Determine which process we want to run this on.
        let sandbox_process = self.get_sandbox_process(sandbox_safe_system_state.canister_id());

        // Ensure that Wasm is compiled.
        let (wasm_id, compilation_result) = match open_wasm(
            &sandbox_process,
            &*execution_state.wasm_binary,
            compilation_cache,
            &self.metrics,
        ) {
            Ok((wasm_id, compilation_result)) => (wasm_id, compilation_result),
            Err(err) => {
                return (None, wasm_execution_error(err, message_instruction_limit));
            }
        };

        // Create channel through which we will receive the execution
        // output from closure (running by IPC thread at end of
        // execution).
        let (tx, rx) = std::sync::mpsc::sync_channel(1);

        // Generate an ID for this execution, register it. We need to
        // pass the system state accessor as well as the completion
        // function that gets our result back in the end.
        let sandbox_process_weakref = Arc::downgrade(&sandbox_process);
        let exec_id =
            sandbox_process
                .execution_states
                .register_execution(move |exec_id, result| {
                    if let Some(sandbox_process) = sandbox_process_weakref.upgrade() {
                        sandbox_process
                            .history
                            .record(format!("Completion(exec_id={})", exec_id));
                    }
                    tx.send(result).unwrap();
                });

        // Now set up resources on the sandbox to drive the execution.
        let wasm_memory_handle = open_remote_memory(&sandbox_process, &execution_state.wasm_memory);
        let canister_id = sandbox_safe_system_state.canister_id();
        let wasm_memory_id = MemoryId::from(wasm_memory_handle.get_id());
        let next_wasm_memory_id = MemoryId::new();

        let stable_memory_handle =
            open_remote_memory(&sandbox_process, &execution_state.stable_memory);
        let stable_memory_id = MemoryId::from(stable_memory_handle.get_id());
        let next_stable_memory_id = MemoryId::new();

        sandbox_process.history.record(
            format!("StartExecution(exec_id={} wasm_id={} wasm_memory_id={} stable_member_id={} api_type={}, next_wasm_memory_id={} next_stable_memory_id={}",
                exec_id, wasm_id, wasm_memory_id, stable_memory_id, api_type.as_str(), next_wasm_memory_id, next_stable_memory_id));
        sandbox_process
            .sandbox_service
            .start_execution(protocol::sbxsvc::StartExecutionRequest {
                exec_id,
                wasm_id,
                wasm_memory_id,
                stable_memory_id,
                exec_input: SandboxExecInput {
                    func_ref,
                    api_type,
                    globals: execution_state.exported_globals.clone(),
                    canister_current_memory_usage,
                    execution_parameters,
                    subnet_available_memory,
                    next_wasm_memory_id,
                    next_stable_memory_id,
                    sandox_safe_system_state: sandbox_safe_system_state,
                    wasm_reserved_pages: get_wasm_reserved_pages(execution_state),
                },
            })
            .on_completion(|_| {});
        drop(prepare_timer);

        let wait_timer = self
            .metrics
            .sandboxed_execution_replica_execute_wait_duration
            .with_label_values(&[api_type_label])
            .start_timer();
        // Wait for completion.
        let result = rx.recv().unwrap();
        drop(wait_timer);
        let _finish_timer = self
            .metrics
            .sandboxed_execution_replica_execute_finish_duration
            .with_label_values(&[api_type_label])
            .start_timer();
        let execution_result = Self::process_completion(
            self,
            exec_id,
            canister_id,
            execution_state,
            result,
            next_wasm_memory_id,
            next_stable_memory_id,
            message_instruction_limit,
            api_type_label,
            sandbox_process,
        );
        (compilation_result, execution_result)
    }

    fn create_execution_state(
        &self,
        canister_module: CanisterModule,
        canister_root: PathBuf,
        canister_id: CanisterId,
        compilation_cache: Arc<CompilationCache>,
    ) -> HypervisorResult<(ExecutionState, NumInstructions, Option<CompilationResult>)> {
        let _create_exe_state_timer = self
            .metrics
            .sandboxed_execution_replica_create_exe_state_duration
            .start_timer();
        let sandbox_process = self.get_sandbox_process(canister_id);
        let wasm_binary = WasmBinary::new(canister_module);

        // Steps 1, 2, 3, 4 are performed by the sandbox process.
        let wasm_id = WasmId::new();
        let wasm_page_map = PageMap::default();
        let next_wasm_memory_id = MemoryId::new();

        let (memory_modifications, exported_globals, serialized_module, compilation_result) =
            match compilation_cache.get(&wasm_binary.binary) {
                None => {
                    self.metrics.inc_cache_lookup(CACHE_MISS);
                    let _compilation_timer = self
                        .metrics
                        .sandboxed_execution_replica_create_exe_state_wait_compile_duration
                        .start_timer();
                    sandbox_process.history.record(format!(
                        "CreateExecutionState(wasm_id={}, next_wasm_memory_id={})",
                        wasm_id, next_wasm_memory_id
                    ));
                    let reply = sandbox_process
                        .sandbox_service
                        .create_execution_state(protocol::sbxsvc::CreateExecutionStateRequest {
                            wasm_id,
                            wasm_binary: wasm_binary.binary.as_slice().to_vec(),
                            wasm_page_map: wasm_page_map.serialize(),
                            next_wasm_memory_id,
                            canister_id,
                        })
                        .sync()
                        .unwrap()
                        .0;
                    match reply {
                        Err(err) => {
                            compilation_cache.insert(&wasm_binary.binary, Err(err.clone()));
                            return Err(err);
                        }
                        Ok(reply) => {
                            let serialized_module = Arc::new(reply.serialized_module);
                            compilation_cache
                                .insert(&wasm_binary.binary, Ok(Arc::clone(&serialized_module)));
                            (
                                reply.wasm_memory_modifications,
                                reply.exported_globals,
                                serialized_module,
                                Some(reply.compilation_result),
                            )
                        }
                    }
                }
                Some(Err(err)) => {
                    self.metrics
                        .inc_cache_lookup(COMPILATION_CACHE_HIT_COMPILATION_ERROR);
                    return Err(err);
                }
                Some(Ok(serialized_module)) => {
                    self.metrics.inc_cache_lookup(COMPILATION_CACHE_HIT);
                    let _deserialization_timer = self
                        .metrics
                        .sandboxed_execution_replica_create_exe_state_wait_deserialize_duration
                        .start_timer();
                    sandbox_process.history.record(format!(
                        "CreateExecutionStateSerialized(wasm_id={}, next_wasm_memory_id={})",
                        wasm_id, next_wasm_memory_id
                    ));
                    let sandbox_result = sandbox_process
                        .sandbox_service
                        .create_execution_state_serialized(
                            protocol::sbxsvc::CreateExecutionStateSerializedRequest {
                                wasm_id,
                                serialized_module: Arc::clone(&serialized_module),
                                wasm_page_map: wasm_page_map.serialize(),
                                next_wasm_memory_id,
                                canister_id,
                            },
                        )
                        .sync()
                        .unwrap()
                        .0?;
                    self.metrics
                        .sandboxed_execution_sandbox_create_exe_state_deserialize_total_duration
                        .observe(sandbox_result.total_sandbox_time.as_secs_f64());
                    self.metrics
                        .sandboxed_execution_sandbox_create_exe_state_deserialize_duration
                        .observe(sandbox_result.deserialization_time.as_secs_f64());
                    (
                        sandbox_result.wasm_memory_modifications,
                        sandbox_result.exported_globals,
                        serialized_module,
                        None,
                    )
                }
            };
        let _finish_timer = self
            .metrics
            .sandboxed_execution_replica_create_exe_state_finish_duration
            .start_timer();
        observe_metrics(&self.metrics, &serialized_module.imports_details);

        cache_opened_wasm(
            &mut *wasm_binary.embedder_cache.lock().unwrap(),
            &sandbox_process,
            wasm_id,
        );

        // Step 5. Create the execution state.
        let mut wasm_memory = Memory::new(wasm_page_map, memory_modifications.size);
        wasm_memory
            .page_map
            .deserialize_delta(memory_modifications.page_delta);
        wasm_memory.sandbox_memory =
            SandboxMemory::synced(wrap_remote_memory(&sandbox_process, next_wasm_memory_id));
        if let Err(err) = wasm_memory.verify_size() {
            error!(
                self.logger,
                "{}: Canister {} has invalid initial wasm memory size: {}",
                SANDBOXED_EXECUTION_INVALID_MEMORY_SIZE,
                canister_id,
                err
            );
            self.metrics
                .sandboxed_execution_critical_error_invalid_memory_size
                .inc();
        }

        let stable_memory = Memory::default();
        let execution_state = ExecutionState::new(
            canister_root,
            wasm_binary,
            ExportedFunctions::new(serialized_module.exported_functions.clone()),
            wasm_memory,
            stable_memory,
            exported_globals,
            serialized_module.wasm_metadata.clone(),
        );
        Ok((
            execution_state,
            serialized_module.compilation_cost,
            compilation_result,
        ))
    }
}

fn observe_metrics(metrics: &SandboxedExecutionMetrics, imports_details: &WasmImportsDetails) {
    if imports_details.imports_call_simple {
        metrics.sandboxed_execution_wasm_imports_call_simple.inc();
    }
    if imports_details.imports_controller_size {
        metrics
            .sandboxed_execution_wasm_imports_controller_size
            .inc();
    }
    if imports_details.imports_controller_copy {
        metrics
            .sandboxed_execution_wasm_imports_controller_copy
            .inc();
    }
    if imports_details.imports_call_cycles_add {
        metrics
            .sandboxed_execution_wasm_imports_call_cycles_add
            .inc();
    }
    if imports_details.imports_canister_cycle_balance {
        metrics
            .sandboxed_execution_wasm_imports_canister_cycle_balance
            .inc();
    }
    if imports_details.imports_msg_cycles_available {
        metrics
            .sandboxed_execution_wasm_imports_msg_cycles_available
            .inc();
    }
    if imports_details.imports_msg_cycles_accept {
        metrics
            .sandboxed_execution_wasm_imports_msg_cycles_accept
            .inc();
    }
    if imports_details.imports_msg_cycles_refunded {
        metrics
            .sandboxed_execution_wasm_imports_msg_cycles_refunded
            .inc();
    }
    if imports_details.imports_mint_cycles {
        metrics.sandboxed_execution_wasm_imports_mint_cycles.inc();
    }
}

impl SandboxedExecutionController {
    /// Create a new sandboxed execution controller. It provides the
    /// same interface as the `WasmExecutor`.
    pub fn new(
        logger: ReplicaLogger,
        metrics_registry: &MetricsRegistry,
        embedder_config: &EmbeddersConfig,
    ) -> std::io::Result<Self> {
        let launcher_exec_argv = create_launcher_argv().expect("No sandbox_launcher binary found");
        let sandbox_exec_argv =
            create_sandbox_argv(embedder_config).expect("No canister_sandbox binary found");
        let backends = Arc::new(Mutex::new(HashMap::new()));
        let metrics = Arc::new(SandboxedExecutionMetrics::new(metrics_registry));

        let backends_copy = Arc::clone(&backends);
        let metrics_copy = Arc::clone(&metrics);
        let logger_copy = logger.clone();

        std::thread::spawn(move || {
            SandboxedExecutionController::monitor_and_evict_sandbox_processes(
                logger_copy,
                backends_copy,
                metrics_copy,
            );
        });

        let exit_watcher = Arc::new(ExitWatcher {
            logger: logger.clone(),
            backends: Arc::clone(&backends),
        });

        let (launcher_service, mut child) = spawn_launcher_process(
            &launcher_exec_argv[0],
            &launcher_exec_argv[1..],
            exit_watcher,
        )?;

        // We spawn a thread to wait for the exit notification of the launcher
        // process.
        thread::spawn(move || {
            let pid = child.id();
            let output = child.wait().unwrap();

            panic_due_to_exit(output, pid);
        });

        Ok(Self {
            backends,
            logger,
            sandbox_exec_argv,
            metrics,
            launcher_service,
        })
    }

    // Periodically walk through all the backend processes and:
    // - evict inactive processes,
    // - update memory usage metrics.
    fn monitor_and_evict_sandbox_processes(
        // `logger` isn't used on MacOS.
        #[allow(unused_variables)] logger: ReplicaLogger,
        backends: Arc<Mutex<HashMap<CanisterId, Backend>>>,
        metrics: Arc<SandboxedExecutionMetrics>,
    ) {
        loop {
            let sandbox_processes = scavenge_sandbox_processes(&backends);

            #[cfg(target_os = "linux")]
            {
                let mut total_anon_rss: u64 = 0;
                let mut total_memfd_rss: u64 = 0;

                // For all processes requested, get their memory usage and report
                // it keyed by pid. Ignore processes failures to get
                for (sandbox_process, stats) in &sandbox_processes {
                    let pid = sandbox_process.pid;
                    let mut process_rss = 0;
                    if let Ok(kib) = process_os_metrics::get_anon_rss(pid) {
                        total_anon_rss += kib;
                        process_rss += kib;
                        metrics
                            .sandboxed_execution_subprocess_anon_rss
                            .observe(kib as f64);
                    } else {
                        warn!(logger, "Unable to get anon RSS for pid {}", pid);
                    }
                    if let Ok(kib) = process_os_metrics::get_memfd_rss(pid) {
                        total_memfd_rss += kib;
                        process_rss += kib;
                        metrics
                            .sandboxed_execution_subprocess_memfd_rss
                            .observe(kib as f64);
                    } else {
                        warn!(logger, "Unable to get memfd RSS for pid {}", pid);
                    }
                    metrics
                        .sandboxed_execution_subprocess_rss
                        .observe(process_rss as f64);
                    match stats.status {
                        SandboxProcessStatus::Active => {
                            metrics
                                .sandboxed_execution_subprocess_active_last_used
                                .observe(stats.time_since_last_usage.as_secs_f64());
                        }
                        SandboxProcessStatus::Evicted => {
                            metrics
                                .sandboxed_execution_subprocess_evicted_last_used
                                .observe(stats.time_since_last_usage.as_secs_f64());
                        }
                    }
                }

                metrics
                    .sandboxed_execution_subprocess_anon_rss_total
                    .set(total_anon_rss.try_into().unwrap());

                metrics
                    .sandboxed_execution_subprocess_memfd_rss_total
                    .set(total_memfd_rss.try_into().unwrap());
            }

            // We don't need to record memory metrics on non-linux systems.  And
            // the functions to get memory usage use `proc` so they won't work
            // on macos anyway.
            #[cfg(not(target_os = "linux"))]
            {
                // For all processes requested, get their memory usage and report
                // it keyed by pid. Ignore processes failures to get
                for (_sandbox_process, stats) in &sandbox_processes {
                    match stats.status {
                        SandboxProcessStatus::Active => {
                            metrics
                                .sandboxed_execution_subprocess_active_last_used
                                .observe(stats.time_since_last_usage.as_secs_f64());
                        }
                        SandboxProcessStatus::Evicted => {
                            metrics
                                .sandboxed_execution_subprocess_evicted_last_used
                                .observe(stats.time_since_last_usage.as_secs_f64());
                        }
                    }
                }
            }

            // Scavenge and collect metrics sufficiently infrequently that it
            // does not use excessive compute resources. It might be sensible to
            // scale this based on the time measured to perform the collection
            // and e.g.  ensure that we are 99% idle instead of using a static
            // duration here.
            std::thread::sleep(SANDBOX_PROCESS_UPDATE_INTERVAL);
        }
    }

    fn get_sandbox_process(&self, canister_id: CanisterId) -> Arc<SandboxProcess> {
        let mut guard = self.backends.lock().unwrap();

        if let Some(backend) = (*guard).get_mut(&canister_id) {
            let old = std::mem::replace(backend, Backend::Empty);
            let sandbox_process = match old {
                Backend::Active {
                    sandbox_process, ..
                } => Some(sandbox_process),
                Backend::Evicted {
                    sandbox_process, ..
                } => sandbox_process.upgrade(),
                Backend::Empty => None,
            };
            if let Some(sandbox_process) = sandbox_process {
                let now = std::time::Instant::now();
                if SANDBOX_PROCESS_INACTIVE_TIME_BEFORE_EVICTION.as_secs() > 0 {
                    *backend = Backend::Active {
                        sandbox_process: Arc::clone(&sandbox_process),
                        last_used: now,
                    };
                } else {
                    *backend = Backend::Evicted {
                        sandbox_process: Arc::downgrade(&sandbox_process),
                        last_used: now,
                    };
                }
                return sandbox_process;
            }
        }

        let _timer = self.metrics.sandboxed_execution_spawn_process.start_timer();
        // No sandbox process found for this canister. Start a new one and register it.
        let reg = Arc::new(ActiveExecutionStateRegistry::new());
        let controller_service = ControllerServiceImpl::new(Arc::clone(&reg), self.logger.clone());

        let (sandbox_service, pid) = create_sandbox_process(
            controller_service,
            &*self.launcher_service,
            canister_id,
            self.sandbox_exec_argv.clone(),
        )
        .unwrap();

        let sandbox_process = Arc::new(SandboxProcess {
            execution_states: reg,
            sandbox_service,
            pid,
            history: SandboxProcessRequestHistory::new(),
        });

        let now = std::time::Instant::now();
        let backend = Backend::Active {
            sandbox_process: Arc::clone(&sandbox_process),
            last_used: now,
        };
        (*guard).insert(canister_id, backend);

        sandbox_process
    }

    #[allow(clippy::too_many_arguments)]
    fn process_completion(
        self: Arc<Self>,
        exec_id: ExecId,
        canister_id: CanisterId,
        execution_state: &ExecutionState,
        result: CompletionResult,
        next_wasm_memory_id: MemoryId,
        next_stable_memory_id: MemoryId,
        message_instruction_limit: NumInstructions,
        api_type_label: &'static str,
        sandbox_process: Arc<SandboxProcess>,
    ) -> WasmExecutionResult {
        let mut exec_output = match result {
            CompletionResult::Paused(slice) => {
                let paused = Box::new(PausedSandboxExecution {
                    canister_id,
                    sandbox_process,
                    exec_id,
                    next_wasm_memory_id,
                    next_stable_memory_id,
                    message_instruction_limit,
                    api_type_label,
                    controller: self,
                });
                return WasmExecutionResult::Paused(slice, paused);
            }
            CompletionResult::Finished(exec_output) => exec_output,
        };

        // If sandbox is compromised this value could be larger than the initial limit.
        if exec_output.wasm.num_instructions_left > message_instruction_limit {
            exec_output.wasm.num_instructions_left = message_instruction_limit;
            error!(self.logger, "[EXC-BUG] Canister {} completed execution with more instructions left than the initial limit.", canister_id)
        }

        let canister_state_changes = self.update_execution_state(
            &mut exec_output,
            execution_state,
            next_wasm_memory_id,
            next_stable_memory_id,
            canister_id,
            sandbox_process,
        );

        self.metrics
            .sandboxed_execution_sandbox_execute_duration
            .with_label_values(&[api_type_label])
            .observe(exec_output.execute_total_duration.as_secs_f64());
        self.metrics
            .sandboxed_execution_sandbox_execute_run_duration
            .with_label_values(&[api_type_label])
            .observe(exec_output.execute_run_duration.as_secs_f64());

        WasmExecutionResult::Finished(exec_output.slice, exec_output.wasm, canister_state_changes)
    }

    // Unless execution trapped, commit state (applying execution state
    // changes, returning system state changes to caller).
    #[allow(clippy::too_many_arguments)]
    fn update_execution_state(
        &self,
        exec_output: &mut SandboxExecOutput,
        execution_state: &ExecutionState,
        next_wasm_memory_id: MemoryId,
        next_stable_memory_id: MemoryId,
        canister_id: CanisterId,
        sandbox_process: Arc<SandboxProcess>,
    ) -> Option<CanisterStateChanges> {
        // If the execution has failed, then we don't apply any changes.
        if exec_output.wasm.wasm_result.is_err() {
            return None;
        }
        match exec_output.state.take() {
            None => None,
            Some(state_modifications) => {
                // TODO: If a canister has broken out of wasm then it might have allocated more
                // wasm or stable memory then allowed. We should add an additional check here
                // that thet canister is still within it's allowed memory usage.
                let mut wasm_memory = execution_state.wasm_memory.clone();
                wasm_memory
                    .page_map
                    .deserialize_delta(state_modifications.wasm_memory.page_delta);
                wasm_memory.size = state_modifications.wasm_memory.size;
                wasm_memory.sandbox_memory = SandboxMemory::synced(wrap_remote_memory(
                    &sandbox_process,
                    next_wasm_memory_id,
                ));
                if let Err(err) = wasm_memory.verify_size() {
                    error!(
                        self.logger,
                        "{}: Canister {} has invalid wasm memory size: {}",
                        SANDBOXED_EXECUTION_INVALID_MEMORY_SIZE,
                        canister_id,
                        err
                    );
                    self.metrics
                        .sandboxed_execution_critical_error_invalid_memory_size
                        .inc();
                }
                let mut stable_memory = execution_state.stable_memory.clone();
                stable_memory
                    .page_map
                    .deserialize_delta(state_modifications.stable_memory.page_delta);
                stable_memory.size = state_modifications.stable_memory.size;
                stable_memory.sandbox_memory = SandboxMemory::synced(wrap_remote_memory(
                    &sandbox_process,
                    next_stable_memory_id,
                ));
                if let Err(err) = stable_memory.verify_size() {
                    error!(
                        self.logger,
                        "{}: Canister {} has invalid stable memory size: {}",
                        SANDBOXED_EXECUTION_INVALID_MEMORY_SIZE,
                        canister_id,
                        err
                    );
                    self.metrics
                        .sandboxed_execution_critical_error_invalid_memory_size
                        .inc();
                }
                Some(CanisterStateChanges {
                    globals: state_modifications.globals,
                    wasm_memory,
                    stable_memory,
                    system_state_changes: state_modifications.system_state_changes,
                })
            }
        }
    }
}

/// Cache the sandbox process and wasm id of the opened wasm in the embedder
/// cache.
fn cache_opened_wasm(
    embedder_cache: &mut Option<EmbedderCache>,
    sandbox_process: &Arc<SandboxProcess>,
    wasm_id: WasmId,
) {
    let opened_wasm: HypervisorResult<OpenedWasm> =
        Ok(OpenedWasm::new(Arc::downgrade(sandbox_process), wasm_id));
    *embedder_cache = Some(EmbedderCache::new(opened_wasm));
}

/// Cache an error from compilation so that we don't try to recompile just to
/// get the same error.
fn cache_errored_wasm(embedder_cache: &mut Option<EmbedderCache>, err: HypervisorError) {
    let cache: HypervisorResult<OpenedWasm> = Err(err);
    *embedder_cache = Some(EmbedderCache::new(cache));
}

// Get compiled wasm object in sandbox. Ask cache first, upload + compile if
// needed.
fn open_wasm(
    sandbox_process: &Arc<SandboxProcess>,
    wasm_binary: &WasmBinary,
    compilation_cache: Arc<CompilationCache>,
    metrics: &SandboxedExecutionMetrics,
) -> HypervisorResult<(WasmId, Option<CompilationResult>)> {
    let mut embedder_cache = wasm_binary.embedder_cache.lock().unwrap();
    if let Some(cache) = embedder_cache.as_ref() {
        if let Some(opened_wasm) = cache.downcast::<HypervisorResult<OpenedWasm>>() {
            match opened_wasm {
                Ok(opened_wasm) => {
                    if let Some(cached_sandbox_process) = opened_wasm.sandbox_process.upgrade() {
                        metrics.inc_cache_lookup(EMBEDDER_CACHE_HIT_SUCCESS);
                        assert!(Arc::ptr_eq(&cached_sandbox_process, sandbox_process));
                        return Ok((opened_wasm.wasm_id, None));
                    } else {
                        metrics.inc_cache_lookup(EMBEDDER_CACHE_HIT_SANDBOX_EVICTED);
                    }
                }
                Err(err) => {
                    metrics.inc_cache_lookup(EMBEDDER_CACHE_HIT_COMPILATION_ERROR);
                    return Err(err.clone());
                }
            }
        }
    }

    let wasm_id = WasmId::new();
    match compilation_cache.get(&wasm_binary.binary) {
        None => {
            metrics.inc_cache_lookup(CACHE_MISS);
            sandbox_process
                .history
                .record(format!("OpenWasm(wasm_id={})", wasm_id));
            match sandbox_process
                .sandbox_service
                .open_wasm(protocol::sbxsvc::OpenWasmRequest {
                    wasm_id,
                    wasm_src: wasm_binary.binary.as_slice().to_vec(),
                })
                .sync()
                .unwrap()
                .0
            {
                Ok((compilation_result, serialized_module)) => {
                    cache_opened_wasm(&mut *embedder_cache, sandbox_process, wasm_id);
                    observe_metrics(metrics, &serialized_module.imports_details);
                    compilation_cache.insert(&wasm_binary.binary, Ok(Arc::new(serialized_module)));
                    Ok((wasm_id, Some(compilation_result)))
                }
                Err(err) => {
                    compilation_cache.insert(&wasm_binary.binary, Err(err.clone()));
                    cache_errored_wasm(&mut *embedder_cache, err.clone());
                    Err(err)
                }
            }
        }
        Some(Err(err)) => {
            metrics.inc_cache_lookup(COMPILATION_CACHE_HIT_COMPILATION_ERROR);
            cache_errored_wasm(&mut *embedder_cache, err.clone());
            Err(err)
        }
        Some(Ok(serialized_module)) => {
            metrics.inc_cache_lookup(COMPILATION_CACHE_HIT);
            observe_metrics(metrics, &serialized_module.imports_details);
            sandbox_process
                .history
                .record(format!("OpenWasmSerialized(wasm_id={})", wasm_id));
            sandbox_process
                .sandbox_service
                .open_wasm_serialized(protocol::sbxsvc::OpenWasmSerializedRequest {
                    wasm_id,
                    serialized_module: Arc::clone(&serialized_module.bytes),
                })
                .on_completion(|_| ());
            cache_opened_wasm(&mut *embedder_cache, sandbox_process, wasm_id);
            Ok((wasm_id, None))
        }
    }
}

// Returns the id of the remote memory after making sure that the remote memory
// is in sync with the local memory.
fn open_remote_memory(
    sandbox_process: &Arc<SandboxProcess>,
    memory: &Memory,
) -> SandboxMemoryHandle {
    let mut guard = memory.sandbox_memory.lock().unwrap();
    match &*guard {
        SandboxMemory::Synced(id) => id.clone(),
        SandboxMemory::Unsynced => {
            let serialized_page_map = memory.page_map.serialize();
            // Only clean memory without any dirty pages can be unsynced.
            // That is because all dirty pages are created by the sandbox and
            // they are automatically synced using `wrap_remote_memory`.
            assert!(serialized_page_map.page_delta.is_empty());
            assert!(serialized_page_map.round_delta.is_empty());
            let serialized_memory = MemorySerialization {
                page_map: serialized_page_map,
                num_wasm_pages: memory.size,
            };
            let memory_id = MemoryId::new();
            sandbox_process
                .history
                .record(format!("OpenMemory(memory_id={})", memory_id));
            sandbox_process
                .sandbox_service
                .open_memory(protocol::sbxsvc::OpenMemoryRequest {
                    memory_id,
                    memory: serialized_memory,
                })
                .on_completion(|_| {});
            let handle = wrap_remote_memory(sandbox_process, memory_id);
            *guard = SandboxMemory::Synced(handle.clone());
            handle
        }
    }
}

fn wrap_remote_memory(
    sandbox_process: &Arc<SandboxProcess>,
    memory_id: MemoryId,
) -> SandboxMemoryHandle {
    let opened_memory = OpenedMemory::new(Arc::clone(sandbox_process), memory_id);
    SandboxMemoryHandle::new(Arc::new(opened_memory))
}

// Evicts inactive process and returns all processes that are still alive.
fn scavenge_sandbox_processes(
    backends: &Arc<Mutex<HashMap<CanisterId, Backend>>>,
) -> Vec<(Arc<SandboxProcess>, SandboxProcessStats)> {
    let mut guard = backends.lock().unwrap();
    let now = std::time::Instant::now();
    let mut result = vec![];
    for backend in guard.values_mut() {
        let old = std::mem::replace(backend, Backend::Empty);
        let new = match old {
            Backend::Active {
                sandbox_process,
                last_used,
            } => {
                let inactive_time = now
                    .checked_duration_since(last_used)
                    .unwrap_or_else(|| std::time::Duration::from_secs(0));
                if inactive_time > SANDBOX_PROCESS_INACTIVE_TIME_BEFORE_EVICTION {
                    result.push((
                        Arc::clone(&sandbox_process),
                        SandboxProcessStats {
                            time_since_last_usage: inactive_time,
                            status: SandboxProcessStatus::Evicted,
                        },
                    ));
                    Backend::Evicted {
                        sandbox_process: Arc::downgrade(&sandbox_process),
                        last_used,
                    }
                } else {
                    result.push((
                        Arc::clone(&sandbox_process),
                        SandboxProcessStats {
                            time_since_last_usage: inactive_time,
                            status: SandboxProcessStatus::Active,
                        },
                    ));
                    Backend::Active {
                        sandbox_process,
                        last_used,
                    }
                }
            }
            Backend::Evicted {
                sandbox_process,
                last_used,
            } => match sandbox_process.upgrade() {
                Some(strong_reference) => {
                    let inactive_time = now
                        .checked_duration_since(last_used)
                        .unwrap_or_else(|| std::time::Duration::from_secs(0));
                    result.push((
                        strong_reference,
                        SandboxProcessStats {
                            time_since_last_usage: inactive_time,
                            status: SandboxProcessStatus::Evicted,
                        },
                    ));
                    Backend::Evicted {
                        sandbox_process,
                        last_used,
                    }
                }
                None => Backend::Empty,
            },
            Backend::Empty => Backend::Empty,
        };
        *backend = new;
    }
    result
}

pub fn panic_due_to_exit(output: ExitStatus, pid: u32) {
    match output.code() {
        Some(code) => panic!(
            "Error from launcher process, pid {} exited with status code: {}",
            pid, code
        ),
        None => panic!(
            "Error from launcher process, pid {} exited due to signal!",
            pid
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, File};

    use super::*;
    use ic_config::logger::Config as LoggerConfig;
    use ic_logger::{new_replica_logger, replica_logger::no_op_logger};
    use ic_test_utilities::types::ids::canister_test_id;
    use libc::kill;
    use slog::{o, Drain};

    #[test]
    #[should_panic(expected = "exited due to signal!")]
    fn controller_handles_killed_launcher_process() {
        let launcher_exec_argv = create_launcher_argv().unwrap();
        let exit_watcher = Arc::new(ExitWatcher {
            logger: no_op_logger(),
            backends: Arc::new(Mutex::new(HashMap::new())),
        });

        let (_launcher_service, mut child) = spawn_launcher_process(
            &launcher_exec_argv[0],
            &launcher_exec_argv[1..],
            exit_watcher,
        )
        .unwrap();

        let pid = child.id();

        unsafe {
            kill(pid.try_into().unwrap(), libc::SIGKILL);
        }
        let output = child.wait().unwrap();
        panic_due_to_exit(output, pid);
    }

    #[test]
    fn sandbox_history_logged_on_sandbox_crash() {
        let tempdir = tempfile::tempdir().unwrap();
        let log_path = tempdir.path().join("log");
        let file = File::create(&log_path).unwrap();

        let decorator = slog_term::PlainDecorator::new(file);
        let drain = slog_term::FullFormat::new(decorator).build().fuse();
        let drain = slog_async::Async::new(drain).build().fuse();

        let root = slog::Logger::root(drain, o!());
        let logger = new_replica_logger(root, &LoggerConfig::default());

        let controller = SandboxedExecutionController::new(
            logger,
            &MetricsRegistry::new(),
            &EmbeddersConfig::default(),
        )
        .unwrap();

        let wat = "(module)";
        let canister_module = CanisterModule::new(wabt::wat2wasm(wat).unwrap());
        let canister_id = canister_test_id(0);
        controller
            .create_execution_state(
                canister_module,
                PathBuf::new(),
                canister_id,
                Arc::new(CompilationCache::default()),
            )
            .unwrap();
        let sandbox_pid = match controller
            .backends
            .lock()
            .unwrap()
            .get(&canister_id)
            .unwrap()
        {
            Backend::Active {
                sandbox_process, ..
            } => sandbox_process.pid,
            Backend::Evicted { .. } | Backend::Empty => panic!("sandbox should be active"),
        };

        unsafe {
            kill(sandbox_pid.try_into().unwrap(), libc::SIGKILL);
        }

        let mut logs = String::new();
        while logs.is_empty() {
            thread::sleep(Duration::from_millis(100));
            logs = fs::read_to_string(&log_path).unwrap();
        }
        assert!(logs.contains(&format!(
            "History for canister {} with pid {}: CreateExecutionState",
            canister_id, sandbox_pid
        )));
    }
}

/// Service responsible for printing the history of a canister's activity when
/// it unexpectedly exits.
struct ExitWatcher {
    logger: ReplicaLogger,
    backends: Arc<Mutex<HashMap<CanisterId, Backend>>>,
}

impl ControllerLauncherService for ExitWatcher {
    fn sandbox_exited(
        &self,
        req: protocol::ctllaunchersvc::SandboxExitedRequest,
    ) -> ic_canister_sandbox_common::rpc::Call<protocol::ctllaunchersvc::SandboxExitedReply> {
        let guard = self.backends.lock().unwrap();
        let sandbox_process = match guard.get(&req.canister_id).unwrap_or_else(|| {
            panic!(
                "Sandbox exited for unrecognized canister id {}",
                req.canister_id,
            )
        }) {
            Backend::Active {
                sandbox_process, ..
            } => sandbox_process,
            Backend::Evicted { .. } | Backend::Empty => {
                return rpc::Call::new_resolved(Ok(protocol::ctllaunchersvc::SandboxExitedReply));
            }
        };
        sandbox_process
            .history
            .replay(&self.logger, req.canister_id, sandbox_process.pid);
        rpc::Call::new_resolved(Ok(protocol::ctllaunchersvc::SandboxExitedReply))
    }
}
