use crate::{host::HostContext, instance_wrapper::InstanceWrapper, util};
use sc_executor_common::error::WasmError;
use sp_wasm_interface::{Function, ValueType};
use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
};
use wasmedge_sys::ImportInstance;

pub(crate) fn prepare_imports(
	instance_wrapper: Arc<Mutex<InstanceWrapper>>,
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

			let host_func_ty =
				wasmedge_sys::FuncType::create(params, results).expect("fail to create a FuncType");

			// if host_func_ty != func_ty {
			// 	panic!("fail to create a");
			// }

			let instance_wrapper_clone = Arc::clone(&instance_wrapper);

			let function_static = move |inputs: Vec<wasmedge_sys::WasmValue>| -> std::result::Result<
				Vec<wasmedge_sys::WasmValue>,
				u8,
			> {
				let mut host_ctx = HostContext::new(instance_wrapper_clone.lock().unwrap());
				let mut params = inputs.iter().cloned().map(util::from_wasmedge_val);
				let res = host_func.execute(&mut host_ctx, &mut params).unwrap().unwrap();
				Ok(vec![util::into_wasmedge_val(res)])
			};

			let func = wasmedge_sys::Function::create(&host_func_ty, Box::new(function_static), 0)
				.expect("fail to create a Function instance");

			import.add_func(&name, func);
		} else {
			missing_func_imports.insert(name, (import_ty, func_ty));
		}
	}

	if !missing_func_imports.is_empty() {
		if allow_missing_func_imports {
			for (name, (import_ty, func_ty)) in missing_func_imports {
				// let error = format!("call to a missing function {}:{}", import_ty.module_name(), name);
				// log::debug!("Missing import: '{}' {:?}", name, func_ty);

				let function_static = move |inputs: Vec<wasmedge_sys::WasmValue>| -> std::result::Result<
					Vec<wasmedge_sys::WasmValue>,
					u8,
				> { Err(0) };
				// let func = wasmedge_sys::Function::create(&func_ty, Box::new(function_static), 0)
				// 	.expect("fail to create a Function instance");

				// import.add_func(&name, func);
				// linker
				// 	.func_new("env", &name, func_ty.clone(), move |_, _, _| {
				// 		Err(Trap::new(error.clone()))
				// 	})
				// 	.expect("adding a missing import stub can only fail when the item already exists, and it is missing here; qed");
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
		.lock()
		.unwrap()
		.vm()
		.lock()
		.unwrap()
		.register_wasm_from_import(wasmedge_sys::ImportObject::Import(import))
		.map_err(|e| WasmError::Other(format!("vm register import err: {}", e)))?;

	instance_wrapper
		.lock()
		.unwrap()
		.vm()
		.lock()
		.unwrap()
		.validate()
		.map_err(|e| WasmError::Other(format!("fail to validate the wasm module: {}", e)))?;

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
