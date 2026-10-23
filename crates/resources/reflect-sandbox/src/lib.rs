//! `reflect-sandbox` — OS 沙箱扩展。
//!
//! v1.2 P0-1:`os_sandbox` 从 stub 升级为真实沙箱(macOS Seatbelt +
//! Linux Landlock)。

pub mod os_sandbox;

pub use os_sandbox::{
    OsSandbox, OsSandboxStatus, OsSandboxStubStatus, detect_backend, probe_landlock_exec,
};
