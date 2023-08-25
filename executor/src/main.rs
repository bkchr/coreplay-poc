//! The executor
//!
//! The executor starts two different instances of the `task`. Then it always
//! schedules one instance and let's both instances talk to each other using [`Message`]'s.

use anyhow::Result;
use codec::{Decode, Encode};
use futures::{pin_mut, select, FutureExt};
use messaging::Message;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;
use wasmtime::*;

/// The inner of [`StoreData`].
struct StoreDataInner {
    memory: SharedMemory,
    messages: Vec<Message>,
    messages_to_send: Vec<Message>,
    name: String,
    should_yield: bool,
}

/// The data that is passed to our host calls.
#[derive(Clone)]
struct StoreData(Arc<Mutex<StoreDataInner>>);

/// The reprentation of a task.
struct Task {
    name: String,
    store_data: StoreData,
    service_fn: Option<Func>,
}

/// A simple wrapper that represents a function in wasm.
struct Func {
    store: Store<StoreData>,
    _instance: Instance,
    func: TypedFunc<(), ()>,
}

impl Func {
    async fn call(&mut self) -> Result<()> {
        self.func.call_async(&mut self.store, ()).await
    }
}

impl Task {
    fn send_message(&self, msg: Message) {
        self.store_data.0.lock().unwrap().messages.push(msg);
    }

    fn drain_messages_to_send(&self) -> Vec<Message> {
        std::mem::take(&mut self.store_data.0.lock().unwrap().messages_to_send)
    }

    fn set_yield(&self, _yield: bool) {
        self.store_data.0.lock().unwrap().should_yield = _yield;
    }
}

fn create_linker(
    store: &mut Store<StoreData>,
    engine: &Engine,
    shared_memory: &SharedMemory,
) -> Result<Linker<StoreData>> {
    let mut linker = Linker::new(engine);
    // Register the `next_message` function.
    linker.func_wrap(
        "env",
        "next_message",
        |caller: Caller<'_, StoreData>, next_message_ptr: u32| {
            if next_message_ptr == 0 {
                panic!("Invalid `next_message` argument");
            }

            let store = (*caller.data()).clone();
            let mut store = store.0.lock().unwrap();
            if let Some(message) = store.messages.pop() {
                let encoded = message.encode();

                println!("Passing message {message:?} into task {}.", store.name);

                unsafe {
                    let memory: *mut u8 = std::mem::transmute(store.memory.data().as_ptr());

                    // Our memory location is a local variable in wasm, thus this is safe.
                    memory
                        .offset(next_message_ptr as isize)
                        .copy_from(encoded.as_ptr(), encoded.len());
                }
                encoded.len() as u32
            } else {
                0u32
            }
        },
    )?;

    // Register the `send_message` function.
    linker.func_wrap(
        "env",
        "send_message",
        |caller: Caller<'_, StoreData>, message_ptr: u32, msg_len: u32| {
            if message_ptr == 0 || msg_len == 0 {
                panic!("Invalid `send_message` argument");
            }

            let store = (*caller.data()).clone();
            let mut store = store.0.lock().unwrap();
            let mut buffer = vec![0; msg_len as usize];

            unsafe {
                let memory: *mut u8 = std::mem::transmute(store.memory.data().as_ptr());

                // Our memory location is a local variable in wasm, thus this is safe.
                memory
                    .offset(message_ptr as isize)
                    .copy_to(buffer.as_mut_ptr(), msg_len as usize);
            }

            let message = Message::decode(&mut &buffer[..]).unwrap();

            println!("Received message {message:?} from task {}.", store.name);

            store.messages_to_send.push(message);
        },
    )?;

    // Register the `print` function.
    linker.func_wrap(
        "env",
        "print",
        |caller: Caller<'_, StoreData>, message_ptr: u32, msg_len: u32| {
            if message_ptr == 0 || msg_len == 0 {
                panic!("Invalid `print` argument");
            }

            let store = (*caller.data()).clone();
            let store = store.0.lock().unwrap();
            let mut buffer = vec![0; msg_len as usize];

            unsafe {
                let memory: *mut u8 = std::mem::transmute(store.memory.data().as_ptr());

                // Our memory location is a local variable in wasm, thus this is safe.
                memory
                    .offset(message_ptr as isize)
                    .copy_to(buffer.as_mut_ptr(), msg_len as usize);
            }

            println!("{}", String::from_utf8(buffer).unwrap());
        },
    )?;

    // Register the `should_yield` function.
    linker.func_wrap("env", "should_yield", |caller: Caller<'_, StoreData>| {
        if caller.data().0.lock().unwrap().should_yield {
            1u32
        } else {
            0u32
        }
    })?;

    linker.define(store, "env", "memory", shared_memory.clone())?;

    Ok(linker)
}

/// Create an instance of `Func` that holds the `service` function.
async fn service_fn(
    store_data: &StoreData,
    engine: &Engine,
    module: &Module,
    shared_memory: &SharedMemory,
) -> Result<Func> {
    let mut store = Store::new(&engine, store_data.clone());
    let linker = create_linker(&mut store, engine, shared_memory)?;
    let instance = linker.instantiate_async(&mut store, module).await?;

    let func = instance.get_typed_func(&mut store, "service")?;

    Ok(Func {
        func,
        store,
        _instance: instance,
    })
}

/// Create a task with the given `name`.
async fn create_task(name: impl Into<String>) -> Result<Task> {
    let mut config = Config::default();
    config.async_support(true);
    config.wasm_threads(true);
    let engine = Engine::new(&config)?;
    let module = Module::from_file(
        &engine,
        format!(
            "{}/../target/wasm32-unknown-unknown/release/task.wasm",
            env!("CARGO_MANIFEST_DIR")
        ),
    )?;

    let shared_memory = SharedMemory::new(&engine, MemoryType::shared(17, 16384))?;

    let name = name.into();
    let store_data = StoreData(Arc::new(Mutex::new(StoreDataInner {
        name: name.clone(),
        memory: shared_memory.clone(),
        messages_to_send: Vec::new(),
        messages: Vec::new(),
        should_yield: false,
    })));

    // TODO: Find out why changing the order of these functions make the TLS fail.
    let service_fn = service_fn(&store_data, &engine, &module, &shared_memory).await?;

    Ok(Task {
        name,
        store_data,
        service_fn: Some(service_fn),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("Initializing...");

    let task_a = create_task("task_a").await?;
    let task_b = create_task("task_b").await?;

    // Let's start with some message for `task_a`.
    task_a.send_message(Message::Ping);

    let mut tasks = vec![task_a, task_b];
    let mut messages_to_send = vec![];

    // Interleave the execution between both tasks.
    for task_i in 0.. {
        let index = task_i % tasks.len();
        let task = &mut tasks[index];

        messages_to_send
            .drain(..)
            .for_each(|m| task.send_message(m));

        println!("Scheduling task: {}", task.name);

        // Reset the `yield` signal.
        task.set_yield(false);
        let mut service_fn = task.service_fn.take().unwrap();

        // We need to `spawn` the `service` function. While the `call` function is async,
        // `wasmtime` still runs the wasm execution in the `poll` function.
        // Thus, to not block our execution we need to run it on another thread.
        let service_call = tokio::task::spawn_blocking(move || {
            Handle::current().block_on(async move {
                service_fn.call().await?;
                Ok::<_, anyhow::Error>(service_fn)
            })
        })
        .fuse();
        // Let's give each task `100ms`.
        let timeout = futures_timer::Delay::new(Duration::from_millis(100)).fuse();

        pin_mut!(service_call);
        pin_mut!(timeout);

        select! {
            // This should actually never return, as we only return when we set
            // the `yield` signal right now.
            res = service_call => {
                println!("Task {} service fn returned, is_err={}", task.name, res.is_err());
                task.service_fn = Some(res??);
            },
            _ = timeout => {
                println!("Yielding task {}", task.name);
                // Let's set the yield signal.
                task.set_yield(true);
                // Wait until the `service` function returns.
                task.service_fn = Some(service_call.await??);
            }
        }

        messages_to_send.extend(task.drain_messages_to_send());
    }

    Ok(())
}
