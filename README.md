# Coreplay Proof of Concept

This is a small proof of concept for the Coreplay RFC.

## Introduction

The idea of Coreplay is to have small tasks scheduled on the relay chain instead of full blown blockchains. These tasks are similar to smart contracts, as the idea is that they will
be much more specialized on a specific task. These tasks can be co-scheduled on the same core and Polkadot. These tasks will be able to pass synchronously messages to each other. The
idea is that these tasks are more like normal programs, this means that have a `main()` function and some other functions to talk to them. This brings some complications with it, as 
it will be required to "pause" the task to send the task proof of validity to the relay chain to let the task progress. The next time the task is scheduled, it will continue running 
at this previous "pause" point.

## Proof of Concept

This proof of concept shows the possibility to co-schedule long running tasks in wasm. Each task gets around 100ms of execution time, before the executor sets the `yield` signal. The
task is required to handle the signal in a timely manner and return back to the `executor`. The task is exposing two functions `service` and `yield`. The `service` function is the one driving
the `main` function. The `yield` function is used to set the `yield` signal. The proof of concept is using `wasmtime` and with `wasmtime` we will not be able to snapshot the `stack` when we 
want to "pause"(interrupt) the execution of the task. We need to interrupt the task, to send the proof of validity to the relay chain for validation. The relay chain will then need to use the 
same initial `memory` state and the same inputs to continue the execution. The `stack` in `wasmtime` uses the native `stack` for executing the wasm file and the exact stack layout also depends on the optimizations used at compile time and on the target architecture. Thus, we could not ensure that the relay chain uses the exact same stack layout as the collator/sequencer and the validation would fail. 
The proof of concept is using an explicit `yield` signal to let the task return. 
The task itself is written using `async` Rust code. `async` Rust code is using [generators](https://doc.rust-lang.org/beta/unstable-book/language-features/generators.html) 
to synthesize the `async` blocks. This comes with the advantage that this approach is stackless and thus, 
we can work around with the problem of not being able to snapshot the `stack`.

The proof of concept currently provides three host functions to the `task`:
- `next_message`: Returns back the `next` message, if there is one available.
- `send_message`: Send the given message to the other `task`.
- `print`: Print the given string (used for debugging ;).

The proof of concept is split into three crates:

- `messaging`: Contains the `Message` type and functions for sending and receiving messages.
- `task`: The task that is scheduled and executed. It supports sending and receiving messages and shows the stackless approach using async Rust.
- `executor`: Setups the task two times and lets both task execute and communicate with each other.

## Build & Run

This currently requires that you have a nightly Rust, the wasm target and rust-src installed.

Building the task binary:

```shell
RUSTFLAGS="-Ctarget-feature=+atomics,+bulk-memory" cargo build -Zbuild-std=panic_abort,std,core --target wasm32-unknown-unknown --release -p task
```

Running the executor:

```shell
cargo run -p executor
```

The executor will load the `task` from the `target` directory. Thus, the `task` needs to be build before.

## Open questions

- How to prevent that the `TLS` is destroyed. When we are creating a new `instance` of the wasm module, we can not 
reuse our memory and call our `service` function. It looks like the `TLS` maybe got destroyed in between?
- How to snapshot/restore the memory. Actually snapshoting should be easy, but restoring it on a different machine should be more complicated as pointers inside wasm maybe using the real memory location
and are not "relative".
- How to handle panic? We probably need to clean the entire memory and let the task completely restart. The proper solution for sure would be to not panic :P 
  - We could maybe also provide some sort of checkpointing host function. The task calls this and then it could be restored at this point. Maybe also with a hint or similar on what it will process. For example
  it will start to process a message and will panic. The host could be informed in this way that this message is bad. However, we could not do this for questions that require delivery (XCMP).
