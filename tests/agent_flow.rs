//! Cross-crate flow regressions; shared fixtures remain in one test binary.

#[path = "flows/support.rs"]
mod support;

#[path = "flows/agent.rs"]
mod agent;

#[path = "flows/command_assist.rs"]
mod command_assist;

#[path = "flows/context.rs"]
mod context;

#[path = "flows/permissions.rs"]
mod permissions;
