use std::borrow::Cow;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf, Prefix};

use windows_sys::Win32::Globalization::{CSTR_EQUAL, CompareStringOrdinal};

pub(super) fn normalize(path: &Path) -> Cow<'_, Path> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => match prefix.kind() {
                Prefix::VerbatimDisk(drive) => normalized.push(format!("{}:", char::from(drive))),
                Prefix::VerbatimUNC(server, share) => {
                    let mut root = std::ffi::OsString::from("\\\\");
                    root.push(server);
                    root.push("\\");
                    root.push(share);
                    normalized.push(root);
                }
                _ => normalized.push(prefix.as_os_str()),
            },
            Component::RootDir => normalized.push("\\"),
            Component::Normal(name) => match name.to_str() {
                Some(name) => normalized.push(name.trim_end_matches(['.', ' '])),
                None => normalized.push(name),
            },
            Component::CurDir => {}
            Component::ParentDir => normalized.push(".."),
        }
    }
    if normalized == path {
        Cow::Borrowed(path)
    } else {
        Cow::Owned(normalized)
    }
}

fn equal(left: &OsStr, right: &OsStr) -> bool {
    let left: Vec<_> = left.encode_wide().collect();
    let right: Vec<_> = right.encode_wide().collect();
    let (Ok(left_len), Ok(right_len)) = (i32::try_from(left.len()), i32::try_from(right.len()))
    else {
        return false;
    };
    // SAFETY: both UTF-16 slices are retained for their explicitly supplied lengths.
    unsafe {
        CompareStringOrdinal(left.as_ptr(), left_len, right.as_ptr(), right_len, 1) == CSTR_EQUAL
    }
}

pub(super) fn strip_scope(path: &Path, root: &Path) -> Option<PathBuf> {
    let path = normalize(path);
    let root = normalize(root);
    let mut components = path.components();
    for base in root.components() {
        if !equal(components.next()?.as_os_str(), base.as_os_str()) {
            return None;
        }
    }
    Some(components.as_path().to_path_buf())
}

fn device(name: &str) -> bool {
    let base = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "CONIN$" | "CONOUT$")
        || ["COM", "LPT"].iter().any(|prefix| {
            base.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2"
                        | "3"
                        | "4"
                        | "5"
                        | "6"
                        | "7"
                        | "8"
                        | "9"
                        | "\u{00b9}"
                        | "\u{00b2}"
                        | "\u{00b3}"
                )
            })
        })
}

pub(super) fn ambiguous(path: &Path) -> bool {
    !path.is_absolute()
        || path.components().any(|component| match component {
            Component::Prefix(prefix) => !matches!(
                prefix.kind(),
                Prefix::Disk(_)
                    | Prefix::UNC(_, _)
                    | Prefix::VerbatimDisk(_)
                    | Prefix::VerbatimUNC(_, _)
            ),
            Component::Normal(name) => name.to_str().is_some_and(|name| {
                name.contains(':')
                    || name.trim_end_matches(['.', ' ']).is_empty()
                    || device(name.trim_end_matches(['.', ' ']))
            }),
            _ => false,
        })
}

pub(super) fn is_null(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            name.split('.')
                .next()
                .is_some_and(|stem| stem.eq_ignore_ascii_case("NUL"))
        })
}
