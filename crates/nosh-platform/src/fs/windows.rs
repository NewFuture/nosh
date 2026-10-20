use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, LocalFree};
use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, TOKEN_INFORMATION_CLASS,
    TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER, TokenOwner, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FileBasicInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, ReplaceFileW,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub volume: u32,
    pub index: u64,
    pub links: u32,
    pub attributes: u32,
}

pub fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the file handle and output storage remain valid throughout this query.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file is a reparse point; edit its target explicitly",
        ));
    }
    Ok(FileIdentity {
        volume: information.dwVolumeSerialNumber,
        index: (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        links: information.nNumberOfLinks,
        attributes: information.dwFileAttributes,
    })
}

pub(super) fn file_stamp(path: &Path) -> io::Result<super::FileStamp> {
    let file = File::open(path)?;
    let identity = file_identity(&file)?;
    let metadata = file.metadata()?;
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: the opened file and correctly sized output storage outlive this query.
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileBasicInfo,
            (&raw mut basic).cast(),
            std::mem::size_of::<FILE_BASIC_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let unix_ns = |ticks: i64| -> io::Result<u64> {
        u64::try_from(ticks)
            .ok()
            .and_then(|ticks| ticks.checked_sub(116_444_736_000_000_000))
            .and_then(|ticks| ticks.checked_mul(100))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file time outside supported range",
                )
            })
    };
    Ok(super::FileStamp {
        size: metadata.len(),
        mtime_ns: unix_ns(basic.LastWriteTime)?,
        ctime_ns: unix_ns(basic.ChangeTime)?,
        dev: u64::from(identity.volume),
        ino: identity.index,
    })
}

struct SecurityDescriptor(*mut std::ffi::c_void);

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        // SAFETY: GetSecurityInfo allocated this descriptor with LocalAlloc.
        unsafe { LocalFree(self.0) };
    }
}

pub fn owned_by_current_user(file: &File) -> io::Result<bool> {
    let mut owner = std::ptr::null_mut();
    let mut descriptor = std::ptr::null_mut();
    // SAFETY: all returned pointers are retained until the descriptor guard is dropped.
    let result = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    let _descriptor = SecurityDescriptor(descriptor);
    let mut token = std::ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a borrowed pseudo-handle; the token is owned below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token) };
    let user = token_information(&token, TokenUser)?;
    let default_owner = token_information(&token, TokenOwner)?;
    // SAFETY: each aligned buffer contains the requested token structure and its SIDs.
    Ok(matches_token_owner(
        owner,
        unsafe { &*user.as_ptr().cast::<TOKEN_USER>() },
        unsafe { &*default_owner.as_ptr().cast::<TOKEN_OWNER>() },
    ))
}

fn matches_token_owner(owner: PSID, user: &TOKEN_USER, default_owner: &TOKEN_OWNER) -> bool {
    // Elevated tokens can create files owned by their default owner rather than TokenUser.
    !owner.is_null()
        && [user.User.Sid, default_owner.Owner]
            .into_iter()
            .any(|sid| !sid.is_null() && unsafe { EqualSid(owner, sid) } != 0)
}

fn token_information(
    token: &OwnedHandle,
    class: TOKEN_INFORMATION_CLASS,
) -> io::Result<Vec<usize>> {
    let mut length = 0;
    let first = unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            std::ptr::null_mut(),
            0,
            &raw mut length,
        )
    };
    if first != 0
        || io::Error::last_os_error().raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32)
    {
        return Err(io::Error::other("cannot query token identity size"));
    }
    let mut storage = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: aligned storage has at least the token information's reported byte size.
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            class,
            storage.as_mut_ptr().cast(),
            length,
            &raw mut length,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(storage)
}

pub fn replace_file(source: &Path, target: &Path) -> io::Result<()> {
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both paths are NUL-terminated and retained. ReplaceFile preserves the
    // existing file's ACL instead of replacing it with a temporary file's ACL.
    if unsafe {
        ReplaceFileW(
            target.as_ptr(),
            source.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, SECURITY_MAX_SID_SIZE, SID_AND_ATTRIBUTES, WELL_KNOWN_SID_TYPE,
        WinBuiltinAdministratorsSid, WinBuiltinUsersSid, WinLocalServiceSid,
    };

    fn sid(kind: WELL_KNOWN_SID_TYPE) -> Vec<usize> {
        let mut length = SECURITY_MAX_SID_SIZE;
        let mut storage = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
        assert_ne!(
            unsafe {
                CreateWellKnownSid(
                    kind,
                    std::ptr::null_mut(),
                    storage.as_mut_ptr().cast(),
                    &raw mut length,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        storage
    }

    #[test]
    fn ownership_accepts_only_the_token_user_or_default_owner() {
        let mut user_sid = sid(WinLocalServiceSid);
        let mut owner_sid = sid(WinBuiltinAdministratorsSid);
        let mut unrelated_sid = sid(WinBuiltinUsersSid);
        let user = TOKEN_USER {
            User: SID_AND_ATTRIBUTES {
                Sid: user_sid.as_mut_ptr().cast(),
                Attributes: 0,
            },
        };
        let default_owner = TOKEN_OWNER {
            Owner: owner_sid.as_mut_ptr().cast(),
        };
        assert!(matches_token_owner(user.User.Sid, &user, &default_owner));
        assert!(matches_token_owner(
            default_owner.Owner,
            &user,
            &default_owner
        ));
        assert!(!matches_token_owner(
            unrelated_sid.as_mut_ptr().cast(),
            &user,
            &default_owner,
        ));
        assert!(!matches_token_owner(
            std::ptr::null_mut(),
            &user,
            &default_owner,
        ));
    }

    #[test]
    fn newly_created_files_match_the_current_tokens_ownership() {
        let file = tempfile::tempfile().unwrap();
        assert_eq!(file_identity(&file).unwrap().links, 1);
        assert!(owned_by_current_user(&file).unwrap());
    }
}
