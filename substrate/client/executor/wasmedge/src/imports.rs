use crate::{
	host::{HostContext, HostState},
	instance_wrapper::InstanceWrapper,
	util,
};
use sc_executor_common::error::WasmError;
use sp_wasm_interface::Function;
use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};
use wasmedge_sdk::{CallingFrame, ImportObjectBuilder, Instance, Module};
use wasmedge_sys::types::WasmValue;
use wasmedge_types::{error::HostFuncError, ExternalInstanceType, FuncType};

struct Wrapper {
	host_state: *mut Option<HostState>,
	instance: *mut Option<Instance>,
}

unsafe impl Send for Wrapper {}

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
			)));
		}

		match import_ty.ty() {
			Ok(ExternalInstanceType::Func(func_ty)) => {
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
				)));
			}

			let host_state = instance_wrapper.host_state_ptr();
			let instance = instance_wrapper.instance_ptr();

			let s = Arc::new(Mutex::new(Wrapper { host_state, instance }));

			let function_static = move |_: &CallingFrame,
			                            inputs: Vec<WasmValue>|
			      -> std::result::Result<Vec<WasmValue>, HostFuncError> {
				let mut wrapper = s.lock().unwrap();
				let host_state = unsafe { &mut *(wrapper.host_state) };
				let instance = unsafe { &*(wrapper.instance) };
				let instance = instance.as_ref().unwrap();
				let host_state = host_state.as_mut().unwrap();

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
						host_func.execute(&mut host_context, &mut params)
					}))
				};
				let execution_result = match unwind_result {
					Ok(execution_result) => execution_result,
					Err(_) => return Err(HostFuncError::User(1)),
				};

				match execution_result {
					Ok(Some(ret_val)) => {
						debug_assert!(
							returns_len == 1,
							"wasmedge function signature, therefore the number of results, should always \
							correspond to the number of results returned by the host function",
						);
						Ok(vec![util::into_wasmedge_value(ret_val)])
					},
					Ok(None) => {
						debug_assert!(
							returns_len == 0,
							"wasmedge function signature, therefore the number of results, should always \
							correspond to the number of results returned by the host function",
						);
						Ok(vec![])
					},
					Err(_) => Err(HostFuncError::User(1)),
				}
			};

			import = import.with_func_by_type(&name, func_ty, function_static).map_err(|e| {
				WasmError::Other(format!(
					"failed to register host function '{}' into WASM: {}",
					name, e
				))
			})?;
		} else {
			missing_func_imports.insert(name, (import_ty, func_ty));
		}
	}

	if !missing_func_imports.is_empty() {
		if allow_missing_func_imports {
			for (name, (_, _)) in missing_func_imports {
				let function_static = move |_: &CallingFrame,
				                            _: Vec<WasmValue>|
				      -> std::result::Result<
					Vec<WasmValue>,
					HostFuncError,
				> { Err(HostFuncError::User(1)) };
				import = import.with_func::<(), ()>(&name, function_static).map_err(|e| {
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
			)));
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
