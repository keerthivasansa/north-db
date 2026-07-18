//! Core library for the North database engine.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod config;
pub mod heap;
pub mod schema;
pub mod storage;

pub use config::Config;
