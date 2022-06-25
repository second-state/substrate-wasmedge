use crate::{host::HostState, instance_wrapper::InstanceWrapper, util};
use sc_allocator::FreeingBumpHeapAllocator;
use sc_executor_common::{
	error::{Result, WasmError},
	runtime_blob::{
		self, DataSegmentsSnapshot, ExposedMutableGlobalsSet, GlobalsSnapshot, RuntimeBlob,
	},
	wasm_runtime::{InvokeMethod, WasmInstance, WasmModule},
};
use sp_runtime_interface::unpack_ptr_and_len;
use sp_wasm_interface::{Function, HostFunctions, Pointer, Value, WordSize};
use std::{
	fs::File,
	io::Write,
	path::Path,
	sync::{Arc, Mutex},
};
use wasmedge_sys::Vm;

pub struct Config {
	/// The WebAssembly standard requires all imports of an instantiated module to be resolved,
	/// otherwise, the instantiation fails. If this option is set to `true`, then this behavior is
	/// overriden and imports that are requested by the module and not provided by the host
	/// functions will be resolved using stubs. These stubs will trap upon a call.
	pub allow_missing_func_imports: bool,

	/// Tuning of various semantics of the wasmedge executor.
	pub semantics: Semantics,
}

/// Knobs for deterministic stack height limiting.
///
/// The WebAssembly standard defines a call/value stack but it doesn't say anything about its
/// size except that it has to be finite. The implementations are free to choose their own notion
/// of limit: some may count the number of calls or values, others would rely on the host machine
/// stack and trap on reaching a guard page.
///
/// This obviously is a source of non-determinism during execution. This feature can be used
/// to instrument the code so that it will count the depth of execution in some deterministic
/// way (the machine stack limit should be so high that the deterministic limit always triggers
/// first).
///
/// The deterministic stack height limiting feature allows to instrument the code so that it will
/// count the number of items that may be on the stack. This counting will only act as an rough
/// estimate of the actual stack limit in wasmedge. This is because wasmedge measures it's stack
/// usage in bytes.
///
/// The actual number of bytes consumed by a function is not trivial to compute  without going
/// through full compilation. Therefore, it's expected that `native_stack_max` is greatly
/// overestimated and thus never reached in practice. The stack overflow check introduced by the
/// instrumentation and that relies on the logical item count should be reached first.
///
/// See [here][stack_height] for more details of the instrumentation
///
/// [stack_height]: https://github.com/paritytech/wasm-utils/blob/d9432baf/src/stack_height/mod.rs#L1-L50
pub struct DeterministicStackLimit {
	/// A number of logical "values" that can be pushed on the wasm stack. A trap will be triggered
	/// if exceeded.
	///
	/// A logical value is a local, an argument or a value pushed on operand stack.
	pub logical_max: u32,
}

pub struct Semantics {
	/// Enabling this will lead to some optimization shenanigans that make calling [`WasmInstance`]
	/// extremely fast.
	///
	/// Primarily this is achieved by not recreating the instance for each call and performing a
	/// bare minimum clean up: reapplying the data segments and restoring the values for global
	/// variables.
	///
	/// Since this feature depends on instrumentation, it can be set only if runtime is
	/// instantiated using the runtime blob, e.g. using [`create_runtime`].
	// I.e. if [`CodeSupplyMode::Verbatim`] is used.
	pub fast_instance_reuse: bool,

	/// Specifying `Some` will enable deterministic stack height. That is, all executor
	/// invocations will reach stack overflow at the exactly same point across different wasmedge
	/// versions and architectures.
	///
	/// This is achieved by a combination of running an instrumentation pass on input code and
	/// configuring wasmedge accordingly.
	///
	/// Since this feature depends on instrumentation, it can be set only if runtime is
	/// instantiated using the runtime blob, e.g. using [`create_runtime`].
	// I.e. if [`CodeSupplyMode::Verbatim`] is used.
	pub deterministic_stack_limit: Option<DeterministicStackLimit>,

	/// The number of extra WASM pages which will be allocated
	/// on top of what is requested by the WASM blob itself.
	pub extra_heap_pages: u64,

	/// The total amount of memory in bytes an instance can request.
	///
	/// If specified, the runtime will be able to allocate only that much of wasm memory.
	/// This is the total number and therefore the [`Semantics::extra_heap_pages`] is accounted
	/// for.
	///
	/// That means that the initial number of pages of a linear memory plus the
	/// [`Semantics::extra_heap_pages`] multiplied by the wasm page size (64KiB) should be less
	/// than or equal to `max_memory_size`, otherwise the instance won't be created.
	///
	/// Moreover, `memory.grow` will fail (return -1) if the sum of sizes of currently mounted
	/// and additional pages exceeds `max_memory_size`.
	///
	/// The default is `None`.
	pub max_memory_size: Option<usize>,
}

/// Data required for creating instances with the fast instance reuse strategy.
struct InstanceSnapshotData {
	mutable_globals: ExposedMutableGlobalsSet,
	data_segments_snapshot: Arc<DataSegmentsSnapshot>,
}

/// A `WasmModule` implementation using wasmtime to compile the runtime module to machine code
/// and execute the compiled code.
pub struct WasmEdgeRuntime {
	vm: Arc<Mutex<Vm>>,
	snapshot_data: Option<InstanceSnapshotData>,
	host_functions: Vec<&'static dyn Function>,
	module: wasmedge_sys::Module,
	config: Config,
}

impl WasmModule for WasmEdgeRuntime {
	fn new_instance(&self) -> Result<Box<dyn WasmInstance>> {
		let instance_wrapper = InstanceWrapper::new(Arc::clone(&self.vm));

		crate::imports::prepare_imports(
			Arc::clone(&instance_wrapper),
			&self.module,
			&self.host_functions,
			self.config.allow_missing_func_imports,
		)
		.map_err(|e| WasmError::Other(format!("fail to register imports: {}", e)))?;

		let strategy = if let Some(ref snapshot_data) = self.snapshot_data {
			instance_wrapper
				.lock()
				.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
				.instantiate()?;
			let heap_base = instance_wrapper
				.lock()
				.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
				.extract_heap_base()?;

			// This function panics if the instance was created from a runtime blob different from
			// which the mutable globals were collected. Here, it is easy to see that there is only
			// a single runtime blob and thus it's the same that was used for both creating the
			// instance and collecting the mutable globals.
			let globals_snapshot = GlobalsSnapshot::take(
				&snapshot_data.mutable_globals,
				&mut InstanceGlobals { instance: Arc::clone(&instance_wrapper) },
			);

			Strategy::FastInstanceReuse {
				instance_wrapper,
				globals_snapshot,
				data_segments_snapshot: snapshot_data.data_segments_snapshot.clone(),
				heap_base,
			}
		} else {
			Strategy::RecreateInstance(InstanceCreator {
				instance_wrapper: Arc::clone(&instance_wrapper),
			})
		};

		Ok(Box::new(WasmEdgeInstance { strategy }))
	}
}

struct InstanceGlobals {
	instance: Arc<Mutex<InstanceWrapper>>,
}

impl runtime_blob::InstanceGlobals for InstanceGlobals {
	type Global = Arc<Mutex<wasmedge_sys::Global>>;

	fn get_global(&mut self, export_name: &str) -> Self::Global {
		Arc::new(Mutex::new(
			self.instance.lock().expect("failed to lock").get_global(export_name).expect(
				"get_global is guaranteed to be called with an export name of a global; qed",
			),
		))
	}

	fn get_global_value(&mut self, global: &Self::Global) -> Value {
		util::from_wasmedge_val(global.lock().expect("failed to lock").get_value())
	}

	fn set_global_value(&mut self, global: &Self::Global, value: Value) {
		global.lock().expect("failed to lock").set_value(util::into_wasmedge_val(value)).expect(
			"the value is guaranteed to be of the same value; the global is guaranteed to be mutable; qed",
		);
	}
}

/// A `WasmInstance` implementation that reuses compiled module and spawns instances
/// to execute the compiled code.
pub struct WasmEdgeInstance {
	strategy: Strategy,
}

enum Strategy {
	FastInstanceReuse {
		instance_wrapper: Arc<Mutex<InstanceWrapper>>,
		globals_snapshot: GlobalsSnapshot<Arc<Mutex<wasmedge_sys::Global>>>,
		data_segments_snapshot: Arc<DataSegmentsSnapshot>,
		heap_base: u32,
	},
	RecreateInstance(InstanceCreator),
}

struct InstanceCreator {
	instance_wrapper: Arc<Mutex<InstanceWrapper>>,
}

impl InstanceCreator {
	fn instantiate(&mut self) -> Result<()> {
		Ok(self
			.instance_wrapper
			.lock()
			.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
			.instantiate()?)
	}
}

impl WasmInstance for WasmEdgeInstance {
	fn call(&mut self, method: InvokeMethod, data: &[u8]) -> Result<Vec<u8>> {
		match &mut self.strategy {
			Strategy::FastInstanceReuse {
				instance_wrapper,
				globals_snapshot,
				data_segments_snapshot,
				heap_base,
			} => {
				data_segments_snapshot.apply(|offset, contents| {
					util::write_memory_from(
						util::memory_slice_mut(
							instance_wrapper
								.lock()
								.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
								.memory_mut(),
						),
						Pointer::new(offset),
						contents,
					)
				})?;

				globals_snapshot
					.apply(&mut InstanceGlobals { instance: Arc::clone(instance_wrapper) });
				let allocator = FreeingBumpHeapAllocator::new(*heap_base);

				let result = perform_call(data, instance_wrapper, method, allocator);

				// Signal to the OS that we are done with the linear memory and that it can be
				// reclaimed.
				instance_wrapper
					.lock()
					.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
					.decommit();

				result
			},
			Strategy::RecreateInstance(instance_creator) => {
				instance_creator.instantiate()?;
				let heap_base =
					instance_creator.instance_wrapper.lock().unwrap().extract_heap_base()?;

				let allocator = FreeingBumpHeapAllocator::new(heap_base);

				perform_call(data, &instance_creator.instance_wrapper, method, allocator)
			},
		}
	}

	fn get_global_const(&mut self, name: &str) -> Result<Option<Value>> {
		match &mut self.strategy {
			Strategy::FastInstanceReuse { instance_wrapper, .. } => instance_wrapper
				.lock()
				.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
				.get_global_val(name),
			Strategy::RecreateInstance(ref mut instance_creator) => {
				instance_creator.instantiate()?;
				instance_creator
					.instance_wrapper
					.lock()
					.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
					.get_global_val(name)
			},
		}
	}

	fn linear_memory_base_ptr(&self) -> Option<*const u8> {
		match &self.strategy {
			Strategy::RecreateInstance(_) => {
				// We do not keep the wasm instance around, therefore there is no linear memory
				// associated with it.
				None
			},
			Strategy::FastInstanceReuse { instance_wrapper, .. } => {
				Some(instance_wrapper.lock().expect("failed to lock").base_ptr())
			},
		}
	}
}

enum CodeSupplyMode<'a> {
	/// The runtime is instantiated using the given runtime blob.
	Fresh(RuntimeBlob),

	/// The runtime is instantiated using a precompiled module.
	///
	/// This assumes that the code is already prepared for execution and the same `Config` was
	/// used.
	///
	/// We use a `Path` here instead of simply passing a byte slice to allow `wasmedge` to
	/// map the runtime's linear memory on supported platforms in a copy-on-write fashion.
	Precompiled(&'a Path),
}

/// Create a new `WasmEdgeRuntime` given the code. This function performs translation from Wasm to
/// machine code, which can be computationally heavy.
///
/// The `H` generic parameter is used to statically pass a set of host functions which are exposed
/// to the runtime.
pub fn create_runtime<H>(
	blob: RuntimeBlob,
	config: Config,
) -> std::result::Result<WasmEdgeRuntime, WasmError>
where
	H: HostFunctions,
{
	// SAFETY: this is safe because it doesn't use `CodeSupplyMode::Precompiled`.
	unsafe { do_create_runtime::<H>(CodeSupplyMode::Fresh(blob), config) }
}

/// The same as [`create_runtime`] but takes a path to a precompiled artifact,
/// which makes this function considerably faster than [`create_runtime`].
///
/// # Safety
///
/// The caller must ensure that the compiled artifact passed here was:
///   1) produced by [`prepare_runtime_artifact`],
///   2) written to the disk as a file,
///   3) was not modified,
///   4) will not be modified while any runtime using this artifact is alive, or is being
///      instantiated.
///
/// Failure to adhere to these requirements might lead to crashes and arbitrary code execution.
///
/// It is ok though if the compiled artifact was created by code of another version or with
/// different configuration flags. In such case the caller will receive an `Err` deterministically.
pub unsafe fn create_runtime_from_artifact<H>(
	compiled_artifact_path: &Path,
	config: Config,
) -> std::result::Result<WasmEdgeRuntime, WasmError>
where
	H: HostFunctions,
{
	do_create_runtime::<H>(CodeSupplyMode::Precompiled(compiled_artifact_path), config)
}

/// Takes a [`RuntimeBlob`] and precompiles it returning the serialized result of compilation. It
/// can then be used for calling [`create_runtime`] avoiding long compilation times.
pub fn prepare_runtime_artifact(
	blob: RuntimeBlob,
	semantics: &Semantics,
	compiled_artifact_path: &Path,
) -> std::result::Result<(), WasmError> {
	let blob = prepare_blob_for_compilation(blob, semantics)?;
	let dir = tempfile::tempdir().map_err(|e| {
		WasmError::Other(format!(
			"cannot create a directory inside of `std::env::temp_dir()` {}",
			e
		))
	})?;
	let path_temp = dir.path().join("temp.wasm");

	File::create(path_temp.clone())
		.map_err(|e| {
			WasmError::Other(format!("cannot create the file to store runtime artifact: {}", e))
		})?
		.write_all(&blob.serialize())
		.map_err(|e| {
			WasmError::Other(format!("cannot write the runtime blob bytes into the file: {}", e))
		})?;

	wasmedge_sys::Compiler::create(common_config(semantics)?)
		.map_err(|e| {
			WasmError::Other(format!("fail to create a WasmEdge Compiler context: {}", e))
		})?
		.compile(path_temp, compiled_artifact_path)
		.map_err(|e| WasmError::Other(format!("fail to compile the input WASM file: {}", e)))?;

	Ok(())
}

/// # Safety
///
/// This is only unsafe if called with [`CodeSupplyMode::Artifact`]. See
/// [`create_runtime_from_artifact`] to get more details.
unsafe fn do_create_runtime<H>(
	code_supply_mode: CodeSupplyMode<'_>,
	config: Config,
) -> std::result::Result<WasmEdgeRuntime, WasmError>
where
	H: HostFunctions,
{
	println!("vincent debug: do_create_runtime of wasmedge");
	let loader = wasmedge_sys::Loader::create(common_config(&config.semantics)?).map_err(|e| {
		WasmError::Other(format!("fail to create a WasmEdge Loader context: {}", e))
	})?;

	let (module, snapshot_data) = match code_supply_mode {
		CodeSupplyMode::Fresh(blob) => {
			let blob = prepare_blob_for_compilation(blob, &config.semantics)?;
			let serialized_blob = blob.clone().serialize();

			let module = loader.from_bytes(&serialized_blob).map_err(|e| {
				WasmError::Other(format!("fail to create a WasmEdge Module context: {}", e))
			})?;

			if config.semantics.fast_instance_reuse {
				let data_segments_snapshot = DataSegmentsSnapshot::take(&blob).map_err(|e| {
					WasmError::Other(format!("cannot take data segments snapshot: {}", e))
				})?;
				let data_segments_snapshot = Arc::new(data_segments_snapshot);
				let mutable_globals = ExposedMutableGlobalsSet::collect(&blob);

				(module, Some(InstanceSnapshotData { data_segments_snapshot, mutable_globals }))
			} else {
				(module, None)
			}
		},
		CodeSupplyMode::Precompiled(compiled_artifact_path) => {
			let module = loader.from_file(compiled_artifact_path).map_err(|e| {
				WasmError::Other(format!("fail to create a WasmEdge Module context: {}", e))
			})?;

			(module, None)
		},
	};

	let mut vm = Vm::create(common_config(&config.semantics)?, None)
		.map_err(|e| WasmError::Other(format!("fail to create a WasmEdge Vm context: {}", e)))?;

	vm.load_wasm_from_module(&module)
		.map_err(|e| WasmError::Other(format!("fail to load wasm from Module: {}", e)))?;

	Ok(WasmEdgeRuntime {
		vm: Arc::new(Mutex::new(vm)),
		snapshot_data,
		host_functions: H::host_functions(),
		module,
		config,
	})
}

fn common_config(
	semantics: &Semantics,
) -> std::result::Result<Option<wasmedge_sys::Config>, WasmError> {
	let mut wasmedge_config = wasmedge_sys::Config::create().map_err(|e| {
		WasmError::Other(format!("fail to create a WasmEdge Config context: {}", e))
	})?;

	wasmedge_config.set_aot_optimization_level(wasmedge_types::CompilerOptimizationLevel::Os);

	if let Some(max_memory_size) = semantics.max_memory_size {
		wasmedge_config.set_max_memory_pages((max_memory_size / 64 / 1024) as u32);
	}

	// Be clear and specific about the extensions we support. If an update brings new features
	// they should be introduced here as well.
	wasmedge_config.reference_types(false);
	wasmedge_config.simd(false);
	wasmedge_config.bulk_memory_operations(false);
	wasmedge_config.multi_value(false);
	wasmedge_config.threads(false);
	wasmedge_config.memory64(false);

	Ok(Some(wasmedge_config))
}

fn prepare_blob_for_compilation(
	mut blob: RuntimeBlob,
	semantics: &Semantics,
) -> std::result::Result<RuntimeBlob, WasmError> {
	if let Some(DeterministicStackLimit { logical_max }) = semantics.deterministic_stack_limit {
		blob = blob.inject_stack_depth_metering(logical_max)?;
	}

	// If enabled, this should happen after all other passes that may introduce global variables.
	if semantics.fast_instance_reuse {
		blob.expose_mutable_globals();
	}

	// We don't actually need the memory to be imported so we can just convert any memory
	// import into an export with impunity. This simplifies our code since `wasmedge` will
	// now automatically take care of creating the memory for us, and it is also necessary
	// to enable `wasmedge`'s instance pooling. (Imported memories are ineligible for pooling.)
	blob.convert_memory_import_into_export()?;
	blob.add_extra_heap_pages_to_memory_section(
		semantics
			.extra_heap_pages
			.try_into()
			.map_err(|e| WasmError::Other(format!("invalid `extra_heap_pages`: {}", e)))?,
	)?;

	Ok(blob)
}

fn perform_call(
	data: &[u8],
	instance_wrapper: &Arc<Mutex<InstanceWrapper>>,
	method: InvokeMethod,
	mut allocator: FreeingBumpHeapAllocator,
) -> Result<Vec<u8>> {
	let (data_ptr, data_len) = inject_input_data(instance_wrapper, &mut allocator, data)?;

	let host_state = HostState::new(allocator);

	// Set the host state before calling into wasm.
	instance_wrapper
		.lock()
		.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
		.set_host_state(Some(host_state));

	let ret = instance_wrapper
		.lock()
		.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
		.call(method, data_ptr, data_len)
		.map(unpack_ptr_and_len);

	// Reset the host state
	instance_wrapper
		.lock()
		.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
		.set_host_state(None);

	let (output_ptr, output_len) = ret?;
	let output = extract_output_data(instance_wrapper, output_ptr, output_len)?;

	Ok(output)
}

fn inject_input_data(
	instance_wrapper: &Arc<Mutex<InstanceWrapper>>,
	allocator: &mut FreeingBumpHeapAllocator,
	data: &[u8],
) -> Result<(Pointer<u8>, WordSize)> {
	let mut instance_wrapper_locked = instance_wrapper
		.lock()
		.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?;
	let memory_slice = util::memory_slice_mut(instance_wrapper_locked.memory_mut());
	let data_len = data.len() as WordSize;
	let data_ptr = allocator.allocate(memory_slice, data_len)?;
	util::write_memory_from(memory_slice, data_ptr, data)?;
	Ok((data_ptr, data_len))
}

fn extract_output_data(
	instance_wrapper: &Arc<Mutex<InstanceWrapper>>,
	output_ptr: u32,
	output_len: u32,
) -> Result<Vec<u8>> {
	let mut output = vec![0; output_len as usize];
	util::read_memory_into(
		util::memory_slice(
			instance_wrapper
				.lock()
				.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
				.memory(),
		),
		Pointer::new(output_ptr),
		&mut output,
	)?;
	Ok(output)
}
