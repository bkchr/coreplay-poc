//! Messaging related functionality.
//!
//! Exposes the [`Messages`] type, the [`next_message`] function and the [`send_message`] function.

use codec::{Decode, Encode};

/// The host function for sending and receiving messages.
mod host_function {
    extern "C" {
        /// Poll for the next message.
        ///
        /// `next` will be used to pass the SCALE encoded message.
        ///
        /// Returns `0` if there was no message and otherwise the lenght of the encoded message in `next`.
        pub fn next_message(next: *mut u8) -> u32;

        /// Send the given message.
        ///
        /// `msg` is the SCALE encoded message.
        /// `len` is the lenght of the SCALE encoded message.
        pub fn send_message(msg: *const u8, len: u32);

        pub fn call_task(task: *const u8, call: *const u8, len: u32, result: *mut u8) -> u32;
    }
}

/// The ID of a task.
pub type TaskId = [u8; 32];

#[derive(Encode, Decode, Debug)]
pub enum CallError {
    ResultDecoding,
}

/// Our glorious message type.
#[derive(Encode, Decode, Debug)]
pub enum Message {
    Ping,
    Pong,
    Call(TaskId),
}

/// Fetches the next message.
///
/// Returns when there is a new `message`.
pub async fn next_message() -> Message {
    // 1KiB should be enough for now.
    let mut buffer = vec![0; 1024];
    loop {
        let len = unsafe { host_function::next_message(buffer.as_mut_ptr()) as usize };

        if len == 0 {
            futures::pending!()
        } else {
            return Message::decode(&mut &buffer[..len]).unwrap();
        }
    }
}

/// Send a message.
pub fn send_message(msg: Message) {
    let encoded = msg.encode();

    unsafe { host_function::send_message(encoded.as_ptr(), encoded.len() as u32) };
}

pub fn call_task<R: Decode>(task: TaskId, msg: &impl Encode) -> Result<R, CallError> {
    let mut result_buffer = vec![0; 4096];
    let msg = msg.encode();

    let result_len = unsafe {
        host_function::call_task(
            task.as_ptr(),
            msg.as_ptr(),
            msg.len() as u32,
            result_buffer.as_mut_ptr(),
        ) as usize
    };

    if result_len > result_buffer.len() {
        unimplemented!("Implement")
    }

    Result::<R, CallError>::decode(&mut &result_buffer[..result_len])
        .map_err(|_| CallError::ResultDecoding)?
}
