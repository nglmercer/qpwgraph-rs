#![no_std]
#![allow(non_snake_case)]

#[cfg(not(test))]
extern crate wdk_panic;

#[cfg(not(test))]
use wdk_alloc::WdkAllocator;

#[cfg(not(test))]
#[global_allocator]
static GLOBAL_ALLOCATOR: WdkAllocator = WdkAllocator;

mod driver;
mod ring {
    pub use qpwgraph_audio_core::*;
}
