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

use sc_executor_common::error::WasmError;
use sp_wasm_interface::HostFunctions;
use std::collections::HashMap;
use wasmedge_sys::Vm;

pub(crate) fn prepare_imports<H>(
	vm: &mut Vm,
	module: &wasmedge_sys::Module,
	allow_missing_func_imports: bool,
) -> Result<(), WasmError>
where
	H: HostFunctions,
{
	let mut pending_func_imports = HashMap::new();

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

	let mut import = wasmedge_sys::ImportModule::create("env")
		.map_err(|e| WasmError::Other(format!("fail to create a WasmEdge Vm context: {}", e)))?;

	// ============================== There is some remaining work ================================

	// ============================================================================================

	vm.register_wasm_from_import(wasmedge_sys::ImportObject::Import(import))
		.map_err(|e| WasmError::Other(format!("vm register import err: {}", e)))?;

	Ok(())
}
