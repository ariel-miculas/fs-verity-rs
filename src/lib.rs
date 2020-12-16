#![feature(str_split_once)]
#![feature(slice_fill)]

mod config;

pub use config::*;

#[cfg(target_os = "linux")]
pub mod linux;

mod digest;

pub use digest::*;
