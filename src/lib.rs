//! VirtIO guest drivers.

#![no_std]
#![deny(unused_must_use, missing_docs)]
#![allow(clippy::identity_op)]
#![allow(dead_code)]

// #[macro_use]
extern crate log;
#[macro_use]
extern crate alloc;

mod blk;
mod console;
mod gpu;
mod hal;
mod header;
mod input;
mod net;
mod queue;

pub use self::blk::VirtIOBlk;
pub use self::console::VirtIOConsole;
pub use self::gpu::VirtIOGpu;
pub use self::header::*;
pub use self::input::VirtIOInput;
pub use self::net::VirtIONet;
use self::queue::VirtQueue;
use core::mem::size_of;
use hal::*;

/// The type returned by driver methods.
pub type Result<T = ()> = core::result::Result<T, Error>;

// pub struct Error {
//     kind: ErrorKind,
//     reason: &'static str,
// }

/// The error type of VirtIO drivers.
#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    /// The buffer is too small.
    BufferTooSmall,
    /// The device is not ready.
    NotReady,
    /// The queue is already in use.
    AlreadyUsed,
    /// Invalid parameter.
    InvalidParam,
    /// Failed to alloc DMA memory.
    DmaError,
    /// I/O Error
    IoError,
}

/// Align `size` up to a page.
const fn align_up(size: usize, page_size: usize) -> usize {
    (size + page_size) & !(page_size - 1)
}

/// Pages of `size`.
const fn pages(size: usize, page_size: usize) -> usize {
    (size + page_size - 1) / page_size
}

/// Page size
pub trait PageSize {
    /// Page size is equal to 1 << PAGE_SIZE_SHIFT
    const PAGE_SIZE_SHIFT: u8;

    /// Return calculated page size
    fn page_size() -> usize {
        1 << Self::PAGE_SIZE_SHIFT
    }
}

/// A standard 4KiB page.
pub struct Size4KiB;
impl PageSize for Size4KiB {
    const PAGE_SIZE_SHIFT: u8 = 12;
}

/// Convert a struct into buffer.
unsafe trait AsBuf: Sized {
    fn as_buf(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const _ as _, size_of::<Self>()) }
    }
    fn as_buf_mut(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self as *mut _ as _, size_of::<Self>()) }
    }
}

/// The error type of handle_interrupt fn.
pub enum HandleIntrError {
    /// The device is not ready.
    QueueNotReady,
    /// Corresponding waker does not exist. (id)
    WakerNotExist(u16),
}

/// Interrupt handler, Driver should implement it.
pub trait InterruptHandler {
    /// handler interrupt
    fn handle_interrupt(&self) -> core::result::Result<(), HandleIntrError>;
}

/// Handle virtio interrupts.
/// `driver_fetcher` is a function that accepts a device id.
/// and returns the corresponding driver.
/// Returned driver must implement the `InterruptHandler` trait.
/// `handle_interrupt_fn` returns a function for handling virtio device interrupts.
/// Returned function needs to pass in a device id.
pub fn handle_interrupt_fn<'a, F: Fn(usize) -> &'a dyn InterruptHandler>(
    driver_fetcher: F,
) -> impl Fn(usize) -> core::result::Result<(), HandleIntrError> {
    move |dev_id| {
        let driver = driver_fetcher(dev_id);
        driver.handle_interrupt()
    }
}
