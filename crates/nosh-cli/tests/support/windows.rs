use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsHandle, AsRawHandle, FromRawHandle, OwnedHandle};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nosh_platform::process::ProcessGroup;
use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows_sys::Win32::System::Console::{COORD, ClosePseudoConsole, CreatePseudoConsole, HPCON};
use windows_sys::Win32::System::Threading::{
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess, InitializeProcThreadAttributeList,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
};

#[derive(Default)]
struct Output {
    bytes: Vec<u8>,
    parser: vte::Parser<0>,
    text: Text,
    error: Option<String>,
}

#[derive(Default)]
struct Text(String);

impl vte::Perform for Text {
    fn print(&mut self, character: char) {
        self.0.push(character);
    }

    fn execute(&mut self, byte: u8) {
        self.0.push(char::from(byte));
    }
}

impl Output {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
        self.parser.advance(&mut self.text, bytes);
    }

    fn consume(&self, needle: &str, observed: &mut usize) -> bool {
        assert!(!needle.is_empty());
        let Some(offset) = self.text.0[*observed..].find(needle) else {
            return false;
        };
        *observed += offset + needle.len();
        true
    }
}

pub struct Terminal {
    input: std::io::PipeWriter,
    console: Option<HPCON>,
    process: OwnedHandle,
    group: ProcessGroup,
    output: Arc<Mutex<Output>>,
    reader: Option<JoinHandle<()>>,
    observed: usize,
    replies: usize,
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn argument(value: &OsStr, output: &mut Vec<u16>) {
    output.push(b'"' as u16);
    let mut slashes = 0;
    for unit in value.encode_wide() {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        let count = if unit == b'"' as u16 {
            slashes * 2 + 1
        } else {
            slashes
        };
        output.extend(std::iter::repeat_n(b'\\' as u16, count));
        output.push(unit);
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
    output.push(b'"' as u16);
}

impl Terminal {
    pub fn spawn(mut command: Command) -> Self {
        if !command
            .get_envs()
            .any(|(name, _)| name.to_string_lossy().eq_ignore_ascii_case("TERM"))
        {
            command.env_remove("TERM");
        }
        command
            .env("PS1", "NATIVE> ")
            .env("PROMPT_COMMAND", "printf 'NOSH-TEST-PROMPT\\n'")
            .env_remove("COLORTERM");
        let (input_reader, input) = std::io::pipe().unwrap();
        let (mut output_reader, output_writer) = std::io::pipe().unwrap();
        let mut console = 0;
        // SAFETY: these synchronous pipe handles remain live until the client is created.
        let result = unsafe {
            CreatePseudoConsole(
                COORD { X: 100, Y: 30 },
                input_reader.as_raw_handle(),
                output_writer.as_raw_handle(),
                0,
                &raw mut console,
            )
        };
        assert!(result >= 0, "CreatePseudoConsole: {result:#x}");
        let output = Arc::new(Mutex::new(Output::default()));
        let collected = output.clone();
        let reader = std::thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match output_reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(size) => {
                        let mut output = collected.lock().unwrap();
                        if output.bytes.len() + size > 1024 * 1024 {
                            output.error = Some("ConPTY output exceeded 1 MiB".into());
                            break;
                        }
                        output.push(&buffer[..size]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => break,
                    Err(error) => {
                        collected.lock().unwrap().error = Some(error.to_string());
                        break;
                    }
                }
            }
        });
        let mut size = 0;
        // SAFETY: the first call obtains the required size; usize storage supplies alignment.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &raw mut size) };
        assert!(size > 0);
        let mut storage = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        let attributes = storage.as_mut_ptr().cast();
        assert_ne!(
            unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &raw mut size) },
            0
        );
        assert_ne!(
            unsafe {
                UpdateProcThreadAttribute(
                    attributes,
                    0,
                    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                    console as *const std::ffi::c_void,
                    std::mem::size_of::<HPCON>(),
                    std::ptr::null_mut(),
                    std::ptr::null(),
                )
            },
            0
        );
        let startup = STARTUPINFOEXW {
            StartupInfo: windows_sys::Win32::System::Threading::STARTUPINFOW {
                cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
                // Do not inherit the test runner's redirected pipes into the console client.
                dwFlags: STARTF_USESTDHANDLES,
                hStdInput: INVALID_HANDLE_VALUE,
                hStdOutput: INVALID_HANDLE_VALUE,
                hStdError: INVALID_HANDLE_VALUE,
                ..Default::default()
            },
            lpAttributeList: attributes,
        };
        let program = wide(command.get_program());
        let mut arguments = Vec::new();
        argument(command.get_program(), &mut arguments);
        for value in command.get_args() {
            arguments.push(b' ' as u16);
            argument(value, &mut arguments);
        }
        arguments.push(0);
        let mut environment: BTreeMap<_, _> = std::env::vars_os()
            .map(|(name, value)| (name.to_string_lossy().to_uppercase(), (name, value)))
            .collect();
        for (name, value) in command.get_envs() {
            let key = name.to_string_lossy().to_uppercase();
            if let Some(value) = value {
                environment.insert(key, (name.to_owned(), value.to_owned()));
            } else {
                environment.remove(&key);
            }
        }
        let mut environment_block = Vec::new();
        for (name, value) in environment.into_values() {
            environment_block.extend(name.encode_wide());
            environment_block.push(b'=' as u16);
            environment_block.extend(value.encode_wide());
            environment_block.push(0);
        }
        environment_block.push(0);
        let directory = command.get_current_dir().map(|path| wide(path.as_os_str()));
        let mut information = PROCESS_INFORMATION::default();
        let group = ProcessGroup::new(None).unwrap();
        // SAFETY: all pointer arguments are retained for the duration of CreateProcessW.
        let created = unsafe {
            CreateProcessW(
                program.as_ptr(),
                arguments.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
                environment_block.as_ptr().cast(),
                directory
                    .as_ref()
                    .map_or(std::ptr::null(), |path| path.as_ptr()),
                &raw const startup.StartupInfo,
                &raw mut information,
            )
        };
        let error = std::io::Error::last_os_error();
        unsafe { DeleteProcThreadAttributeList(attributes) };
        if created == 0 {
            drop(input_reader);
            drop(output_writer);
            unsafe { ClosePseudoConsole(console) };
            reader.join().unwrap();
            panic!("ConPTY child: {error}");
        }
        let process = unsafe { OwnedHandle::from_raw_handle(information.hProcess) };
        let _thread = unsafe { OwnedHandle::from_raw_handle(information.hThread) };
        if let Err(error) = group.attach(information.dwProcessId, process.as_handle()) {
            assert_ne!(unsafe { TerminateProcess(process.as_raw_handle(), 1) }, 0);
            drop(input_reader);
            drop(output_writer);
            unsafe { ClosePseudoConsole(console) };
            reader.join().unwrap();
            panic!("ConPTY process ownership: {error}");
        }
        drop(input_reader);
        drop(output_writer);
        Self {
            input,
            console: Some(console),
            process,
            group,
            output,
            reader: Some(reader),
            observed: 0,
            replies: 0,
        }
    }

    fn pump(&mut self) {
        let output = self.output.lock().unwrap();
        assert!(output.error.is_none(), "{:?}", output.error);
        let queries = output
            .bytes
            .windows(4)
            .filter(|bytes| *bytes == b"\x1b[6n")
            .count();
        drop(output);
        for _ in self.replies..queries {
            self.input.write_all(b"\x1b[1;1R").unwrap();
        }
        self.replies = queries;
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.observed = self.output.lock().unwrap().text.0.len();
        self.input.write_all(bytes).unwrap();
    }

    pub fn control_c(&mut self) {
        // ConPTY requests Win32 input mode for ReadConsoleInput clients.
        self.send(b"\x1b[67;46;3;1;8;1_\x1b[67;46;3;0;8;1_");
        // A draft repaint can contain the old prompt before Ctrl+C is processed.
        self.expect("NOSH-TEST-PROMPT");
    }

    pub fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            self.pump();
            let output = self.output.lock().unwrap();
            if output.consume(needle, &mut self.observed) {
                return;
            }
            drop(output);
            if Instant::now() >= deadline {
                let mut code = 0;
                assert_ne!(
                    unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) },
                    0
                );
                panic!("waiting for {needle:?} (process {code}): {}", self.text());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn wait_for_prompt(&mut self) {
        self.expect("NOSH-TEST-PROMPT");
        self.expect("NATIVE>");
    }

    pub fn text(&self) -> String {
        self.output.lock().unwrap().text.0.clone()
    }

    pub fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.pump();
            // SAFETY: this is the retained handle of our own ConPTY client.
            match unsafe { WaitForSingleObject(self.process.as_raw_handle(), 0) } {
                WAIT_OBJECT_0 => break,
                WAIT_TIMEOUT => {}
                _ => panic!("ConPTY wait: {}", std::io::Error::last_os_error()),
            }
            assert!(
                Instant::now() < deadline,
                "ConPTY child did not exit: {}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut code = 0;
        assert_ne!(
            unsafe { GetExitCodeProcess(self.process.as_raw_handle(), &raw mut code) },
            0
        );
        assert_eq!(code, 0, "{}", self.text());
        self.close();
    }

    fn close(&mut self) {
        self.group.finish().unwrap();
        if let Some(console) = self.console.take() {
            // The reader continues draining while ConPTY flushes and closes its output.
            unsafe { ClosePseudoConsole(console) };
        }
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::Output;

    #[test]
    fn consecutive_markers_in_one_read_preserve_the_suffix() {
        let mut output = Output::default();
        output.push(b"\x1b[32mSETUP-DONE\x1b[0m\r\nNATIVE> ready");
        let mut observed = 0;
        assert!(output.consume("SETUP-DONE", &mut observed));
        assert!(output.consume("NATIVE>", &mut observed));
        assert!(output.consume("ready", &mut observed));
        assert!(!output.consume("ready", &mut observed));
    }

    #[test]
    fn marker_offsets_survive_split_ansi_and_utf8_sequences() {
        let mut output = Output::default();
        output.push(b"\x1b[31");
        output.push(b"m\xe4\xb8");
        assert!(output.text.0.is_empty());
        output.push(b"\xad\x1b[0m DONE");
        let mut observed = 0;
        assert!(output.consume("\u{4e2d}", &mut observed));
        assert!(output.consume("DONE", &mut observed));
        output.push(b"\x1b[2KNEXT");
        assert!(output.consume("NEXT", &mut observed));
    }

    #[test]
    fn cancellation_handshake_ignores_old_prompt_redraws() {
        let mut output = Output::default();
        let mut observed = 0;
        output.push(b"NATIVE> draft\r\nNATIVE> draft");
        assert!(!output.consume("NOSH-TEST-PROMPT", &mut observed));
        output.push(b"\r\nNOSH-TEST-PROMPT\r\nNATIVE>");
        assert!(output.consume("NOSH-TEST-PROMPT", &mut observed));
        assert!(output.consume("NATIVE>", &mut observed));
    }
}
