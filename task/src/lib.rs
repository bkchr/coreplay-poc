//! The task
//!
//! This is our example task for the proof of concept.
//!
//! At the top we our plumping logic and our debugging tool, `print` ;).
//!
//! At the bottom of this file we have our user logic.

use codec::{Decode, Encode};
use core::slice;
use futures::{
    channel::mpsc, executor::block_on, future::BoxFuture, pin_mut, poll, select_biased, FutureExt,
};
use messaging::{call_task, next_message, send_message, Message};
use std::{
    collections::VecDeque,
    mem,
    sync::{Arc, Mutex, OnceLock},
};

/// The context describing the relay chain in which the `service` function was called.
struct RelayChainContext {
    // Could probably contain stuff like block number and storage root to read information from the relay chain.
}

#[derive(Default)]
struct MessagesInner {
    relay_chain_contexts: VecDeque<RelayChainContext>,
}

enum IMessage {
    Context(RelayChainContext),
    Extern(Message),
}

#[derive(Clone, Default)]
struct Messages {
    inner: Arc<Mutex<MessagesInner>>,
}

impl Messages {
    async fn next(&self) -> IMessage {
        if let Some(context) = self.inner.lock().unwrap().relay_chain_contexts.pop_front() {
            return IMessage::Context(context);
        }

        IMessage::Extern(next_message().await)
    }
}

struct Context<UserContext> {
    main_fn: BoxFuture<'static, ()>,
    user_context: UserContext,
    yielded: bool,
    messages: Messages,
}

environmental::environmental!(YIELDED: bool);

/// The context holding all the information.
static mut CONTEXT: OnceLock<Context<()>> = OnceLock::new();

/// Host functions
mod host_function {
    extern "C" {
        /// Print the given string.
        pub fn print(msg: *const u8, len: u32);
        /// Returns `1` when program is expected to `yield`.
        pub fn should_yield() -> u32;
    }
}

/// Print the given `data` to the console of the executor
fn print(data: impl AsRef<str>) {
    let data = data.as_ref().as_bytes();
    unsafe {
        host_function::print(data.as_ptr(), data.len() as u32);
    }
}

/// Let the task to some service work.
#[no_mangle]
pub unsafe extern "C" fn service() {
    std::panic::set_hook(Box::new(|info| {
        let message = format!("{}", info);
        print(&message);
    }));

    if let Some(context) = CONTEXT.get_mut() {
        // First reset.
        context.yielded = false;
        YIELDED::using_once(&mut context.yielded, || {
            block_on(async {
                let _ = poll!(&mut context.main_fn);
            });
        })
    } else {
        let messages = Messages::default();
        let mut future = main(messages.clone()).boxed();
        let mut yielded = false;

        YIELDED::using_once(&mut yielded, || {
            block_on(async {
                let _ = poll!(&mut future);
            });
        });

        CONTEXT
            .set(Context {
                main_fn: future,
                user_context: (),
                yielded,
                messages,
            })
            .unwrap_or_else(|_| panic!("There is no `MAIN` set"));
    }
}

#[no_mangle]
pub unsafe extern "C" fn call(call: *const u8, len: u32) -> u64 {
    let data = slice::from_raw_parts(call, len as usize);
    let call = Call::decode(&mut &data[..]).unwrap();

    let res = Ok::<_, CallResult>(call.dispatch()).encode();

    let mut ptr = res.len() as u64;
    ptr <<= 32;
    ptr |= res.as_ptr() as *const u8 as u64;

    mem::forget(res);

    ptr
}

#[no_mangle]
pub unsafe extern "C" fn free_memory(ptr: *mut u8, len: u32) {
    drop(Box::from_raw(slice::from_raw_parts_mut(ptr, len as usize)));
}

#[no_mangle]
pub unsafe extern "C" fn allocate_memory(len: u32) -> u32 {
    let vec = Vec::with_capacity(len as usize);

    let ptr: *const u8 = vec.as_ptr();
    mem::forget(vec);

    ptr as u32
}

/// Maybe yield, depending on if the `executor` requested us to `yield` or not.
///
/// `Yield` means that this function will return `Poll::Pending` to give back
/// the control to the `executor`.
async fn maybe_yield() {
    let _yield = unsafe { host_function::should_yield() == 1 };

    if _yield {
        YIELDED::with(|y| *y = true).expect("Called in a `service` provided context");

        // Yielding is just done by returning `Pending`.
        futures::pending!()
    }
}

// --------------------------------------------------------------------
// Everyting below this line here shows how a user would write its user code.
// The rest of the "magic" should be abstractable.
// --------------------------------------------------------------------

#[derive(Encode, Decode)]
enum Call {
    DoSomething(u32),
}

#[derive(Encode, Decode, Debug)]
enum CallResult {
    DoSomethingR(u32),
}

impl Call {
    fn dispatch(&self) -> CallResult {
        match self {
            Self::DoSomething(r) => CallResult::DoSomethingR(*r),
        }
    }
}

/// The main function of our task.
async fn main(messages: Messages) {
    // let never_return = never_return().fuse();
    // pin_mut!(never_return);

    // The main loop
    loop {
        select_biased! {
            // First, poll for messages
            message = messages.next().fuse() => {
                match message {
                    IMessage::Context(_) => {},
                    IMessage::Extern(message) => {
                        match message {
                            Message::Ping => { send_message(Message::Pong); },
                            Message::Pong => { send_message(Message::Ping); },
                            Message::Call(task_id) => {
                                match call_task::<CallResult>(task_id, &Call::DoSomething(10)) {
                                    Ok(res) => print(format!("Task {task_id:?} returned: {res:?}")),
                                    Err(e) => print(format!("Calling task {task_id:?} returned an error: {e:?}")),
                                }
                            }
                        }
                    }
                }
            },
            // _ = never_return => {},
        }
    }
}

/// Just an example of a long runing future.
///
/// This implementation basically burns CPU time.
async fn never_return() {
    // This is only printed once and shows that we always reuse the same
    // future and not always recreate it.
    print("Init `never_return`");

    let mut counter: u64 = 0;

    loop {
        // Let's check each iteration if we should `yield`.
        //
        // The user will need to do this in synchronous logic, to ensure that we
        // give back our execution schedule when getting asked by the executor.
        maybe_yield().await;

        // Do some meaningful and important work.
        counter += 1;

        if counter % 100_000_000 == 0 {
            // Yes we are making progress!
            print(format!("Counter {}", counter));
        }
    }
}
