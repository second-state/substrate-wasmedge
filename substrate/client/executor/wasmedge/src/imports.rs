use crate::{
	host::{HostContext, HostState},
	instance_wrapper::InstanceWrapper,
	util,
};
use sc_executor_common::error::WasmError;
use sp_wasm_interface::Function;
use std::{
	collections::HashMap,
	fmt,
	sync::{Arc, Mutex},
};
use wasmedge_sdk::{host_function, Caller, ImportObjectBuilder, Module};
use wasmedge_sys::types::WasmValue;
use wasmedge_types::{error::HostFuncError, ExternalInstanceType, FuncType};

lazy_static::lazy_static! {
	// Stores the data that need to be imported into each host function.
	// The data passed into the host function is a reference, so the
	// original data needs to be stored somewhere.
	//
	// The `Box` is to prevent the element address from changing caused by
	// the expansion of the `Vec`.
	static ref HOST_FUNC_DATA: Arc<Mutex<Vec<Box<HostWrapper>>>> = Arc::new(Mutex::new(vec![]));
}

/// A data struct, to set to the host function context.
struct HostWrapper {
	host_state: *mut Option<HostState>,
	returns_len: usize,
	host_func: &'static dyn Function,
}
unsafe impl Send for HostWrapper {}

/// Goes over all imports of a module and register host functions.
/// Returns an error if there are imports that cannot be satisfied.
pub(crate) fn prepare_imports(
	instance_wrapper: &mut InstanceWrapper,
	module: &Module,
	host_functions: &Vec<&'static dyn Function>,
	allow_missing_func_imports: bool,
) -> Result<(), WasmError> {
	let mut pending_func_imports = HashMap::new();
	let mut missing_func_imports = HashMap::new();

	for import_ty in module.imports() {
		let name = import_ty.name();

		if import_ty.module_name() != "env" {
			return Err(WasmError::Other(format!(
				"host doesn't provide any imports from non-env module: {}:{}",
				import_ty.module_name(),
				name,
			)))
		}

		match import_ty.ty() {
			Ok(ExternalInstanceType::Func(func_ty)) => {
				pending_func_imports.insert(name.into_owned(), (import_ty, func_ty));
			},
			_ =>
				return Err(WasmError::Other(format!(
					"host doesn't provide any non function imports: {}:{}",
					import_ty.module_name(),
					name,
				))),
		};
	}

	let mut import = ImportObjectBuilder::new();

	for (name, (import_ty, func_ty)) in pending_func_imports {
		if let Some(host_func) = host_functions.iter().find(|host_func| host_func.name() == name) {
			let host_func: &'static dyn Function = *host_func;

			let signature = host_func.signature();
			let params = signature.args.iter().cloned().map(util::into_wasmedge_val_type);
			let results = signature.return_value.iter().cloned().map(util::into_wasmedge_val_type);

			let returns_len = results.len();

			// Check that the signature of the host function is the same as the wasm import
			let func_ty_check = FuncType::new(Some(params.collect()), Some(results.collect()));
			if func_ty != func_ty_check {
				return Err(WasmError::Other(format!(
					"signature mismatch for: {}:{}",
					import_ty.module_name(),
					name,
				)))
			}

			#[host_function]
			fn function_static(
				caller: Caller,
				inputs: Vec<WasmValue>,
				host_wrapper: &mut HostWrapper,
			) -> std::result::Result<Vec<WasmValue>, HostFuncError> {
				let instance = caller.instance().expect("wasm instance is always set; qed");

				let host_state = unsafe { &mut *(host_wrapper.host_state) };
				let host_state = host_state.as_mut().expect("host state is always set; qed");

				let mut host_context = HostContext::new(
					instance.memory("memory").expect("memory is always set; qed"),
					instance.table("__indirect_function_table"),
					host_state,
				);
				let unwind_result = {
					// `from_wasmedge_val` panics if it encounters a value that doesn't fit into the
					// values available in substrate.
					//
					// This, however, cannot happen since the signature of this function is created
					// from a `dyn Function` signature of which cannot have a non substrate value by
					// definition.
					let mut params = inputs.iter().cloned().map(util::from_wasmedge_value);

					std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
						host_wrapper.host_func.execute(&mut host_context, &mut params)
					}))
				};
				let execution_result = match unwind_result {
					Ok(execution_result) => execution_result,
					Err(e) => {
						let message = e
							.downcast_ref::<String>()
							.ok_or(HostFuncError::User(HostFuncErrorWasmEdge::Others as u32))?
							.as_str();
						if message.contains("Spawned task") {
							return Err(HostFuncError::User(
								HostFuncErrorWasmEdge::SpawnedTaskErr as u32,
							))
						}
						if message.contains("Failed to allocate memory") {
							return Err(HostFuncError::User(
								HostFuncErrorWasmEdge::AllocateMemoryErr as u32,
							))
						}
						return Err(HostFuncError::User(HostFuncErrorWasmEdge::Others as u32))
					},
				};

				match execution_result {
					Ok(Some(ret_val)) => {
						debug_assert!(
							host_wrapper.returns_len == 1,
							"wasmedge function signature, therefore the number of results, should always \
							correspond to the number of results returned by the host function",
						);
						Ok(vec![util::into_wasmedge_value(ret_val)])
					},
					Ok(None) => {
						debug_assert!(
							host_wrapper.returns_len == 0,
							"wasmedge function signature, therefore the number of results, should always \
							correspond to the number of results returned by the host function",
						);
						Ok(vec![])
					},
					Err(_) => Err(HostFuncError::User(HostFuncErrorWasmEdge::Others as u32)),
				}
			}

			let host_state = instance_wrapper.host_state_ptr();

			let mut host_wrapper = Box::new(HostWrapper { host_state, returns_len, host_func });

			import = import
				.with_func_by_type(&name, func_ty, function_static, Some(host_wrapper.as_mut()))
				.map_err(|e| {
					WasmError::Other(format!(
						"failed to register host function '{}' into WASM: {}",
						name, e
					))
				})?;

			HOST_FUNC_DATA
				.lock()
				.map_err(|_| WasmError::Other("failed to lock the HOST_FUNC_DATA".to_string()))?
				.push(host_wrapper);
		} else {
			missing_func_imports.insert(name, (import_ty, func_ty));
		}
	}

	if !missing_func_imports.is_empty() {
		if allow_missing_func_imports {
			for (name, (_, _)) in missing_func_imports {
				#[host_function]
				fn function_static(
					_: Caller,
					_: Vec<WasmValue>,
				) -> std::result::Result<Vec<WasmValue>, HostFuncError> {
					Err(HostFuncError::User(HostFuncErrorWasmEdge::MissingHostFunc as u32))
				}

				import =
					import.with_func::<(), (), !>(&name, function_static, None).map_err(|e| {
						WasmError::Other(format!("fail to create a blank Function instance: {}", e))
					})?;
			}
		} else {
			let mut names = Vec::new();
			for (name, (import_ty, _)) in missing_func_imports {
				names.push(format!("'{}:{}'", import_ty.module_name(), name));
			}
			let names = names.join(", ");
			return Err(WasmError::Other(format!(
				"runtime requires function imports which are not present on the host: {}",
				names
			)))
		}
	}

	let import_obj = import
		.build("env")
		.map_err(|e| WasmError::Other(format!("fail to create a WasmEdge import object: {}", e)))?;

	instance_wrapper
		.register_import(import_obj)
		.map_err(|e| WasmError::Other(format!("failed to register import object: {}", e)))?;

	Ok(())
}

pub enum HostFuncErrorWasmEdge {
	MissingHostFunc = 1,
	AllocateMemoryErr = 2,
	SpawnedTaskErr = 3,
	Others = 4,
}

impl fmt::Display for HostFuncErrorWasmEdge {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			HostFuncErrorWasmEdge::MissingHostFunc => write!(f, "1"),
			HostFuncErrorWasmEdge::AllocateMemoryErr => write!(f, "2"),
			HostFuncErrorWasmEdge::SpawnedTaskErr => write!(f, "3"),
			HostFuncErrorWasmEdge::Others => write!(f, "4"),
		}
	}
}
