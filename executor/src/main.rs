//! The executor
//!
//! The executor starts two different instances of the `task`. Then it always
//! schedules one instance and let's both instances talk to each other using [`Message`]'s.

use anyhow::Result;
use codec::{Decode, Encode};
use futures::{
    channel::{mpsc, oneshot},
    pin_mut, select, FutureExt, SinkExt, StreamExt,
};
use messaging::{Message, TaskId};
use std::time::Duration;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::runtime::Handle;
use wasmtime::*;

enum TaskRequest {
    Call {
        target: TaskId,
        call: Vec<u8>,
        result: oneshot::Sender<Vec<u8>>,
    },
}

/// The inner of [`StoreData`].
struct StoreDataInner {
    memory: SharedMemory,
    messages: Vec<Message>,
    messages_to_send: Vec<Message>,
    name: String,
    should_yield: bool,
    request_sender: mpsc::Sender<TaskRequest>,
}

/// The data that is passed to our host calls.
#[derive(Clone)]
struct StoreData(Arc<Mutex<StoreDataInner>>);

/// The reprentation of a task.
struct Task {
    name: String,
    store_data: StoreData,
    service_fn: Arc<Mutex<Option<Func<(), ()>>>>,
    call_fn: Arc<Mutex<Option<Func<(u32, u32), u64>>>>,
    allocate_fn: Arc<Mutex<Option<Func<u32, u32>>>>,
    free_fn: Arc<Mutex<Option<Func<(u32, u32), ()>>>>,
    task_id: TaskId,
}

/// A simple wrapper that represents a function in wasm.
struct Func<P, R> {
    store: Store<StoreData>,
    _instance: Instance,
    func: TypedFunc<P, R>,
}

impl<P: WasmParams, R: WasmResults> Func<P, R> {
    async fn call(&mut self, params: P) -> Result<R> {
        self.func.call_async(&mut self.store, params).await
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

    fn read_memory(&self, offset: u32, len: u32) -> Vec<u8> {
        let store = self.store_data.0.lock().unwrap();

        let mut res = vec![0u8; len as usize];

        unsafe {
            let memory: *mut u8 = std::mem::transmute(store.memory.data().as_ptr());

            memory
                .offset(offset as isize)
                .copy_to(res.as_mut_ptr(), len as usize);
        }

        res
    }

    fn write_memory(&self, offset: u32, data: &[u8]) {
        let store = self.store_data.0.lock().unwrap();

        unsafe {
            let memory: *mut u8 = std::mem::transmute(store.memory.data().as_ptr());

            memory
                .offset(offset as isize)
                .copy_from(data.as_ptr(), data.len());
        }
    }

    async fn allocate_memory(&self, len: u32) -> u32 {
        let mut allocate_fn = self.allocate_fn.lock().unwrap().take().unwrap();

        let res = allocate_fn.call(len).await.unwrap();

        *self.allocate_fn.lock().unwrap() = Some(allocate_fn);
        res
    }

    async fn free_memory(&self, ptr: u32, len: u32) {
        let mut free_memory = self.free_fn.lock().unwrap().take().unwrap();

        free_memory.call((ptr, len)).await;

        *self.free_fn.lock().unwrap() = Some(free_memory);
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

    linker.func_wrap4_async(
        "env",
        "call_task",
        |caller: Caller<'_, StoreData>,
         target: u32,
         call_ptr: u32,
         call_len: u32,
         result_ptr: u32| {
            if call_ptr == 0 || call_len == 0 || result_ptr == 0 || target == 0 {
                panic!("Invalid `call_task` argument");
            }

             println!("call_task");

            let store_data = (*caller.data()).clone();
            let store = store_data.0.lock().unwrap();
            let mut buffer = vec![0; call_len as usize];
            let mut target_task: TaskId = [0u8; 32];

            unsafe {
                let memory: *mut u8 = std::mem::transmute(store.memory.data().as_ptr());

                // Our memory location is a local variable in wasm, thus this is safe.
                memory
                    .offset(call_ptr as isize)
                    .copy_to(buffer.as_mut_ptr(), call_len as usize);

                memory
                    .offset(target as isize)
                    .copy_to(target_task.as_mut_ptr(), target_task.len());
            }

            let mut sender = store.request_sender.clone();
            drop(store);

            Box::new(async move {
                let (result_sender, result_recv) = oneshot::channel();

                sender
                    .send(TaskRequest::Call {
                        target: target_task,
                        call: buffer,
                        result: result_sender,
                    })
                    .await
                    .unwrap();

                let result = result_recv.await.unwrap();

                let store = store_data.0.lock().unwrap();

                unsafe {
                    let memory: *mut u8 = std::mem::transmute(store.memory.data().as_ptr());

                    // Our memory location is a local variable in wasm, thus this is safe.
                    memory
                        .offset(result_ptr as isize)
                        .copy_from(result.as_ptr(), result.len());
                }
                drop(store);

                result.len() as u32
            })
        },
    )?;

    linker.define(store, "env", "memory", shared_memory.clone())?;

    Ok(linker)
}

/// Create an instance of `Func` that holds the given `function`.
async fn get_fn<P: WasmParams, R: WasmResults>(
    function: &str,
    store_data: &StoreData,
    engine: &Engine,
    module: &Module,
    shared_memory: &SharedMemory,
) -> Result<Func<P, R>> {
    let mut store = Store::new(&engine, store_data.clone());
    let linker = create_linker(&mut store, engine, shared_memory)?;
    let instance = linker.instantiate_async(&mut store, module).await?;

    let func = instance.get_typed_func::<P, R>(&mut store, function)?;

    Ok(Func {
        func,
        store,
        _instance: instance,
    })
}

/// Create a task with the given `name`.
async fn create_task(
    name: impl Into<String>,
    request_sender: mpsc::Sender<TaskRequest>,
) -> Result<Task> {
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
        request_sender,
    })));

    // TODO: Find out why changing the order of these functions make the TLS fail.
    let service_fn = get_fn("service", &store_data, &engine, &module, &shared_memory).await?;
    let call_fn = get_fn("call", &store_data, &engine, &module, &shared_memory).await?;
    let allocate_fn = get_fn(
        "allocate_memory",
        &store_data,
        &engine,
        &module,
        &shared_memory,
    )
    .await?;
    let free_fn = get_fn("free_memory", &store_data, &engine, &module, &shared_memory).await?;

    let mut task_id = [0u8; 32];
    task_id[..name.as_bytes().len()].copy_from_slice(name.as_bytes());

    Ok(Task {
        name,
        store_data,
        service_fn: Arc::new(Mutex::new(Some(service_fn))),
        call_fn: Arc::new(Mutex::new(Some(call_fn))),
        allocate_fn: Arc::new(Mutex::new(Some(allocate_fn))),
        free_fn: Arc::new(Mutex::new(Some(free_fn))),
        task_id,
    })
}

pub fn unpack_ptr_and_len(val: u64) -> (u32, u32) {
    let ptr = (val & (!0u32 as u64)) as u32;
    let len = (val >> 32) as u32;

    (ptr, len)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("Initializing...");

    let (request_sender, mut request_recv) = mpsc::channel(1);

    let task_a = create_task("task_a", request_sender.clone()).await?;
    let task_b = create_task("task_b", request_sender.clone()).await?;

    // Let's start with some message for `task_a`.
    task_a.send_message(Message::Call(task_b.task_id));

    let task_a_running = run_task(&task_a).fuse();
    pin_mut!(task_a_running);

    select! {
        _ = task_a_running => {},
        request = request_recv.select_next_some() => {
            match request {
                TaskRequest::Call {
                    target,
                    call,
                    result,
                } => {
                    if target == task_b.task_id {
                        let ptr = task_b.allocate_memory(call.len() as u32).await;
                        task_b.write_memory(ptr, &call);
                        let mut call_fn = task_b.call_fn.lock().unwrap().take().unwrap();
                        let res = call_fn.call((ptr, call.len() as u32)).await?;
                        *task_b.call_fn.lock().unwrap() = Some(call_fn);
                        task_b.free_memory(ptr, call.len() as u32).await;

                        let (ptr, len) = unpack_ptr_and_len(res);
                        let res = task_b.read_memory(ptr, len);
                        task_b.free_memory(ptr, len).await;

                        result.send(res);
                    } else {
                        unimplemented!("Not supported right now")
                    }
                }
            }
        }
    }

    Ok(())
}

async fn run_task(task: &Task) {
    println!("Scheduling task: {}", task.name);

    // Reset the `yield` signal.
    task.set_yield(false);
    let mut service_fn = task.service_fn.lock().unwrap().take().unwrap();

    // We need to `spawn` the `service` function. While the `call` function is async,
    // `wasmtime` still runs the wasm execution in the `poll` function.
    // Thus, to not block our execution we need to run it on another thread.
    let service_fn = tokio::task::spawn_blocking(move || {
        Handle::current().block_on(async move {
            service_fn.call(()).await?;
            Ok::<_, anyhow::Error>(service_fn)
        })
    })
    .await
    .unwrap()
    .unwrap();

    *task.service_fn.lock().unwrap() = Some(service_fn);
}
