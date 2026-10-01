//! Real-terminal regressions; probe test names are stable subprocess entrypoints.

#![cfg(unix)]

#[cfg(target_os = "macos")]
#[global_allocator]
static INPUT_WORKER_ALLOCATOR: nosh_shell::input_assist::WorkerAllocator =
    nosh_shell::input_assist::WorkerAllocator;

#[path = "terminal/support.rs"]
mod support;

#[path = "terminal/screen.rs"]
mod screen;

#[path = "terminal/display.rs"]
mod display;

#[path = "terminal/approval.rs"]
mod approval;

#[path = "terminal/input.rs"]
mod input;

#[path = "terminal/command_assist.rs"]
mod command_assist;

#[path = "terminal/status.rs"]
mod status;

#[path = "terminal/probes.rs"]
mod probes;

#[test]
fn blocked_input_worker_probe() {
    probes::blocked_input_worker_probe();
}

#[test]
fn input_worker_probe() {
    probes::input_worker_probe();
}

#[test]
fn terminal_probe() {
    probes::terminal_probe();
}
