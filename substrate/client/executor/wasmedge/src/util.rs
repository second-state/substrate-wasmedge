use sc_executor_common::{
	error::{Error, Result},
	util::checked_range,
};
use sp_wasm_interface::{Pointer, Value, ValueType};
use wasmedge_sdk::{types::Val, Memory, ValType, WasmValue};

/// Converts a [`wasmedge_sdk::types::Val`] into a substrate runtime interface [`Value`].
///
/// Panics if the given value doesn't have a corresponding variant in `Value`.
pub fn from_wasmedge_val(val: Val) -> Value {
	match val {
		Val::I32(v) => Value::I32(v),
		Val::I64(v) => Value::I64(v),
		Val::F32(v) => Value::F32(v as u32),
		Val::F64(v) => Value::F64(v as u64),
		v => panic!("Given value type is unsupported by Substrate: {:?}", v),
	}
}

/// Converts a sp_wasm_interface's [`Value`] into the corresponding variant in wasmedge's
/// [`wasmedge_sdk::types::Val`].
pub fn into_wasmedge_val(value: Value) -> Val {
	match value {
		Value::I32(v) => Val::I32(v),
		Value::I64(v) => Val::I64(v),
		Value::F32(f_bits) => Val::F32(f_bits as f32),
		Value::F64(f_bits) => Val::F64(f_bits as f64),
	}
}

/// Converts a [`wasmedge_sys::WasmValue`] into a substrate runtime interface [`Value`].
///
/// Panics if the given value doesn't have a corresponding variant in `Value`.
pub fn from_wasmedge_value(val: WasmValue) -> Value {
	match val.ty() {
		ValType::I32 => Value::I32(val.to_i32()),
		ValType::I64 => Value::I64(val.to_i64()),
		ValType::F32 => Value::F32(val.to_f32() as u32),
		ValType::F64 => Value::F64(val.to_f64() as u64),
		v => panic!("Given value type is unsupported by Substrate: {:?}", v),
	}
}

/// Converts a sp_wasm_interface's [`Value`] into the corresponding variant in wasmedge's
/// [`wasmedge_sys::WasmValue`].
pub fn into_wasmedge_value(value: Value) -> WasmValue {
	match value {
		Value::I32(v) => WasmValue::from_i32(v),
		Value::I64(v) => WasmValue::from_i64(v),
		Value::F32(f_bits) => WasmValue::from_f32(f_bits as f32),
		Value::F64(f_bits) => WasmValue::from_f64(f_bits as f64),
	}
}

pub fn into_wasmedge_val_type(val_ty: ValueType) -> ValType {
	match val_ty {
		ValueType::I32 => ValType::I32,
		ValueType::I64 => ValType::I64,
		ValueType::F32 => ValType::F32,
		ValueType::F64 => ValType::F64,
	}
}

pub(crate) fn read_memory_into(memory: &[u8], address: Pointer<u8>, dest: &mut [u8]) -> Result<()> {
	let range = checked_range(address.into(), dest.len(), memory.len())
		.ok_or_else(|| Error::Other("memory read is out of bounds".into()))?;

	dest.copy_from_slice(&memory[range]);
	Ok(())
}

pub(crate) fn write_memory_from(
	memory: &mut [u8],
	address: Pointer<u8>,
	data: &[u8],
) -> Result<()> {
	let range = checked_range(address.into(), data.len(), memory.len())
		.ok_or_else(|| Error::Other("memory write is out of bounds".into()))?;

	memory[range].copy_from_slice(data);
	Ok(())
}

pub(crate) fn read_memory(memory: &[u8], source_addr: Pointer<u8>, size: usize) -> Result<Vec<u8>> {
	let range = checked_range(source_addr.into(), size, memory.len())
		.ok_or_else(|| Error::Other("memory read is out of bounds".into()))?;

	let mut buffer = vec![0; range.len()];
	read_memory_into(memory, source_addr, &mut buffer)?;

	Ok(buffer)
}

pub(crate) fn memory_slice(memory: &Memory) -> &[u8] {
	let base_ptr: *const u8 = memory
		.data_pointer(0, 1)
		.expect("failed to returns the const data pointer to the Memory.");

	unsafe { std::slice::from_raw_parts(base_ptr, (memory.size() * 64 * 1024) as usize) }
}

pub(crate) fn memory_slice_mut(memory: &mut Memory) -> &mut [u8] {
	let base_ptr_mut: *mut u8 = memory
		.data_pointer_mut(0, 1)
		.expect("failed to returns the mut data pointer to the Memory.");

	unsafe { std::slice::from_raw_parts_mut(base_ptr_mut, (memory.size() * 64 * 1024) as usize) }
}
