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
use std::sync::{Arc, Mutex};
use wasmedge_sys::Vm;

pub struct Config {
	pub max_memory_size: Option<usize>,
	pub heap_pages: u32,
	pub allow_missing_func_imports: bool,
	pub semantics: Semantics,
}

pub struct Semantics {
	pub fast_instance_reuse: bool,
	pub extra_heap_pages: u64,
}

struct InstanceSnapshotData {
	mutable_globals: ExposedMutableGlobalsSet,
	data_segments_snapshot: Arc<DataSegmentsSnapshot>,
}

pub struct WasmEdgeRuntime {
	vm: Arc<Mutex<Vm>>,
	snapshot_data: Option<InstanceSnapshotData>,
	host_functions: Vec<&'static dyn Function>,
	module: wasmedge_sys::Module,
	allow_missing_func_imports: bool,
}

impl WasmModule for WasmEdgeRuntime {
	fn new_instance(&self) -> Result<Box<dyn WasmInstance>> {
		let instance_wrapper = InstanceWrapper::new(Arc::clone(&self.vm));

		crate::imports::prepare_imports(
			Arc::clone(&instance_wrapper),
			&self.module,
			&self.host_functions,
			self.allow_missing_func_imports,
		)
		.map_err(|e| WasmError::Other(format!("fail to register imports: {}", e)))?;

		let strategy = if let Some(ref snapshot_data) = self.snapshot_data {
			instance_wrapper.lock().unwrap().instantiate()?;

			let heap_base = instance_wrapper.lock().unwrap().extract_heap_base()?;

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
			self.instance.lock().unwrap().get_global(export_name).expect(
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
		self.instance_wrapper.lock().unwrap().instantiate()?;
		Ok(())
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
						instance_wrapper.lock().unwrap().memory_slice_mut(),
						Pointer::new(offset),
						contents,
					)
				})?;

				globals_snapshot
					.apply(&mut InstanceGlobals { instance: Arc::clone(instance_wrapper) });
				let allocator = FreeingBumpHeapAllocator::new(*heap_base);

				let result = perform_call(data, Arc::clone(instance_wrapper), method, allocator);

				instance_wrapper.lock().unwrap().decommit();

				result
			},
			Strategy::RecreateInstance(instance_creator) => {
				instance_creator.instantiate()?;
				let heap_base =
					instance_creator.instance_wrapper.lock().unwrap().extract_heap_base()?;

				let allocator = FreeingBumpHeapAllocator::new(heap_base);

				perform_call(
					data,
					Arc::clone(&instance_creator.instance_wrapper),
					method,
					allocator,
				)
			},
		}
	}

	fn get_global_const(&mut self, name: &str) -> Result<Option<Value>> {
		match &mut self.strategy {
			Strategy::FastInstanceReuse { instance_wrapper, .. } => {
				instance_wrapper.lock().unwrap().get_global_val(name)
			},
			Strategy::RecreateInstance(ref mut instance_creator) => {
				instance_creator.instantiate()?;
				instance_creator.instance_wrapper.lock().unwrap().get_global_val(name)
			},
		}
	}

	fn linear_memory_base_ptr(&self) -> Option<*const u8> {
		match &self.strategy {
			Strategy::RecreateInstance(_) => None,
			Strategy::FastInstanceReuse { instance_wrapper, .. } => {
				Some(instance_wrapper.lock().unwrap().base_ptr())
			},
		}
	}
}

pub fn create_runtime<H>(
	blob: RuntimeBlob,
	config: Config,
) -> std::result::Result<WasmEdgeRuntime, WasmError>
where
	H: HostFunctions,
{
	let snapshot_data = if config.semantics.fast_instance_reuse {
		let data_segments_snapshot = DataSegmentsSnapshot::take(&blob)
			.map_err(|e| WasmError::Other(format!("cannot take data segments snapshot: {}", e)))?;
		let data_segments_snapshot = Arc::new(data_segments_snapshot);
		let mutable_globals = ExposedMutableGlobalsSet::collect(&blob);

		Some(InstanceSnapshotData { data_segments_snapshot, mutable_globals })
	} else {
		None
	};

	let blob = prepare_blob_for_compilation(blob, &config.semantics)?;
	let serialized_blob = blob.serialize();

	let loader = wasmedge_sys::Loader::create(Some(common_config(&config)?)).map_err(|e| {
		WasmError::Other(format!("fail to create a WasmEdge Loader context: {}", e))
	})?;

	let module = loader.from_bytes(&serialized_blob).map_err(|e| {
		WasmError::Other(format!("fail to create a WasmEdge Module context: {}", e))
	})?;

	let mut vm = Vm::create(Some(common_config(&config)?), None)
		.map_err(|e| WasmError::Other(format!("fail to create a WasmEdge Vm context: {}", e)))?;

	vm.load_wasm_from_module(&module)
		.map_err(|e| WasmError::Other(format!("fail to load wasm from Module: {}", e)))?;

	// crate::imports::prepare_imports::<H>(&mut vm, &module, config.allow_missing_func_imports)?;

	// vm.validate()
	// 	.map_err(|e| WasmError::Other(format!("fail to validate the wasm module: {}", e)))?;

	Ok(WasmEdgeRuntime {
		vm: Arc::new(Mutex::new(vm)),
		snapshot_data,
		host_functions: H::host_functions(),
		module,
		allow_missing_func_imports: config.allow_missing_func_imports,
	})
}

fn common_config(config: &Config) -> std::result::Result<wasmedge_sys::Config, WasmError> {
	let mut wasmedge_config = wasmedge_sys::Config::create().map_err(|e| {
		WasmError::Other(format!("fail to create a WasmEdge Config context: {}", e))
	})?;

	if let Some(max_memory_size) = config.max_memory_size {
		wasmedge_config.set_max_memory_pages((max_memory_size / 64 / 1024) as u32);
	}

	wasmedge_config.reference_types(false);
	wasmedge_config.simd(false);
	wasmedge_config.bulk_memory_operations(false);
	wasmedge_config.multi_value(false);
	wasmedge_config.threads(false);
	wasmedge_config.memory64(false);

	Ok(wasmedge_config)
}

fn prepare_blob_for_compilation(
	mut blob: RuntimeBlob,
	semantics: &Semantics,
) -> std::result::Result<RuntimeBlob, WasmError> {
	if semantics.fast_instance_reuse {
		blob.expose_mutable_globals();
	}

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
	instance_wrapper: Arc<Mutex<InstanceWrapper>>,
	method: InvokeMethod,
	mut allocator: FreeingBumpHeapAllocator,
) -> Result<Vec<u8>> {
	let data_len = data.len() as WordSize;
	let data_ptr = allocator
		.allocate(Arc::clone(&instance_wrapper).lock().unwrap().memory_slice_mut(), data_len)?;
	util::write_memory_from(
		Arc::clone(&instance_wrapper).lock().unwrap().memory_slice_mut(),
		data_ptr,
		data,
	)?;

	let host_state = HostState::new(allocator);

	Arc::clone(&instance_wrapper).lock().unwrap().set_host_state(Some(host_state));

	let ret = instance_wrapper
		.lock()
		.unwrap()
		.call(method, data_ptr, data_len)
		.map(unpack_ptr_and_len);

	instance_wrapper.lock().unwrap().set_host_state(None);

	let (output_ptr, output_len) = ret?;

	let mut output = vec![0; output_len as usize];
	util::read_memory_into(
		instance_wrapper.lock().unwrap().memory_slice(),
		Pointer::new(output_ptr),
		&mut output,
	)?;

	Ok(output)
}

// fn inject_input_data(
// 	instance_wrapper: Arc<Mutex<InstanceWrapper>>,
// 	allocator: &mut FreeingBumpHeapAllocator,
// 	data: &[u8],
// ) -> Result<(Pointer<u8>, WordSize)> {
// 	let memory_slice = instance_wrapper.lock().unwrap().memory_slice_mut();
// 	let data_len = data.len() as WordSize;
// 	let data_ptr = allocator.allocate(memory_slice, data_len)?;
// 	util::write_memory_from(memory_slice, data_ptr, data)?;
// 	Ok((data_ptr, data_len))
// }

// fn extract_output_data(
// 	instance_wrapper: Arc<Mutex<InstanceWrapper>>,
// 	output_ptr: u32,
// 	output_len: u32,
// ) -> Result<Vec<u8>> {
// 	let mut output = vec![0; output_len as usize];
// 	util::read_memory_into(instance_wrapper.lock().unwrap().memory_slice(), Pointer::new(output_ptr), &mut output)?;
// 	Ok(output)
// }
