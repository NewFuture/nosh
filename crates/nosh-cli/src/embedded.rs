use std::ffi::OsString;

pub const EMBEDDED_SHELL_FLAG: &str = "--__nosh_shell";
const INPUT_WORKER_FLAG: &str = "--__nosh_input_worker";

fn shell_arguments(mut arguments: Vec<OsString>) -> Option<Vec<OsString>> {
    match arguments.get(1).map(|value| value.as_os_str()) {
        Some(flag) if flag == EMBEDDED_SHELL_FLAG => {
            arguments.remove(1);
            Some(arguments)
        }
        Some(flag) if flag == INPUT_WORKER_FLAG => Some(arguments),
        _ => None,
    }
}

/// Dispatch self-exec shell/worker modes before GUI initialization or parsing.
/// Returns only for a normal desktop invocation. The CLI's worker allocator is
/// linked into the host and stays unbounded until the worker enables its budget.
pub fn dispatch_embedded() {
    let Some(arguments) = shell_arguments(std::env::args_os().collect()) else {
        return;
    };
    #[cfg(windows)]
    let _console = if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(EMBEDDED_SHELL_FLAG))
    {
        match console_stdio() {
            Ok(handles) => handles,
            Err(error) => {
                eprintln!("nosh: embedded console bootstrap: {error}");
                std::process::exit(2);
            }
        }
    } else {
        Vec::new()
    };
    crate::run_from_args(arguments);
}

#[cfg(windows)]
fn console_stdio() -> std::io::Result<Vec<std::fs::File>> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_INVALID_HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetConsoleProcessList, GetStdHandle,
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetStdHandle,
    };
    let streams = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let inherited = streams.map(|stream| unsafe { GetStdHandle(stream) });
    let valid = |handle: windows_sys::Win32::Foundation::HANDLE| {
        !handle.is_null() && handle != INVALID_HANDLE_VALUE
    };
    let mut pid = 0;
    if unsafe { GetConsoleProcessList(&mut pid, 1) } == 0 {
        if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
            let error = std::io::Error::last_os_error();
            // A fully redirected command can run without a parent console.
            if error.raw_os_error() != Some(ERROR_INVALID_HANDLE as i32)
                || inherited.iter().any(|handle| !valid(*handle))
            {
                return Err(error);
            }
        }
        // Attaching can initialize standard handles; retain explicit redirections.
        for (stream, handle) in streams.into_iter().zip(inherited) {
            if valid(handle) && unsafe { SetStdHandle(stream, handle) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
    }
    let mut handles = Vec::new();
    for (stream, device) in [
        (STD_INPUT_HANDLE, r"\\.\CONIN$"),
        (STD_OUTPUT_HANDLE, r"\\.\CONOUT$"),
        (STD_ERROR_HANDLE, r"\\.\CONOUT$"),
    ] {
        let current = unsafe { GetStdHandle(stream) };
        if valid(current) {
            continue;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(device)?;
        if unsafe { SetStdHandle(stream, file.as_raw_handle()) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        handles.push(file);
    }
    // start /b passes an inherited Ctrl+C-ignore attribute. Restore the normal
    // shell policy before its own Tokio interrupt handlers are installed.
    let mut pid = 0;
    if unsafe { GetConsoleProcessList(&mut pid, 1) } != 0
        && unsafe { SetConsoleCtrlHandler(None, 0) } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(handles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn redirected_handles_do_not_skip_parent_console_attachment() {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        use windows_sys::Win32::System::Console::{
            FreeConsole, GetConsoleProcessList, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
            STD_OUTPUT_HANDLE,
        };
        use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, DETACHED_PROCESS};
        const MODE: &str = "NOSH_CONSOLE_STDIO_TEST";
        const TEST: &str =
            "embedded::tests::redirected_handles_do_not_skip_parent_console_attachment";
        let mode = std::env::var(MODE).unwrap_or_default();
        if mode == "attached-parent" || mode == "detached-parent" {
            let mut pid = 0;
            assert_ne!(unsafe { GetConsoleProcessList(&mut pid, 1) }, 0);
            if mode == "detached-parent" {
                assert_ne!(unsafe { FreeConsole() }, 0);
            }
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(MODE, mode.replace("parent", "child"))
                .creation_flags(DETACHED_PROCESS)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            return;
        }
        if mode == "attached-child" || mode == "detached-child" {
            let streams = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
            let inherited = streams.map(|stream| unsafe { GetStdHandle(stream) });
            let mut pid = 0;
            assert_eq!(unsafe { GetConsoleProcessList(&mut pid, 1) }, 0);
            let _handles = console_stdio().unwrap();
            assert_eq!(
                streams.map(|stream| unsafe { GetStdHandle(stream) }),
                inherited,
                "console attachment must preserve all inherited redirections"
            );
            assert_eq!(
                unsafe { GetConsoleProcessList(&mut pid, 1) } != 0,
                mode == "attached-child"
            );
            return;
        }
        for mode in ["attached-parent", "detached-parent"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env(MODE, mode)
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
        }
    }

    #[test]
    fn only_first_argument_can_enter_a_process_role() {
        fn args(values: &[&str]) -> Vec<OsString> {
            values.iter().map(OsString::from).collect()
        }
        assert!(shell_arguments(args(&["desktop", "--cwd", EMBEDDED_SHELL_FLAG])).is_none());
        assert!(shell_arguments(args(&["desktop"])).is_none());
        assert_eq!(
            shell_arguments(args(&[
                "desktop",
                EMBEDDED_SHELL_FLAG,
                "--safe",
                "-c",
                "exit 37"
            ])),
            Some(args(&["desktop", "--safe", "-c", "exit 37"]))
        );
        assert_eq!(
            shell_arguments(args(&["desktop", INPUT_WORKER_FLAG])),
            Some(args(&["desktop", INPUT_WORKER_FLAG]))
        );
    }
}
