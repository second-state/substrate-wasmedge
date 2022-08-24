# substrate-wasmedge

Supporting [WasmEdge](https://github.com/WasmEdge/WasmEdge) as an alternative [Substrate](https://github.com/paritytech/substrate) WebAssembly runtime. This project increases the [Substrate](https://github.com/paritytech/substrate) ecosystem's node software diversity by supporting an alternative high-performance WebAssembly Runtime implementation.

Currently, [Substrate](https://github.com/paritytech/substrate) runs on the [wasmtime](https://github.com/bytecodealliance/wasmtime) WebAssembly runtime created by the Mozilla and Fastly team.  [WasmEdge](https://github.com/WasmEdge/WasmEdge) is another leading WebAssembly runtime hosted by the Linux Foundation / Cloud Native Computing Foundation (CNCF). It is fully compliant to the WebAssembly specification as well as standard WebAssembly extensions. It is supported across many OSes including Linux, Windows, Mac OS X, seL4, and CPU architectures including x86, aarch64, and Apple M1.  [WasmEdge](https://github.com/WasmEdge/WasmEdge) is among the fastest WebAssembly runtimes available today.

Compared with [wasmtime](https://github.com/bytecodealliance/wasmtime),  [WasmEdge](https://github.com/WasmEdge/WasmEdge) features a completely different software architecture. It is written in C++ and depends on the LLVM for runtime code generation, while [wasmtime](https://github.com/bytecodealliance/wasmtime) is written in Rust and depends on Cranelift for dynamic compilation. That makes  [WasmEdge](https://github.com/WasmEdge/WasmEdge) a compelling choice for improving [Substrate](https://github.com/paritytech/substrate) software stack diversity.

In this project, we use  [WasmEdge](https://github.com/WasmEdge/WasmEdge) as an alternative WebAssembly runtime for [Substrate](https://github.com/paritytech/substrate). We also create a software layer that allows users to choose between [wasmtime](https://github.com/bytecodealliance/wasmtime) and  [WasmEdge](https://github.com/WasmEdge/WasmEdge) when they build [Substrate](https://github.com/paritytech/substrate) from source.

**Note:** This project is still in a testing state, so don't use it in production!

## Overview

This project contains several folders:

* `WasmEdge`: [WasmEdge](https://github.com/WasmEdge/WasmEdge) code included (version 0.10.1 (2022-07-28)). In order to use the latest code, we specify the [`wasmedge-sys`](https://github.com/second-state/substrate-wasmedge/tree/main/WasmEdge/bindings/rust/wasmedge-sys) crate on a local path in [`cargo.toml`](https://github.com/second-state/substrate-wasmedge/blob/main/substrate/client/executor/wasmedge/Cargo.toml).
* `substrate`: [Substrate](https://github.com/paritytech/substrate) code included (version polkadot-v0.9.28). The path of the WasmEdge Executor is [`substrate/client/executor/wasmedge`](https://github.com/second-state/substrate-wasmedge/tree/main/substrate/client/executor/wasmedge).
* `substrate-node-template`: a fresh FRAME-based [Substrate](https://github.com/paritytech/substrate) node, ready for hacking. The [substrate-node-template](https://github.com/substrate-developer-hub/substrate-node-template/releases/tag/polkadot-v0.9.28) includes everything we need to get started with a core set of features in [Substrate](https://github.com/paritytech/substrate). In this project, we use this demo to verify that our WasmEdge Executor is working as expected.

## Build

1. Follow this [document](https://docs.substrate.io/install/rust-toolchain/) to install the packages and rust environment required for [Substrate](https://github.com/paritytech/substrate).

2. It is recommended to use the following command to install [WasmEdge](https://github.com/WasmEdge/WasmEdge). However, you can also follow this [document](https://wasmedge.org/book/en/start/install.html) to install.

   ```bash
   $ cd WasmEdge/utils
   $ ./install.sh
   ```

## Run

Using [WasmEdge](https://github.com/WasmEdge/WasmEdge) as the Executor:

```bash
$ cd substrate-node-template
$ cargo run --release --bin node-template -- \
  --dev \
  --wasm-execution=compiledWasmedge \
  --validator \
  --execution=Wasm \
  --tmp \
  --unsafe-ws-external
```

If you see the following message, then you've run it successfully! Congratulations!

```bash
2022-08-21 05:43:42 ✨ Imported #1 (0xabc1…bd8e)
2022-08-21 05:43:42 💤 Idle (0 peers), best: #1 (0xabc1…bd8e), finalized #0 (0x5640…1677), ⬇ 0 ⬆ 0
2022-08-21 05:43:47 💤 Idle (0 peers), best: #1 (0xabc1…bd8e), finalized #0 (0x5640…1677), ⬇ 0 ⬆ 0
2022-08-21 05:43:48 🙌 Starting consensus session on top of parent 0xabc17a0827771aaf56e027cc176f15bfe5f7589722e790e313e219df75f1bd8e
2022-08-21 05:43:48 🎁 Prepared block for proposing at 2 (0 ms) [hash: 0x6014c89e774871a92ba729addd9e90fdc7290a0da1604f99f07b509286d5e500; parent_hash: 0xabc1…bd8e; extrinsics (1): [0xb1b9…5b5c]]
2022-08-21 05:43:48 🔖 Pre-sealed block for proposal at 2. Hash now 0xe2919bdfbca4c2811f36b02db687fa4d4d5640fcca9fd9f75125d2a101869038, previously 0x6014c89e774871a92ba729addd9e90fdc7290a0da1604f99f07b509286d5e500.
```

By the way, you can also use [wasmtime](https://github.com/bytecodealliance/wasmtime) as the Executor in the following way:

```bash
$ cd substrate-node-template
$ cargo run --release --bin node-template -- --dev --wasm-execution=compiled
```

