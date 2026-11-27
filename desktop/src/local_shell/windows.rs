use crate::terminal::BackendSettings;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

pub(super) fn backend(executable: &Path, folder: &Path) -> io::Result<BackendSettings> {
    if !executable.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "bundled executable is missing",
        ));
    }
    let mut buffer = vec![0u16; 32768];
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    if length == 0 {
        return Err(io::Error::last_os_error());
    }
    if length >= buffer.len() {
        return Err(io::Error::other(
            "system directory path exceeds the supported size",
        ));
    }
    let command =
        std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length])).join("cmd.exe");
    if !command.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "system cmd.exe is missing",
        ));
    }
    // /d disables AutoRun. Delayed expansion inserts the quoted executable only
    // after command parsing, without reparsing %, !, &, ^ or parentheses in its path.
    // No user terminal text or user-controlled arguments enter this fixed template.
    let bootstrap = format!(
        "set \"ERRORLEVEL=\" & start \"\" /b /wait \"!NOSH_EMBEDDED_PROGRAM!\" {} --offline --no-download -i & exit /b !errorlevel!",
        nosh_cli::EMBEDDED_SHELL_FLAG,
    );
    Ok(BackendSettings {
        shell: command.to_string_lossy().into_owned(),
        args: [
            "/d",
            "/s",
            "/e:on",
            "/v:on",
            "/c",
            "%NOSH_EMBEDDED_BOOTSTRAP%",
        ]
        .into_iter()
        .map(Into::into)
        .collect(),
        working_directory: Some(folder.to_owned()),
        env: [
            (
                "NOSH_EMBEDDED_PROGRAM".into(),
                executable.to_string_lossy().into_owned(),
            ),
            ("NOSH_EMBEDDED_BOOTSTRAP".into(), bootstrap),
        ]
        .into_iter()
        .collect(),
    })
}
