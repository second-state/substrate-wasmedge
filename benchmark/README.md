# Benchmark

## Environment

|     HW&SW     |                Contents                 |
| :-----------: | :-------------------------------------: |
|      CPU      | Intel(R) Core(TM) i7-6700 CPU @ 3.40GHz |
|    Memory     |               8045752 KB                |
| Linux version |            5.15.0-48-generic            |
|      OS       |           Ubuntu 22.04.1 LTS            |

## Version

|       Items        |                           Version                            |
| :----------------: | :----------------------------------------------------------: |
| substrate-wasmedge | [commit 139e32](https://github.com/second-state/substrate-wasmedge/commit/139e32c9e7a2a8d6f71bd6a1a0577381b41148c9) |
|     substrate      | [commit fb7792](https://github.com/paritytech/substrate/commit/fb779212ca6b59bd158d72deeab2502cb9670cca) |
|      WasmEdge      | [commit 8a83a0](https://github.com/apepkuss/WasmEdge/commit/8a83a060f244219abfa80133c2b8f1354f18cecb) |
|      wasmtime      |      [v1.0.0](https://docs.rs/wasmtime/1.0.0/wasmtime/)      |
|       wasmi        |         [v0.13](https://docs.rs/wasmi/0.13.2/wasmi/)         |

## Benchmark 1

These benchmarks are mostly low-level microbenchmarks meant to measure the executor's performance. 

### Description

#### Benchmark Functions

`call_empty_function`: It will call the function `test_empty_return()` in wasm runtime. The function content is as follows:

```rust
fn test_empty_return() {}
```

`dirty_1mb_of_memory`: It will call the function `test_dirty_plenty_memory()` in wasm runtime. The function content is as follows:

```rust
fn test_dirty_plenty_memory(heap_base: u32, heap_pages: u32) {
  // This piece of code will dirty multiple pages of memory. The number of pages is given by
  // the `heap_pages`. It's unit is a wasm page (64KiB). The first page to be cleared
  // is a wasm page that that follows the one that holds the `heap_base` address.
  //
  // This function dirties the **host** pages. I.e. we dirty 4KiB at a time and it will take
  // 16 writes to process a single wasm page.

  let heap_ptr = heap_base as usize;

  // Find the next wasm page boundary.
  let heap_ptr = round_up_to(heap_ptr, 65536);

  // Make it an actual pointer
  let heap_ptr = heap_ptr as *mut u8;

  // Traverse the host pages and make each one dirty
  let host_pages = heap_pages as usize * 16;
  for i in 0..host_pages {
    unsafe {
      // technically this is an UB, but there is no way Rust can find this out.
      heap_ptr.add(i * 4096).write(0);
    }
  }

  fn round_up_to(n: usize, divisor: usize) -> usize {
    (n + divisor - 1) / divisor
  }
}
```

#### Benchmark Subjects

**wasmi(interpreted):** Use wasmi as the executor.

**wasmedge:** Use wasmedge as the executor.

* **wasmedge_instance_reuse:** Reuse the instance on each function call. That is, instead of re-instantiating, a previously saved snapshot is used to get a "clean" instance. It does not support precompiled.
* **wasmedge_recreate_instance:** Recreate the instance from scratch on every instantiation. So it is really slow.
* **wasmedge_recreate_instance_precompiled:** Use AOT compiler and `CompilerOptimizationLevel` is `1s`. There come a [bug](https://github.com/WasmEdge/WasmEdge/issues/1818) during the runing, probably is raised while `runtime` is manipulating the linear memory.

**wasmtime:** Use wasmtime as the executor.

* **wasmtime_instance_reuse:** Reuse the instance on each function call. Due to the new arising wasmtime [pool mechanism](https://docs.rs/wasmtime/latest/wasmtime/enum.PoolingAllocationStrategy.html), this strategy will become a legacy, and will be removed in the future. It is also important to note that it does not support precompiled.
* **wasmtime_recreate_instance:** Recreate the instance from scratch on every instantiation.
* **wasmtime_recreate_instance_cow:** Recreate the instance from scratch on every instantiation. Use [copy-on-write memory](https://docs.rs/wasmtime/latest/wasmtime/struct.Config.html#method.memory_init_cow) when possible.
* **wasmtime_recreate_instance_cow_precompiled:** Compared to the previous strategy, now the runtime is instantiated using a precompiled module.
* **wasmtime_pooling:** [Pool](https://docs.rs/wasmtime/latest/wasmtime/enum.PoolingAllocationStrategy.html) the instances to avoid initializing everything from scratch on each instantiation.
* **wasmtime_pooling_cow:** [Pool](https://docs.rs/wasmtime/latest/wasmtime/enum.PoolingAllocationStrategy.html) the instances to avoid initializing everything from scratch on each instantiation. Use [copy-on-write memory](https://docs.rs/wasmtime/latest/wasmtime/struct.Config.html#method.memory_init_cow) when possible.
* **wasmtime_pooling_cow_precompiled:** The runtime is instantiated using a precompiled module and everything works! This strategy should be the fastest.

### Method

Run the following command:

```bash
$ cd substrate/client/executor
$ cargo bench  # test the wasmi executor
$ cargo bench --features=wasmedge  # test the wasmedge executor
$ cargo bench --features=wasmtime  # test the wasmtime executor
```

### Results

|                 Benchmark Functions                  | wasmi(interpreted) | wasmedge_instance_reuse | wasmedge_recreate_instance | wasmedge_recreate_instance_precompiled | wasmtime_instance_reuse | wasmtime_recreate_instance | wasmtime_recreate_instance_cow | wasmtime_recreate_instance_cow_precompiled | wasmtime_pooling | wasmtime_pooling_cow | wasmtime_pooling_cow_precompiled |
| :--------------------------------------------------: | :----------------: | :---------------------: | :------------------------: | :------------------------------------: | :---------------------: | :------------------------: | :----------------------------: | :----------------------------------------: | :--------------: | :------------------: | :------------------------------: |
| call_empty_function_from_kusama_runtime_on_1_threads |     10.661 ms      |        184.55 µs        |         23.994 ms          |               1.6317 ms                |        183.25 µs        |         209.02 µs          |           38.706 µs            |                 38.531 µs                  |    194.15 µs     |      14.802 µs       |            14.980 µs             |
| call_empty_function_from_kusama_runtime_on_2_threads |     21.454 ms      |        193.69 µs        |         26.212 ms          |               1.9893 ms                |        186.90 µs        |         244.97 µs          |           62.732 µs            |                 61.211 µs                  |    212.72 µs     |      20.259 µs       |            19.997 µs             |
| call_empty_function_from_kusama_runtime_on_4_threads |     41.445 ms      |        197.14 µs        |         25.294 ms          |               2.5480 ms                |        196.80 µs        |         324.98 µs          |           128.85 µs            |                 125.80 µs                  |    269.83 µs     |      27.427 µs       |            27.258 µs             |
| dirty_1mb_of_memory_from_kusama_runtime_on_1_threads |     10.747 ms      |        655.75 µs        |         24.519 ms          |               2.0727 ms                |        620.81 µs        |         645.81 µs          |           477.18 µs            |                 482.10 µs                  |    641.91 µs     |      454.19 µs       |            457.17 µs             |
| dirty_1mb_of_memory_from_kusama_runtime_on_2_threads |     21.312 ms      |        670.76 µs        |         27.580 ms          |                  ///                   |        633.35 µs        |         725.63 µs          |           543.73 µs            |                 544.30 µs                  |    674.26 µs     |      470.53 µs       |            470.93 µs             |
| dirty_1mb_of_memory_from_kusama_runtime_on_4_threads |     41.481 ms      |        691.30 µs        |         26.384 ms          |                  ///                   |        674.64 µs        |         891.46 µs          |           700.29 µs            |                 704.09 µs                  |    820.19 µs     |      501.99 µs       |            503.04 µs             |
|  call_empty_function_from_test_runtime_on_1_threads  |     10.443 ms      |        18.976 µs        |         1.8213 ms          |                  ///                   |        17.836 µs        |         44.185 µs          |           34.575 µs            |                 34.276 µs                  |    28.370 µs     |      9.7489 µs       |            9.6514 µs             |
|  call_empty_function_from_test_runtime_on_2_threads  |     20.820 ms      |        21.352 µs        |         2.4384 ms          |                  ///                   |        20.319 µs        |         66.378 µs          |           57.242 µs            |                 57.053 µs                  |    35.710 µs     |      15.661 µs       |            13.721 µs             |
|  call_empty_function_from_test_runtime_on_4_threads  |     35.907 ms      |        25.193 µs        |         4.2762 ms          |                  ///                   |        22.473 µs        |         120.46 µs          |           123.41 µs            |                 119.82 µs                  |    71.655 µs     |      23.273 µs       |            22.820 µs             |
|  dirty_1mb_of_memory_from_test_runtime_on_1_threads  |     10.444 ms      |        471.67 µs        |         2.2938 ms          |                  ///                   |        453.76 µs        |         486.53 µs          |           474.43 µs            |                 477.80 µs                  |    471.22 µs     |      449.26 µs       |            449.41 µs             |
|  dirty_1mb_of_memory_from_test_runtime_on_2_threads  |     20.811 ms      |        491.52 µs        |         3.1051 ms          |                  ///                   |        465.09 µs        |         553.72 µs          |           538.78 µs            |                 537.49 µs                  |    493.52 µs     |      470.92 µs       |            464.66 µs             |
|    dirty_1mb_of_memory_test_runtime_on_4_threads     |     35.722 ms      |        509.54 µs        |         4.7584 ms          |                  ///                   |        493.00 µs        |         685.34 µs          |           686.38 µs            |                 703.88 µs                  |    616.41 µs     |      496.83 µs       |            495.73 µs             |

### Analysis

The `wasmi(interpreted)` does not implement the "Instance reuse" feature. This means that it will recreate the instance from scratch on every instantiation. Correspondingly, `wasmedge_recreate_instance` and `wasmtime_recreate_instance` will also recreate the instance from scratch. So all three of them should be compared together. 

<img src="./image/img1.png" />

As you can see, the running times for `wasmi(interpreted)` and `wasmedge_recreate_instance`, are both at the ms level, but the running time for `wasmtime_recreate_instance` is much shorter. However, it is worth noting that creating a new Instance each time is not our goal, so perhaps the next comparison makes much more sense. 

Both `wasmedge_instance_reuse` and `wasmtime_instance_reuse` use the strategy of reusing Instances. Let's see how they compared in performance.

<img src="./image/img2.png" />

 `wasmedge_instance_reuse` and `wasmtime_instance_reuse` behave very closely. This result is encouraging.

As mentioned earlier, in the near future `wasmtime_instance_reuse` will be removed, due to the new arising wasmtime [pool mechanism](https://docs.rs/wasmtime/latest/wasmtime/enum.PoolingAllocationStrategy.html). Next, we'll look at what performance improvement will come from these new mechanisms in wasmtime. We mainly compare `wasmtime_instance_reuse`, `wasmtime_pooling` and `wasmtime_pooling_cow`.

<img src="./image/img3.png" />

As you can see, the `wasmtime_pooling_cow` has a very significant performance gain in most cases.

## Benchmark 2

These benchmarks focus on the high-level impact of different executors, for example, the performance impact on running nodes. The micro-benchmark above already shows the case very well and is much more accurate. Whether or not to carry out a high-level benchmark needs further discussion and depends on our spare time.

**TDDO()**

## Note

The comparison of different executors does not directly reflect the performance of the underlying wasm runtime. There is space for improvement in the implementation of Executor.
