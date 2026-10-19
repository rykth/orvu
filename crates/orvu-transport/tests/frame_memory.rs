//! Checks that reading a frame does not set memory aside before the data has
//! arrived.
//!
//! This file has one test and its own allocator, so that nothing else running
//! at the same time changes the numbers.

use orvu_transport::frame::read_frame;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;

/// The largest single allocation asked for so far.
static LARGEST: AtomicUsize = AtomicUsize::new(0);

struct Watching;

unsafe impl GlobalAlloc for Watching {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Watching = Watching;

// The other side says a 64 MiB frame is coming, sends a few bytes of it and
// then goes quiet. The reader must not have set 64 MiB aside for it.
#[tokio::test]
async fn a_claimed_size_alone_does_not_use_memory() {
    let (mut sender, mut receiver) = tokio::io::duplex(1024);
    let claimed: u32 = 64 * 1024 * 1024;
    sender.write_all(&claimed.to_be_bytes()).await.unwrap();
    sender.write_all(b"only a few bytes").await.unwrap();

    LARGEST.store(0, Ordering::Relaxed);
    let reading = tokio::spawn(async move { read_frame(&mut receiver).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(largest < 1024 * 1024, "the reader asked for {largest} bytes at once");

    reading.abort();
    drop(sender);
}
