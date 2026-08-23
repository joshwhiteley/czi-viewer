//! Minimal, Darwin-only PTY process support for the embedded OpenSSH console.
//!
//! This crate deliberately contains the workspace's only unsafe code for this feature. The
//! [`libc`] calls are limited to opening/configuring a PTY, creating binary socket endpoints, `posix_spawn` file
//! actions, process signalling, reaping, and the executor's `TIOCSCTTY`/`execve` transition. It
//! uses `POSIX_SPAWN_SETSID` plus an `addopen` action for the slave PTY. Darwin processes file
//! actions before that session transition, so the tiny executor calls `TIOCSCTTY` and then
//! `execve`s the absolute target. This avoids `fork` in a multithreaded application.
//!
//! The safe API always removes `SSH_ASKPASS` and `SSH_ASKPASS_REQUIRE` from the child environment.
//! It invokes `posix_spawn` with an absolute executable path; it never uses a shell or `spawnp`.

#![cfg_attr(
    target_os = "macos",
    allow(
        unsafe_code,
        reason = "audited Darwin libc boundary; all callers use the safe PTY/process API"
    )
)]
#![cfg_attr(target_os = "macos", deny(unsafe_op_in_unsafe_fn))]
#![allow(
    clippy::borrow_as_ptr,
    reason = "the audited Darwin libc boundary passes references as C raw pointers"
)]

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "macos")]
pub use macos::{
    Cancellation, Child, PtyMaster, SpawnedPty, SpawnedTerminal, claim_controlling_terminal,
    claim_controlling_terminal_and_exec, spawn, spawn_terminal,
};
