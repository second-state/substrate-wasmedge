use sc_executor_common::{
	error::{Error, Result},
	util::checked_range,
};
use sp_wasm_interface::{Pointer, Value};

/// Converts a [`wasmedge_sys::WasmValue`] into a substrate runtime interface [`Value`].
///
/// Panics if the given value doesn't have a corresponding variant in `Value`.
pub fn from_wasmedge_val(val: wasmedge_sys::WasmValue) -> Value {
	match val.ty() {
		wasmedge_types::ValType::I32 => Value::I32(val.to_i32()),
		wasmedge_types::ValType::I64 => Value::I64(val.to_i64()),
		wasmedge_types::ValType::F32 => Value::F32(val.to_f32() as u32),
		wasmedge_types::ValType::F64 => Value::F64(val.to_f64() as u64),
		v => panic!("Given value type is unsupported by Substrate: {:?}", v),
	}
}

/// Converts a sp_wasm_interface's [`Value`] into the corresponding variant in wasmedge's
/// [`wasmedge_sys::WasmValue`].
pub fn into_wasmedge_val(value: Value) -> wasmedge_sys::WasmValue {
	match value {
		Value::I32(v) => wasmedge_sys::WasmValue::from_i32(v),
		Value::I64(v) => wasmedge_sys::WasmValue::from_i64(v),
		Value::F32(f_bits) => wasmedge_sys::WasmValue::from_f32(f_bits as f32),
		Value::F64(f_bits) => wasmedge_sys::WasmValue::from_f64(f_bits as f64),
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
