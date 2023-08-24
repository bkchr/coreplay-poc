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
    }
}

/// Our glorious message type.
#[derive(Encode, Decode, Debug)]
pub enum Message {
    Ping,
    Pong,
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
