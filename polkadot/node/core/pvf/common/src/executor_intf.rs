// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Interface to the Substrate Executor

use polkadot_primitives::{
	executor_params::{DEFAULT_LOGICAL_STACK_MAX, DEFAULT_NATIVE_STACK_MAX},
	ExecutorParam, ExecutorParams,
};
use sc_executor_common::{
	error::WasmError,
	runtime_blob::RuntimeBlob,
	wasm_runtime::{HeapAllocStrategy, InvokeMethod, WasmModule as _},
};
use sc_executor_wasmtime::{Config, DeterministicStackLimit, Semantics, WasmtimeRuntime};
use sp_core::storage::{ChildInfo, TrackedStorageKey};
use sp_externalities::MultiRemovalResults;
use std::any::{Any, TypeId};

// Memory configuration
//
// When Substrate Runtime is instantiated, a number of WASM pages are allocated for the Substrate
// Runtime instance's linear memory. The exact number of pages is a sum of whatever the WASM blob
// itself requests (by default at least enough to hold the data section as well as have some space
// left for the stack; this is, of course, overridable at link time when compiling the runtime)
// plus the number of pages specified in the `extra_heap_pages` passed to the executor.
//
// By default, rustc (or `lld` specifically) should allocate 1 MiB for the shadow stack, or 16
// pages. The data section for runtimes are typically rather small and can fit in a single digit
// number of WASM pages, so let's say an extra 16 pages. Thus let's assume that 32 pages or 2 MiB
// are used for these needs by default.
const DEFAULT_HEAP_PAGES_ESTIMATE: u32 = 32;
const EXTRA_HEAP_PAGES: u32 = 2048;

// VALUES OF THE DEFAULT CONFIGURATION SHOULD NEVER BE CHANGED
// They are used as base values for the execution environment parametrization.
// To overwrite them, add new ones to `EXECUTOR_PARAMS` in the `session_info` pallet and perform
// a runtime upgrade to make them active.
pub const DEFAULT_CONFIG: Config = Config {
	allow_missing_func_imports: true,
	cache_path: None,
	semantics: Semantics {
		heap_alloc_strategy: sc_executor_common::wasm_runtime::HeapAllocStrategy::Dynamic {
			maximum_pages: Some(DEFAULT_HEAP_PAGES_ESTIMATE + EXTRA_HEAP_PAGES),
		},

		instantiation_strategy:
			sc_executor_wasmtime::InstantiationStrategy::RecreateInstanceCopyOnWrite,

		// Enable deterministic stack limit to pin down the exact number of items the wasmtime stack
		// can contain before it traps with stack overflow.
		//
		// Here is how the values below were chosen.
		//
		// At the moment of writing, the default native stack size limit is 1 MiB. Assuming a
		// logical item (see the docs about the field and the instrumentation algorithm) is 8 bytes,
		// 1 MiB can fit 2x 65536 logical items.
		//
		// Since reaching the native stack limit is undesirable, we halve the logical item limit and
		// also increase the native 256x. This hopefully should preclude wasm code from reaching
		// the stack limit set by the wasmtime.
		deterministic_stack_limit: Some(DeterministicStackLimit {
			logical_max: DEFAULT_LOGICAL_STACK_MAX,
			native_stack_max: DEFAULT_NATIVE_STACK_MAX,
		}),
		canonicalize_nans: true,
		// Rationale for turning the multi-threaded compilation off is to make the preparation time
		// easily reproducible and as deterministic as possible.
		//
		// Currently the prepare queue doesn't distinguish between precheck and prepare requests.
		// On the one hand, it simplifies the code, on the other, however, slows down compile times
		// for execute requests. This behavior may change in future.
		parallel_compilation: false,

		// WASM extensions. Only those that are meaningful to us may be controlled here. By default,
		// we're using WASM MVP, which means all the extensions are disabled. Nevertheless, some
		// extensions (e.g., sign extension ops) are enabled by Wasmtime and cannot be disabled.
		wasm_reference_types: false,
		wasm_simd: false,
		wasm_bulk_memory: false,
		wasm_multi_value: false,
	},
};

/// Executes the given PVF in the form of a compiled artifact and returns the result of
/// execution upon success.
///
/// # Safety
///
/// The caller must ensure that the compiled artifact passed here was:
///   1) produced by `prepare`,
///   2) was not modified,
///
/// Failure to adhere to these requirements might lead to crashes and arbitrary code execution.
pub unsafe fn execute_artifact(
	compiled_artifact_blob: &[u8],
	executor_params: &ExecutorParams,
	params: &[u8],
) -> Result<Vec<u8>, String> {
	let mut extensions = sp_externalities::Extensions::new();

	extensions.register(sp_core::traits::ReadRuntimeVersionExt::new(ReadRuntimeVersion));

	let mut ext = ValidationExternalities(extensions);

	match sc_executor::with_externalities_safe(&mut ext, || {
		let runtime = create_runtime_from_artifact_bytes(compiled_artifact_blob, executor_params)?;
		runtime.new_instance()?.call(InvokeMethod::Export("validate_block"), params)
	}) {
		Ok(Ok(ok)) => Ok(ok),
		Ok(Err(err)) | Err(err) => Err(err),
	}
	.map_err(|err| format!("execute error: {:?}", err))
}

/// Constructs the runtime for the given PVF, given the artifact bytes.
///
/// # Safety
///
/// The caller must ensure that the compiled artifact passed here was:
///   1) produced by `prepare`,
///   2) was not modified,
///
/// Failure to adhere to these requirements might lead to crashes and arbitrary code execution.
pub unsafe fn create_runtime_from_artifact_bytes(
	compiled_artifact_blob: &[u8],
	executor_params: &ExecutorParams,
) -> Result<WasmtimeRuntime, WasmError> {
	let mut config = DEFAULT_CONFIG.clone();
	config.semantics = params_to_wasmtime_semantics(executor_params);

	sc_executor_wasmtime::create_runtime_from_artifact_bytes::<HostFunctions>(
		compiled_artifact_blob,
		config,
	)
}

pub fn params_to_wasmtime_semantics(par: &ExecutorParams) -> Semantics {
	let mut sem = DEFAULT_CONFIG.semantics.clone();
	let mut stack_limit = sem
		.deterministic_stack_limit
		.expect("There is a comment to not change the default stack limit; it should always be available; qed")
		.clone();

	for p in par.iter() {
		match p {
			ExecutorParam::MaxMemoryPages(max_pages) =>
				sem.heap_alloc_strategy = HeapAllocStrategy::Dynamic {
					maximum_pages: Some((*max_pages).saturating_add(DEFAULT_HEAP_PAGES_ESTIMATE)),
				},
			ExecutorParam::StackLogicalMax(slm) => stack_limit.logical_max = *slm,
			ExecutorParam::StackNativeMax(snm) => stack_limit.native_stack_max = *snm,
			ExecutorParam::WasmExtBulkMemory => sem.wasm_bulk_memory = true,
			ExecutorParam::PrecheckingMaxMemory(_) |
			ExecutorParam::PvfPrepTimeout(_, _) |
			ExecutorParam::PvfExecTimeout(_, _) => (), /* Not used here */
		}
	}
	sem.deterministic_stack_limit = Some(stack_limit);
	sem
}

/// Runs the prevalidation on the given code. Returns a [`RuntimeBlob`] if it succeeds.
pub fn prevalidate(code: &[u8]) -> Result<RuntimeBlob, sc_executor_common::error::WasmError> {
	let blob = RuntimeBlob::new(code)?;
	// It's assumed this function will take care of any prevalidation logic
	// that needs to be done.
	//
	// Do nothing for now.
	Ok(blob)
}

/// Runs preparation on the given runtime blob. If successful, it returns a serialized compiled
/// artifact which can then be used to pass into `Executor::execute` after writing it to the disk.
pub fn prepare(
	blob: RuntimeBlob,
	executor_params: &ExecutorParams,
) -> Result<Vec<u8>, sc_executor_common::error::WasmError> {
	let semantics = params_to_wasmtime_semantics(executor_params);
	sc_executor_wasmtime::prepare_runtime_artifact(blob, &semantics)
}

/// Available host functions. We leave out:
///
/// 1. storage related stuff (PVF doesn't have a notion of a persistent storage/trie)
/// 2. tracing
/// 3. off chain workers (PVFs do not have such a notion)
/// 4. runtime tasks
/// 5. sandbox
type HostFunctions = (
	sp_io::misc::HostFunctions,
	sp_io::crypto::HostFunctions,
	sp_io::hashing::HostFunctions,
	sp_io::allocator::HostFunctions,
	sp_io::logging::HostFunctions,
	sp_io::trie::HostFunctions,
);

/// The validation externalities that will panic on any storage related access. (PVFs should not
/// have a notion of a persistent storage/trie.)
struct ValidationExternalities(sp_externalities::Extensions);

impl sp_externalities::Externalities for ValidationExternalities {
	fn storage(&self, _: &[u8]) -> Option<Vec<u8>> {
		panic!("storage: unsupported feature for parachain validation")
	}

	fn storage_hash(&self, _: &[u8]) -> Option<Vec<u8>> {
		panic!("storage_hash: unsupported feature for parachain validation")
	}

	fn child_storage_hash(&self, _: &ChildInfo, _: &[u8]) -> Option<Vec<u8>> {
		panic!("child_storage_hash: unsupported feature for parachain validation")
	}

	fn child_storage(&self, _: &ChildInfo, _: &[u8]) -> Option<Vec<u8>> {
		panic!("child_storage: unsupported feature for parachain validation")
	}

	fn kill_child_storage(
		&mut self,
		_child_info: &ChildInfo,
		_maybe_limit: Option<u32>,
		_maybe_cursor: Option<&[u8]>,
	) -> MultiRemovalResults {
		panic!("kill_child_storage: unsupported feature for parachain validation")
	}

	fn clear_prefix(
		&mut self,
		_prefix: &[u8],
		_maybe_limit: Option<u32>,
		_maybe_cursor: Option<&[u8]>,
	) -> MultiRemovalResults {
		panic!("clear_prefix: unsupported feature for parachain validation")
	}

	fn clear_child_prefix(
		&mut self,
		_child_info: &ChildInfo,
		_prefix: &[u8],
		_maybe_limit: Option<u32>,
		_maybe_cursor: Option<&[u8]>,
	) -> MultiRemovalResults {
		panic!("clear_child_prefix: unsupported feature for parachain validation")
	}

	fn place_storage(&mut self, _: Vec<u8>, _: Option<Vec<u8>>) {
		panic!("place_storage: unsupported feature for parachain validation")
	}

	fn place_child_storage(&mut self, _: &ChildInfo, _: Vec<u8>, _: Option<Vec<u8>>) {
		panic!("place_child_storage: unsupported feature for parachain validation")
	}

	fn storage_root(&mut self, _: sp_core::storage::StateVersion) -> Vec<u8> {
		panic!("storage_root: unsupported feature for parachain validation")
	}

	fn child_storage_root(&mut self, _: &ChildInfo, _: sp_core::storage::StateVersion) -> Vec<u8> {
		panic!("child_storage_root: unsupported feature for parachain validation")
	}

	fn next_child_storage_key(&self, _: &ChildInfo, _: &[u8]) -> Option<Vec<u8>> {
		panic!("next_child_storage_key: unsupported feature for parachain validation")
	}

	fn next_storage_key(&self, _: &[u8]) -> Option<Vec<u8>> {
		panic!("next_storage_key: unsupported feature for parachain validation")
	}

	fn storage_append(&mut self, _key: Vec<u8>, _value: Vec<u8>) {
		panic!("storage_append: unsupported feature for parachain validation")
	}

	fn storage_start_transaction(&mut self) {
		panic!("storage_start_transaction: unsupported feature for parachain validation")
	}

	fn storage_rollback_transaction(&mut self) -> Result<(), ()> {
		panic!("storage_rollback_transaction: unsupported feature for parachain validation")
	}

	fn storage_commit_transaction(&mut self) -> Result<(), ()> {
		panic!("storage_commit_transaction: unsupported feature for parachain validation")
	}

	fn wipe(&mut self) {
		panic!("wipe: unsupported feature for parachain validation")
	}

	fn commit(&mut self) {
		panic!("commit: unsupported feature for parachain validation")
	}

	fn read_write_count(&self) -> (u32, u32, u32, u32) {
		panic!("read_write_count: unsupported feature for parachain validation")
	}

	fn reset_read_write_count(&mut self) {
		panic!("reset_read_write_count: unsupported feature for parachain validation")
	}

	fn get_whitelist(&self) -> Vec<TrackedStorageKey> {
		panic!("get_whitelist: unsupported feature for parachain validation")
	}

	fn set_whitelist(&mut self, _: Vec<TrackedStorageKey>) {
		panic!("set_whitelist: unsupported feature for parachain validation")
	}

	fn set_offchain_storage(&mut self, _: &[u8], _: std::option::Option<&[u8]>) {
		panic!("set_offchain_storage: unsupported feature for parachain validation")
	}

	fn get_read_and_written_keys(&self) -> Vec<(Vec<u8>, u32, u32, bool)> {
		panic!("get_read_and_written_keys: unsupported feature for parachain validation")
	}
}

impl sp_externalities::ExtensionStore for ValidationExternalities {
	fn extension_by_type_id(&mut self, type_id: TypeId) -> Option<&mut dyn Any> {
		self.0.get_mut(type_id)
	}

	fn register_extension_with_type_id(
		&mut self,
		type_id: TypeId,
		extension: Box<dyn sp_externalities::Extension>,
	) -> Result<(), sp_externalities::Error> {
		self.0.register_with_type_id(type_id, extension)
	}

	fn deregister_extension_by_type_id(
		&mut self,
		type_id: TypeId,
	) -> Result<(), sp_externalities::Error> {
		if self.0.deregister(type_id) {
			Ok(())
		} else {
			Err(sp_externalities::Error::ExtensionIsNotRegistered(type_id))
		}
	}
}

struct ReadRuntimeVersion;

impl sp_core::traits::ReadRuntimeVersion for ReadRuntimeVersion {
	fn read_runtime_version(
		&self,
		wasm_code: &[u8],
		_ext: &mut dyn sp_externalities::Externalities,
	) -> Result<Vec<u8>, String> {
		let blob = RuntimeBlob::uncompress_if_needed(wasm_code)
			.map_err(|e| format!("Failed to read the PVF runtime blob: {:?}", e))?;

		match sc_executor::read_embedded_version(&blob)
			.map_err(|e| format!("Failed to read the static section from the PVF blob: {:?}", e))?
		{
			Some(version) => {
				use parity_scale_codec::Encode;
				Ok(version.encode())
			},
			None => Err("runtime version section is not found".to_string()),
		}
	}
}
