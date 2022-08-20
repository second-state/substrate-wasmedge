use crate::{
	host::{HostContext, HostState},
	instance_wrapper::InstanceWrapper,
	util,
};
use sc_executor_common::error::WasmError;
use sp_wasm_interface::{Function, ValueType};
use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};
use wasmedge_sys::AsImport;

struct Wrapper {
	host_state: *mut Option<HostState>,
	instance: *mut Option<wasmedge_sys::Instance>,
}

unsafe impl Send for Wrapper {}

/// Goes over all imports of a module and register host functions.
/// Returns an error if there are imports that cannot be satisfied.
pub(crate) fn prepare_imports(
	instance_wrapper: &mut InstanceWrapper,
	module: &wasmedge_sys::Module,
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
			)));
		}

		match import_ty.ty() {
			Ok(wasmedge_types::ExternalInstanceType::Func(func_ty)) => {
				pending_func_imports.insert(name.into_owned(), (import_ty, func_ty));
			},
			_ => {
				return Err(WasmError::Other(format!(
					"host doesn't provide any non function imports: {}:{}",
					import_ty.module_name(),
					name,
				)))
			},
		};
	}

	let mut import = wasmedge_sys::ImportModule::create("env").map_err(|e| {
		WasmError::Other(format!("fail to create a WasmEdge ImportModule context: {}", e))
	})?;

	for (name, (import_ty, func_ty)) in pending_func_imports {
		if let Some(host_func) = host_functions.iter().find(|host_func| host_func.name() == name) {
			let host_func: &'static dyn Function = *host_func;

			let signature = host_func.signature();
			let params = signature.args.iter().cloned().map(into_wasmedge_val_type);
			let results = signature.return_value.iter().cloned().map(into_wasmedge_val_type);

			let host_func_ty = wasmedge_sys::FuncType::create(params.clone(), results.clone())
				.map_err(|e| {
					WasmError::Other(format!("fail to create a WasmEdge FuncType context: {}", e))
				})?;

			// Check that the signature of the host function is the same as the wasm import
			let func_ty_check =
				wasmedge_types::FuncType::new(Some(params.collect()), Some(results.collect()));
			if func_ty != func_ty_check {
				return Err(WasmError::Other(format!(
					"signature mismatch for: {}:{}",
					import_ty.module_name(),
					name,
				)));
			}

			let host_state = instance_wrapper.host_state_ptr();
			let instance = instance_wrapper.instance_ptr();

			let s = Arc::new(Mutex::new(Wrapper { host_state, instance }));
			let returns_len = host_func_ty.returns_len();
			let function_static = move |inputs: Vec<wasmedge_sys::WasmValue>| -> std::result::Result<
				Vec<wasmedge_sys::WasmValue>,
				u8,
			> {
				let mut wrapper = s.lock().unwrap();
				let host_state = unsafe { &mut *(wrapper.host_state) };
				let instance = unsafe { &*(wrapper.instance) };
				let instance = instance.as_ref().unwrap();
				let host_state = host_state.as_mut().unwrap();

				let mut host_context = HostContext::new(
					instance
						.get_memory("memory")
						.map_err(|e| {
							WasmError::Other(format!(
								"failed to get WASM memory named 'memory': {}",
								e,
							))
						})
						.unwrap(),
					instance
						.get_table("__indirect_function_table").ok(),
					host_state,
				);

				let unwind_result = {
					// `from_wasmedge_val` panics if it encounters a value that doesn't fit into the values
					// available in substrate.
					//
					// This, however, cannot happen since the signature of this function is created from
					// a `dyn Function` signature of which cannot have a non substrate value by definition.
					let mut params = inputs.iter().cloned().map(util::from_wasmedge_val);
					
					std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| 
						host_func.execute(&mut host_context, &mut params)
					))
				};
				let execution_result = match unwind_result {
					Ok(execution_result) => execution_result,
					Err(_) => return Err(0),
				};

				match execution_result {
					Ok(Some(ret_val)) => {
						debug_assert!(
							returns_len == 1,
							"wasmedge function signature, therefore the number of results, should always \
							correspond to the number of results returned by the host function",
						);
						Ok(vec![util::into_wasmedge_val(ret_val)])
					},
					Ok(None) => {
						debug_assert!(
							returns_len == 0,
							"wasmedge function signature, therefore the number of results, should always \
							correspond to the number of results returned by the host function",
						);
						Ok(vec![])
					},
					Err(_) => Err(0),
				}
			};

			let func = wasmedge_sys::Function::create_single_thread(&host_func_ty, Box::new(function_static), 0)
				.map_err(|e| {
					WasmError::Other(format!(
						"failed to register host function '{}' into WASM: {}",
						name, e
					))
				})?;
			import.add_func(&name, func);
		} else {
			missing_func_imports.insert(name, (import_ty, func_ty));
		}
	}

	if !missing_func_imports.is_empty() {
		if allow_missing_func_imports {
			for (name, (_, _)) in missing_func_imports {
				let function_static = move |_: Vec<wasmedge_sys::WasmValue>| -> std::result::Result<
					Vec<wasmedge_sys::WasmValue>,
					u8,
				> { Err(0) };
				let func = wasmedge_sys::Function::create_single_thread(
					&wasmedge_sys::FuncType::create([], []).map_err(|e| {
						WasmError::Other(format!(
							"fail to create a WasmEdge FuncType context: {}",
							e
						))
					})?,
					Box::new(function_static),
					0,
				)
				.map_err(|e| {
					WasmError::Other(format!("fail to create a blank Function instance: {}", e))
				})?;
				import.add_func(&name, func);
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
			)));
		}
	}

	let import_obj = wasmedge_sys::ImportObject::Import(import);

	instance_wrapper
		.register_import(import_obj)
		.map_err(|e| WasmError::Other(format!("failed to register import object: {}", e)))?;

	Ok(())
}

fn into_wasmedge_val_type(val_ty: ValueType) -> wasmedge_types::ValType {
	match val_ty {
		ValueType::I32 => wasmedge_types::ValType::I32,
		ValueType::I64 => wasmedge_types::ValType::I64,
		ValueType::F32 => wasmedge_types::ValType::F32,
		ValueType::F64 => wasmedge_types::ValType::F64,
	}
}
