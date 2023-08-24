//! The task
//!
//! This is our example task for the proof of concept.
//!
//! At the top we our plumping logic and our debugging tool, `print` ;).
//!
//! At the bottom of this file we have our user logic.

use futures::{executor::block_on, future::BoxFuture, pin_mut, poll, select_biased, FutureExt};
use messaging::{next_message, send_message, Message};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// The main `future` is stored inside this `static`.
static mut MAIN: OnceLock<BoxFuture<()>> = OnceLock::new();
/// Use this as `yield` signal for now.
///
/// A proper implementation should properly abstract over this. We should provide some kind of custom `Yield` struct for doing manual yielding.
/// Another idea could be some kind of attribute macro that could be placed above async functions to let them automatially yield.
static YIELD: AtomicBool = AtomicBool::new(false);

/// Debugging support
mod host_function {
    extern "C" {
        /// Print the given string.
        pub fn print(msg: *const u8, len: u32);
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

    // TODO: There is a potential race condition if `yield` is called before this is called.
    YIELD.store(false, Ordering::Relaxed);

    // Either re-use the already active `Future` or start the `future`.
    if let Some(future) = MAIN.get_mut() {
        print("Poll");
        block_on(async {
            let _ = poll!(future);
        });
    } else {
        print("Init");
        let mut future = main().boxed();
        block_on(async {
            let _ = poll!(&mut future);
        });
        MAIN.set(future)
            .unwrap_or_else(|_| panic!("There is no `MAIN` set"));
    }
}

/// Inform the task that it is time to yield.
#[no_mangle]
#[export_name = "yield"]
pub unsafe extern "C" fn yield_() {
    YIELD.store(true, Ordering::Relaxed);
}

/// Maybe yield, depending on if the `executor` requested us to `yield` or not.
///
/// `Yield` means that this function will return `Poll::Pending` to give back
/// the control to the `executor`.
async fn maybe_yield() {
    let _yield = YIELD.load(Ordering::Relaxed);

    if _yield {
        // Yielding is just done by returning `Pending`.
        futures::pending!()
    }
}

// --------------------------------------------------------------------
// Everyting below this line here shows how a user would write its user code.
// The rest of the "magic" should be abstractable.
// --------------------------------------------------------------------

/// The main function of our task.
async fn main() {
    let never_return = never_return().fuse();
    pin_mut!(never_return);

    // The main loop
    loop {
        select_biased! {
            // First, poll for messages
            message = next_message().fuse() => match message {
                Message::Ping => { send_message(Message::Pong); },
                Message::Pong => { send_message(Message::Ping); },
            },
            _ = never_return => {},
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
