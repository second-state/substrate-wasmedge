use crate::{host::HostState, util};
use sc_executor_common::{
	error::{Backtrace, Error, MessageWithBacktrace, Result, WasmError},
	wasm_runtime::InvokeMethod,
};
use sp_wasm_interface::{Pointer, Value, WordSize};
use wasmedge_sdk::{
	types::Val, Executor, Func, FuncRef, ImportObject, Instance, Memory, Module, Store,
};
use wasmedge_sys::types::WasmValue;
use wasmedge_types::ValType;

pub struct InstanceWrapper {
	store: Store,
	executor: Executor,
	instance: Option<Instance>,
	memory: Option<Memory>,
	host_state: Option<HostState>,
	import: Option<ImportObject>,
}

impl InstanceWrapper {
	pub fn new(semantics: &crate::runtime::Semantics) -> Result<Self> {
		let executor = Executor::new(Some(&crate::runtime::common_config(semantics)?), None)
			.map_err(|e| {
				WasmError::Other(format!("fail to create a WasmEdge Executor context: {}", e))
			})?;

		let store = Store::new().map_err(|e| {
			WasmError::Other(format!("fail to create a WasmEdge Store context: {}", e))
		})?;

		Ok(InstanceWrapper {
			store,
			executor,
			instance: None,
			memory: None,
			host_state: None,
			import: None,
		})
	}

	pub fn register_import(&mut self, import_obj: ImportObject) -> Result<()> {
		self.import = Some(import_obj);
		self.store
			.register_import_module(&mut self.executor, &self.import.as_ref().unwrap())
			.map_err(|error| {
				WasmError::Other(format!("failed to register import object: {}", error,))
			})?;
		Ok(())
	}

	pub fn instantiate(&mut self, module: &Module) -> Result<()> {
		let instance = self
			.store
			.register_active_module(&mut self.executor, &module)
			.map_err(|e| WasmError::Other(format!("failed to register active module: {}", e,)))?;

		let memory = instance
			.memory("memory")
			.ok_or(WasmError::Other(String::from("fail to get WASM memory named 'memory'")))?;

		self.instance = Some(instance);
		self.memory = Some(memory);
		Ok(())
	}

	pub fn call(
		&mut self,
		method: InvokeMethod,
		data_ptr: Pointer<u8>,
		data_len: WordSize,
	) -> Result<u64> {
		let data_ptr = WasmValue::from_i32(u32::from(data_ptr) as i32);
		let data_len = WasmValue::from_i32(u32::from(data_len) as i32);

		let res = match method {
			InvokeMethod::Export(method) => {
				let func = self
					.instance()
					.func(method)
					.ok_or(WasmError::Other(String::from("function is not found")))?;

				check_signature1(&func)?;

				func.call(&mut self.executor, vec![data_ptr, data_len])
			},
			InvokeMethod::Table(func) => {
				let table =
					self.instance().table("__indirect_function_table").ok_or(Error::NoTable)?;

				let func_ref =
					match table.get(func).map_err(|_| Error::NoTableEntryWithIndex(func))? {
						Val::FuncRef(Some(func_ref)) => func_ref,
						_ => {
							return Err(Error::FunctionRefIsNull(func));
						},
					};

				check_signature2(&func_ref)?;

				func_ref.call(&mut self.executor, vec![data_ptr, data_len])
			},
			InvokeMethod::TableWithWrapper { dispatcher_ref, func } => {
				let table =
					self.instance().table("__indirect_function_table").ok_or(Error::NoTable)?;

				let func_ref = match table
					.get(dispatcher_ref)
					.map_err(|_| Error::NoTableEntryWithIndex(dispatcher_ref))?
				{
					Val::FuncRef(Some(func_ref)) => func_ref,
					_ => {
						return Err(Error::FunctionRefIsNull(dispatcher_ref));
					},
				};

				check_signature3(&func_ref)?;

				func_ref.call(
					&mut self.executor,
					vec![WasmValue::from_i32(func as i32), data_ptr, data_len],
				)
			},
		}
		.map_err(|trap| {
			let host_state = self.host_state_mut();

			// The logic to print out a backtrace is somewhat complicated,
			// so let's get wasmtime to print it out for us.
			let mut backtrace_string = trap.to_string();
			let suffix = "\nwasm backtrace:";
			if let Some(index) = backtrace_string.find(suffix) {
				// Get rid of the error message and just grab the backtrace,
				// since we're storing the error message ourselves separately.
				backtrace_string.replace_range(0..index + suffix.len(), "");
			}

			let backtrace = Backtrace { backtrace_string };
			if let Some(error) = host_state.take_panic_message() {
				Error::AbortedDueToPanic(MessageWithBacktrace {
					message: error,
					backtrace: Some(backtrace),
				})
			} else {
				Error::AbortedDueToTrap(MessageWithBacktrace {
					message: trap.to_string(),
					backtrace: Some(backtrace),
				})
			}
		})?;

		Ok(res[0].to_i64() as u64)
	}

	/// Reads `__heap_base: i32` global variable and returns it.
	///
	/// If it doesn't exist, not a global or of not i32 type returns an error.
	pub fn extract_heap_base(&mut self) -> Result<u32> {
		let heap_base = self
			.instance()
			.global("__heap_base")
			.ok_or(WasmError::Other(String::from("failed to get WASM global named '__heap_base'")))?
			.get_value();

		if let Val::I32(v) = heap_base {
			Ok(v as u32)
		} else {
			Err(Error::Other(String::from(
				"the type of WASM global named '__heap_base' is not i32",
			)))
		}
	}

	/// Get the value from a global with the given `name`.
	pub fn get_global_val(&mut self, name: &str) -> Result<Option<Value>> {
		let global = self
			.instance()
			.global(name)
			.ok_or(Error::Other(String::from("failed to get WASM global")))?
			.get_value();

		match global {
			Val::I32(v) => Ok(Some(Value::I32(v))),
			Val::I64(v) => Ok(Some(Value::I64(v))),
			Val::F32(v) => Ok(Some(Value::F32(v as u32))),
			Val::F64(v) => Ok(Some(Value::F64(v as u64))),
			_ => Err("Unknown value type".into()),
		}
	}

	/// Returns the pointer to the first byte of the linear memory for this instance.
	pub fn base_ptr(&self) -> *const u8 {
		self.memory()
			.data_pointer(0, 1)
			.expect("failed to returns the const data pointer to the Memory.")
	}

	pub(crate) fn memory(&self) -> &Memory {
		self.memory.as_ref().expect("memory is always set; qed")
	}

	pub(crate) fn memory_mut(&mut self) -> &mut Memory {
		self.memory.as_mut().expect("memory is always set; qed")
	}

	pub(crate) fn instance(&self) -> &Instance {
		self.instance.as_ref().expect("wasmedge instance is always set; qed")
	}

	pub fn host_state_mut(&mut self) -> &mut HostState {
		self.host_state
			.as_mut()
			.expect("host state is not empty when calling a function in wasm; qed")
	}

	pub fn host_state_ptr(&mut self) -> *mut Option<HostState> {
		&mut self.host_state as *mut Option<HostState>
	}

	pub fn instance_ptr(&mut self) -> *mut Option<Instance> {
		&mut self.instance as *mut Option<Instance>
	}

	pub fn set_host_state(&mut self, host_state: Option<HostState>) {
		self.host_state = host_state;
	}

	pub fn take_host_state(&mut self) -> Option<HostState> {
		self.host_state.take()
	}

	/// If possible removes physical backing from the allocated linear memory which
	/// leads to returning the memory back to the system; this also zeroes the memory
	/// as a side-effect.
	pub fn decommit(&mut self) {
		if self.memory().size() == 0 {
			return;
		}

		cfg_if::cfg_if! {
			if #[cfg(target_os = "linux")] {
				use std::sync::Once;

				unsafe {
					let ptr = self.base_ptr();
					let len = (self.memory().size() * 64 * 1024) as usize;

					// Linux handles MADV_DONTNEED reliably. The result is that the given area
					// is unmapped and will be zeroed on the next pagefault.
					if libc::madvise(ptr as _, len, libc::MADV_DONTNEED) != 0 {
						static LOGGED: Once = Once::new();
						LOGGED.call_once(|| {
							log::warn!(
								"madvise(MADV_DONTNEED) failed: {}",
								std::io::Error::last_os_error(),
							);
						});
					} else {
						return;
					}
				}
			} else if #[cfg(target_os = "macos")] {
				use std::sync::Once;

				unsafe {
					let ptr = self.base_ptr();
					let len = (self.memory().size() * 64 * 1024) as usize;

					if libc::mmap(
						ptr as _,
						len,
						libc::PROT_READ | libc::PROT_WRITE,
						libc::MAP_FIXED | libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
						-1,
						0,
					) == libc::MAP_FAILED {
						static LOGGED: Once = Once::new();
						LOGGED.call_once(|| {
							log::warn!(
								"Failed to decommit WASM instance memory through mmap: {}",
								std::io::Error::last_os_error(),
							);
						});
					} else {
						return;
					}
				}
			}
		}

		// If we're on an unsupported OS or the memory couldn't have been
		// decommited for some reason then just manually zero it out.
		util::memory_slice_mut(self.memory_mut()).fill(0);
	}
}

fn check_signature1(func: &Func) -> Result<()> {
	let func_type = func
		.ty()
		.map_err(|error| WasmError::Other(format!("fail to get the function type: {}", error,)))?;

	let params = func_type.args().unwrap_or(&[]);
	let returns = func_type.returns().unwrap_or(&[]);

	if params != [ValType::I32, ValType::I32] || returns != [ValType::I64] {
		return Err(Error::Other("Invalid signature for direct entry point".to_string()));
	}
	Ok(())
}

fn check_signature2(func_ref: &FuncRef) -> Result<()> {
	let func_type = func_ref
		.ty()
		.map_err(|error| WasmError::Other(format!("fail to get the function type: {}", error,)))?;

	let params = func_type.args().unwrap_or(&[]);
	let returns = func_type.returns().unwrap_or(&[]);

	if params != vec![ValType::I32, ValType::I32] || returns != [ValType::I64] {
		return Err(Error::Other("Invalid signature for direct entry point".to_string()));
	}
	Ok(())
}

fn check_signature3(func_ref: &FuncRef) -> Result<()> {
	let func_type = func_ref
		.ty()
		.map_err(|error| WasmError::Other(format!("fail to get the function type: {}", error,)))?;

	let params = func_type.args().unwrap_or(&[]);
	let returns = func_type.returns().unwrap_or(&[]);

	if params != vec![ValType::I32, ValType::I32, ValType::I32] || returns != [ValType::I64] {
		return Err(Error::Other("Invalid signature for direct entry point".to_string()));
	}
	Ok(())
}
