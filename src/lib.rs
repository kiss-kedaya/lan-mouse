mod capture;
// the test commands drive the platform backends, Linux-only
#[cfg(target_os = "linux")]
pub mod capture_test;
pub mod client;
pub mod config;
mod connect;
mod crypto;
mod discovery;
mod dns;
mod emulation;
#[cfg(target_os = "linux")]
pub mod emulation_test;
pub mod i18n;
mod listen;
pub mod service;
