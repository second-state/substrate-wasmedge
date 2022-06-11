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

use log::trace;
use wasmedge_sys::Vm;

use codec::{Decode, Encode};
use sc_allocator::FreeingBumpHeapAllocator;
use sc_executor_common::{
	error::Result,
	sandbox::{self, SupervisorFuncIndex},
	util::MemoryTransfer,
};
use sp_sandbox::env as sandbox_env;
use sp_wasm_interface::{FunctionContext, MemoryId, Pointer, Sandbox, WordSize};

use crate::{instance_wrapper::InstanceWrapper, util};

use std::sync::Arc;

use sc_executor_common::error::WasmError;

struct SandboxStore(Option<Box<sandbox::Store<Arc<wasmedge_sys::FuncRef>>>>);

unsafe impl Send for SandboxStore {}

pub struct HostState {
	sandbox_store: SandboxStore,
	allocator: FreeingBumpHeapAllocator,
	panic_message: Option<String>,
}

impl HostState {
	pub fn new(allocator: FreeingBumpHeapAllocator) -> Self {
		HostState {
			sandbox_store: SandboxStore(Some(Box::new(sandbox::Store::new(
				sandbox::SandboxBackend::TryWasmer,
			)))),
			allocator,
			panic_message: None,
		}
	}

	pub fn take_panic_message(&mut self) -> Option<String> {
		self.panic_message.take()
	}
}

pub(crate) struct HostContext {
	instance_wrapper: InstanceWrapper,
}

impl sp_wasm_interface::FunctionContext for HostContext {
	fn read_memory_into(
		&self,
		address: Pointer<u8>,
		dest: &mut [u8],
	) -> sp_wasm_interface::Result<()> {
		util::read_memory_into(self.instance_wrapper.memory_slice(), address, dest)
			.map_err(|e| e.to_string())
	}

	fn write_memory(&mut self, address: Pointer<u8>, data: &[u8]) -> sp_wasm_interface::Result<()> {
		util::write_memory_from(self.instance_wrapper.memory_slice_mut(), address, data)
			.map_err(|e| e.to_string())
	}

	fn allocate_memory(&mut self, size: WordSize) -> sp_wasm_interface::Result<Pointer<u8>> {
		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.allocator
			.allocate(self.instance_wrapper.memory_slice_mut(), size)
			.map_err(|e| e.to_string())
	}

	fn deallocate_memory(&mut self, ptr: Pointer<u8>) -> sp_wasm_interface::Result<()> {
		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.allocator
			.deallocate(self.instance_wrapper.memory_slice_mut(), ptr)
			.map_err(|e| e.to_string())
	}

	fn sandbox(&mut self) -> &mut dyn Sandbox {
		self
	}

	fn register_panic_error_message(&mut self, message: &str) {
		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.panic_message = Some(message.to_owned());
	}
}

impl Sandbox for HostContext {
	fn memory_get(
		&mut self,
		memory_id: MemoryId,
		offset: WordSize,
		buf_ptr: Pointer<u8>,
		buf_len: WordSize,
	) -> sp_wasm_interface::Result<u32> {
		let sandboxed_memory = self
			.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_ref()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_ref()
			.expect("sandbox store is only empty when temporarily borrowed")
			.memory(memory_id)
			.map_err(|e| e.to_string())?;

		let len = buf_len as usize + offset as usize;

		let buffer = match sandboxed_memory.read(Pointer::new(offset as u32), len) {
			Err(_) => return Ok(sandbox_env::ERR_OUT_OF_BOUNDS),
			Ok(buffer) => buffer,
		};

		if util::write_memory_from(self.instance_wrapper.memory_slice_mut(), buf_ptr, &buffer)
			.is_err()
		{
			return Ok(sandbox_env::ERR_OUT_OF_BOUNDS);
		}

		Ok(sandbox_env::ERR_OK)
	}

	fn memory_set(
		&mut self,
		memory_id: MemoryId,
		offset: WordSize,
		val_ptr: Pointer<u8>,
		val_len: WordSize,
	) -> sp_wasm_interface::Result<u32> {
		let sandboxed_memory = self
			.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_ref()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_ref()
			.expect("sandbox store is only empty when temporarily borrowed")
			.memory(memory_id)
			.map_err(|e| e.to_string())?;

		let len = val_len as usize;

		let buffer = match util::read_memory(self.instance_wrapper.memory_slice(), val_ptr, len) {
			Err(_) => return Ok(sandbox_env::ERR_OUT_OF_BOUNDS),
			Ok(buffer) => buffer,
		};

		if sandboxed_memory.write_from(Pointer::new(offset as u32), &buffer).is_err() {
			return Ok(sandbox_env::ERR_OUT_OF_BOUNDS);
		}

		Ok(sandbox_env::ERR_OK)
	}

	fn memory_teardown(&mut self, memory_id: MemoryId) -> sp_wasm_interface::Result<()> {
		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_mut()
			.expect("sandbox store is only empty when temporarily borrowed")
			.memory_teardown(memory_id)
			.map_err(|e| e.to_string())
	}

	fn memory_new(&mut self, initial: u32, maximum: u32) -> sp_wasm_interface::Result<u32> {
		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_mut()
			.expect("sandbox store is only empty when temporarily borrowed")
			.new_memory(initial, maximum)
			.map_err(|e| e.to_string())
	}

	fn invoke(
		&mut self,
		instance_id: u32,
		export_name: &str,
		mut args: &[u8],
		return_val: Pointer<u8>,
		return_val_len: u32,
		state: u32,
	) -> sp_wasm_interface::Result<u32> {
		trace!(target: "sp-sandbox", "invoke, instance_idx={}", instance_id);

		let args = Vec::<sp_wasm_interface::Value>::decode(&mut args)
			.map_err(|_| "Can't decode serialized arguments for the invocation")?
			.into_iter()
			.collect::<Vec<_>>();

		let instance = self
			.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_ref()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_ref()
			.expect("sandbox store is only empty when temporarily borrowed")
			.instance(instance_id)
			.map_err(|e| e.to_string())?;

		let dispatch_thunk = self
			.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_ref()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_ref()
			.expect("sandbox store is only empty when temporarily borrowed")
			.dispatch_thunk(instance_id)
			.map_err(|e| e.to_string())?;

		let result = instance.invoke(
			export_name,
			&args,
			state,
			&mut SandboxContext { host_context: self, dispatch_thunk },
		);

		match result {
			Ok(None) => Ok(sandbox_env::ERR_OK),
			Ok(Some(val)) => {
				sp_wasm_interface::ReturnValue::Value(val.into()).using_encoded(|val| {
					if val.len() > return_val_len as usize {
						return Err("Return value buffer is too small".into());
					}
					<HostContext as FunctionContext>::write_memory(self, return_val, val)
						.map_err(|_| "can't write return value")?;
					Ok(sandbox_env::ERR_OK)
				})
			},
			Err(_) => Ok(sandbox_env::ERR_EXECUTION),
		}
	}

	fn instance_teardown(&mut self, instance_id: u32) -> sp_wasm_interface::Result<()> {
		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_mut()
			.expect("sandbox store is only empty when temporarily borrowed")
			.instance_teardown(instance_id)
			.map_err(|e| e.to_string())
	}

	fn instance_new(
		&mut self,
		dispatch_thunk_id: u32,
		wasm: &[u8],
		raw_env_def: &[u8],
		state: u32,
	) -> sp_wasm_interface::Result<u32> {
		let dispatch_thunk = {
			let table = self
				.instance_wrapper
				.instance()
				.get_table("__indirect_function_table")
				.map_err(|error| {
					WasmError::Other(format!(
						"table named '__indirect_function_table' is not found: {}",
						error,
					))
				})
				.unwrap();

			table
				.get_data(dispatch_thunk_id)
				.map_err(|error| WasmError::Other(format!("failed to get the data: {}", error,)))
				.unwrap()
				.func_ref()
		};

		let dispatch_thunk = Arc::new(dispatch_thunk.unwrap());

		let guest_env = match sandbox::GuestEnvironment::decode(
			self.instance_wrapper
				.host_state()
				.lock()
				.expect("failed to lock; qed")
				.as_mut()
				.expect("host state is not empty when calling a function in wasm; qed")
				.sandbox_store
				.0
				.as_ref()
				.expect("sandbox store is only empty when temporarily borrowed"),
			raw_env_def,
		) {
			Ok(guest_env) => guest_env,
			Err(_) => return Ok(sandbox_env::ERR_MODULE as u32),
		};

		let mut store = self
			.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.take()
			.expect("sandbox store is only empty when borrowed");

		let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
			store.instantiate(
				wasm,
				guest_env,
				state,
				&mut SandboxContext { host_context: self, dispatch_thunk: dispatch_thunk.clone() },
			)
		}));

		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0 = Some(store);

		let result = match result {
			Ok(result) => result,
			Err(error) => std::panic::resume_unwind(error),
		};

		let instance_idx_or_err_code = match result {
			Ok(instance) => instance.register(
				self.instance_wrapper
					.host_state()
					.lock()
					.expect("failed to lock; qed")
					.as_mut()
					.expect("host state is not empty when calling a function in wasm; qed")
					.sandbox_store
					.0
					.as_mut()
					.expect("sandbox store is only empty when temporarily borrowed"),
				dispatch_thunk.clone(),
			),
			Err(sandbox::InstantiationError::StartTrapped) => sandbox_env::ERR_EXECUTION,
			Err(_) => sandbox_env::ERR_MODULE,
		};

		Ok(instance_idx_or_err_code as u32)
	}

	fn get_global_val(
		&self,
		instance_idx: u32,
		name: &str,
	) -> sp_wasm_interface::Result<Option<sp_wasm_interface::Value>> {
		self.instance_wrapper
			.host_state()
			.lock()
			.expect("failed to lock; qed")
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
			.sandbox_store
			.0
			.as_ref()
			.expect("sandbox store is only empty when temporarily borrowed")
			.instance(instance_idx)
			.map(|i| i.get_global_val(name))
			.map_err(|e| e.to_string())
	}
}

struct SandboxContext<'a> {
	host_context: &'a mut HostContext,
	dispatch_thunk: Arc<wasmedge_sys::FuncRef>,
}

impl<'a> sandbox::SandboxContext for SandboxContext<'a> {
	fn invoke(
		&mut self,
		invoke_args_ptr: Pointer<u8>,
		invoke_args_len: WordSize,
		state: u32,
		func_idx: SupervisorFuncIndex,
	) -> Result<i64> {
		let mut executor = wasmedge_sys::Executor::create(None, None).map_err(|e| {
			WasmError::Other(format!("fail to create a WasmEdge Executor context: {}", e))
		})?;

		let result = self.dispatch_thunk.call(
			&mut executor,
			vec![
				wasmedge_sys::WasmValue::from_i32(u32::from(invoke_args_ptr) as i32),
				wasmedge_sys::WasmValue::from_i32(invoke_args_len as i32),
				wasmedge_sys::WasmValue::from_i32(state as i32),
				wasmedge_sys::WasmValue::from_i32(usize::from(func_idx) as i32),
			],
		);

		match result {
			Ok(result) => Ok(result[0].to_i64()),
			Err(err) => Err(err.to_string().into()),
		}
	}

	fn supervisor_context(&mut self) -> &mut dyn FunctionContext {
		self.host_context
	}
}
