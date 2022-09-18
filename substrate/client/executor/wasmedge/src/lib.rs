#![feature(never_type)]

mod host;
mod imports;
mod instance_wrapper;
mod runtime;
mod util;

#[cfg(test)]
mod tests;

pub use imports::HostFuncErrorWasmEdge;
pub use runtime::{
	create_runtime, create_runtime_from_artifact, prepare_runtime_artifact, Config,
	DeterministicStackLimit, Semantics,
};
