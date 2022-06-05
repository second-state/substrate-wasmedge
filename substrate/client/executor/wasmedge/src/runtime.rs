// This file is part of Substrate.

// Copyright (C) 2019-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

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
use sp_wasm_interface::{HostFunctions, Value};
use sp_wasm_interface::{Pointer, WordSize};
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
	vm_validated: Arc<Mutex<Vm>>,
	snapshot_data: Option<InstanceSnapshotData>,
}

impl WasmModule for WasmEdgeRuntime {
	fn new_instance(&self) -> Result<Box<dyn WasmInstance>> {
		let strategy = if let Some(ref snapshot_data) = self.snapshot_data {
			let mut instance_wrapper = InstanceWrapper::new(Arc::clone(&self.vm_validated))?;
			let heap_base = instance_wrapper.extract_heap_base()?;

			let globals_snapshot = GlobalsSnapshot::take(
				&snapshot_data.mutable_globals,
				&mut InstanceGlobals { instance: &mut instance_wrapper },
			);

			Strategy::FastInstanceReuse {
				instance_wrapper,
				globals_snapshot,
				data_segments_snapshot: snapshot_data.data_segments_snapshot.clone(),
				heap_base,
			}
		} else {
			Strategy::RecreateInstance(InstanceCreator {
				vm_validated: Arc::clone(&self.vm_validated),
			})
		};

		Ok(Box::new(WasmEdgeInstance { strategy }))
	}
}

struct InstanceGlobals<'a> {
	instance: &'a mut InstanceWrapper,
}

impl<'a> runtime_blob::InstanceGlobals for InstanceGlobals<'a> {
	type Global = Arc<Mutex<wasmedge_sys::Global>>;

	fn get_global(&mut self, export_name: &str) -> Self::Global {
		Arc::new(Mutex::new(
			self.instance.get_global(export_name).expect(
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
		instance_wrapper: InstanceWrapper,
		globals_snapshot: GlobalsSnapshot<Arc<Mutex<wasmedge_sys::Global>>>,
		data_segments_snapshot: Arc<DataSegmentsSnapshot>,
		heap_base: u32,
	},
	RecreateInstance(InstanceCreator),
}

struct InstanceCreator {
	vm_validated: Arc<Mutex<Vm>>,
}

impl InstanceCreator {
	fn instantiate(&mut self) -> Result<InstanceWrapper> {
		InstanceWrapper::new(Arc::clone(&self.vm_validated))
	}
}

impl WasmInstance for WasmEdgeInstance {
	fn call(&mut self, method: InvokeMethod, data: &[u8]) -> Result<Vec<u8>> {
		match &mut self.strategy {
			Strategy::FastInstanceReuse {
				ref mut instance_wrapper,
				globals_snapshot,
				data_segments_snapshot,
				heap_base,
			} => {
				data_segments_snapshot.apply(|offset, contents| {
					util::write_memory_from(
						instance_wrapper.memory_slice_mut(),
						Pointer::new(offset),
						contents,
					)
				})?;

				globals_snapshot.apply(&mut InstanceGlobals { instance: instance_wrapper });
				let allocator = FreeingBumpHeapAllocator::new(*heap_base);

				let result = perform_call(data, instance_wrapper, method, allocator);

				instance_wrapper.decommit();

				result
			},
			Strategy::RecreateInstance(ref mut instance_creator) => {
				let mut instance_wrapper = instance_creator.instantiate()?;
				let heap_base = instance_wrapper.extract_heap_base()?;

				let allocator = FreeingBumpHeapAllocator::new(heap_base);

				perform_call(data, &mut instance_wrapper, method, allocator)
			},
		}
	}

	fn get_global_const(&mut self, name: &str) -> Result<Option<Value>> {
		match &mut self.strategy {
			Strategy::FastInstanceReuse { instance_wrapper, .. } => {
				instance_wrapper.get_global_val(name)
			},
			Strategy::RecreateInstance(ref mut instance_creator) => {
				instance_creator.instantiate()?.get_global_val(name)
			},
		}
	}

	fn linear_memory_base_ptr(&self) -> Option<*const u8> {
		match &self.strategy {
			Strategy::RecreateInstance(_) => None,
			Strategy::FastInstanceReuse { instance_wrapper, .. } => {
				Some(instance_wrapper.base_ptr())
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

	crate::imports::prepare_imports::<H>(&mut vm, &module, config.allow_missing_func_imports)?;

	vm.validate()
		.map_err(|e| WasmError::Other(format!("fail to validate the wasm module: {}", e)))?;

	Ok(WasmEdgeRuntime { vm_validated: Arc::new(Mutex::new(vm)), snapshot_data })
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
	instance_wrapper: &mut InstanceWrapper,
	method: InvokeMethod,
	mut allocator: FreeingBumpHeapAllocator,
) -> Result<Vec<u8>> {
	let (data_ptr, data_len) = inject_input_data(instance_wrapper, &mut allocator, data)?;

	let host_state = HostState::new(allocator);

	instance_wrapper.set_host_state(Some(host_state));

	let ret = instance_wrapper.call(method, data_ptr, data_len).map(unpack_ptr_and_len);

	instance_wrapper.set_host_state(None);

	let (output_ptr, output_len) = ret?;
	let output = extract_output_data(instance_wrapper, output_ptr, output_len)?;

	Ok(output)
}

fn inject_input_data(
	instance_wrapper: &mut InstanceWrapper,
	allocator: &mut FreeingBumpHeapAllocator,
	data: &[u8],
) -> Result<(Pointer<u8>, WordSize)> {
	let memory_slice = instance_wrapper.memory_slice_mut();
	let data_len = data.len() as WordSize;
	let data_ptr = allocator.allocate(memory_slice, data_len)?;
	util::write_memory_from(memory_slice, data_ptr, data)?;
	Ok((data_ptr, data_len))
}

fn extract_output_data(
	instance_wrapper: &InstanceWrapper,
	output_ptr: u32,
	output_len: u32,
) -> Result<Vec<u8>> {
	let mut output = vec![0; output_len as usize];
	util::read_memory_into(instance_wrapper.memory_slice(), Pointer::new(output_ptr), &mut output)?;
	Ok(output)
}
