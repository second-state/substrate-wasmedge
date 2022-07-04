use crate::{instance_wrapper::InstanceWrapper, util};
use sc_executor_common::error::WasmError;
use sp_wasm_interface::{Function, ValueType};
use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};
use wasmedge_sys::ImportInstance;

struct Wrapper(*mut InstanceWrapper);
unsafe impl Send for Wrapper{}

/// Goes over all imports of a module and register host functions into Vm.
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

			// let instance_wrapper_clone = Arc::clone(&instance_wrapper);
			let s = Arc::new(Mutex::new(Wrapper(instance_wrapper as *mut InstanceWrapper)));
			let returns_len = host_func_ty.returns_len();
			let function_static = move |inputs: Vec<wasmedge_sys::WasmValue>| -> std::result::Result<
				Vec<wasmedge_sys::WasmValue>,
				u8,
			> {
				println!("{}", host_func.name());
				let unwind_result = {

					// `from_wasmedge_val` panics if it encounters a value that doesn't fit into the values
					// available in substrate.
					//
					// This, however, cannot happen since the signature of this function is created from
					// a `dyn Function` signature of which cannot have a non substrate value by definition.
					let mut params = inputs.iter().cloned().map(util::from_wasmedge_val);

					std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
						unsafe {
							host_func.execute(&mut *(s.lock().unwrap().0), &mut params)
						}
					}))
				};
				println!("leave");
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

			let func = wasmedge_sys::Function::create(&host_func_ty, Box::new(function_static), 0)
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
				let func = wasmedge_sys::Function::create(
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

	instance_wrapper
		.vm()
		.lock()
		.map_err(|e| WasmError::Other(format!("failed to lock: {}", e,)))?
		.register_wasm_from_import(wasmedge_sys::ImportObject::Import(import))
		.map_err(|e| WasmError::Other(format!("vm register import err: {}", e)))?;

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
