//! # 基于Future(MVP)，用于为外部提供基础的通用异步运行时和工具
//!

#![allow(warnings)]
#![feature(panic_info_message)]
#![feature(allocator_api)]
#![feature(alloc_error_hook)]

pub mod prelude;
pub mod lock;
pub mod rt;