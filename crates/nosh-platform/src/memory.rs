//! Available host RAM observations, not model-selection or reservation policy.

use std::io;

/// Currently available/reclaimable RAM, in bytes, not total RAM or GPU memory.
pub fn available_bytes() -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        linux_available(&std::fs::read_to_string("/proc/meminfo")?)
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/vm_stat")
            .env("LC_ALL", "C")
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "vm_stat failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        macos_available(text)
    }
    #[cfg(windows)]
    {
        windows_memory().map(|(_, available)| available)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "available RAM observation is not supported on this platform",
        ))
    }
}

/// Total physical RAM. It does not include swap, pagefile or any GPU memory.
pub fn total_bytes() -> io::Result<u64> {
    #[cfg(target_os = "linux")]
    {
        let bytes = linux_bytes(&std::fs::read_to_string("/proc/meminfo")?, "MemTotal:")?;
        if bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "total physical memory is zero",
            ));
        }
        Ok(bytes)
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.memsize"])
            .env("LC_ALL", "C")
            .output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "sysctl hw.memsize failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        parse_total_bytes(text)
    }
    #[cfg(windows)]
    {
        windows_memory().map(|(total, _)| total)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "total RAM observation is not supported on this platform",
        ))
    }
}

#[cfg(windows)]
fn windows_memory() -> io::Result<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: status has the documented length and is writable for the call.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if status.ullTotalPhys == 0 || status.ullAvailPhys > status.ullTotalPhys {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid physical memory status",
        ));
    }
    Ok((status.ullTotalPhys, status.ullAvailPhys))
}

#[cfg(any(target_os = "macos", test))]
fn parse_total_bytes(text: &str) -> io::Result<u64> {
    text.trim()
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        .and_then(|bytes| {
            if bytes > 0 {
                Ok(bytes)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "total physical memory is zero",
                ))
            }
        })
}

#[cfg(any(target_os = "linux", target_os = "macos", test))]
fn field<'a>(text: &'a str, name: &str) -> io::Result<&'a str> {
    let mut values = text.lines().filter_map(|line| line.strip_prefix(name));
    let value = values
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("missing {name}")))?;
    if values.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("duplicate {name}"),
        ));
    }
    Ok(value.trim())
}

#[cfg(any(target_os = "linux", test))]
fn linux_available(text: &str) -> io::Result<u64> {
    linux_bytes(text, "MemAvailable:")
}

#[cfg(any(target_os = "linux", test))]
fn linux_bytes(text: &str, key: &str) -> io::Result<u64> {
    let mut value = field(text, key)?.split_whitespace();
    let kb = value
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("missing {key} value")))?
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if value.next() != Some("kB") || value.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{key} must use kB"),
        ));
    }
    kb.checked_mul(1024)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("{key} overflow")))
}

#[cfg(any(target_os = "macos", test))]
fn macos_available(text: &str) -> io::Result<u64> {
    let page_size = text
        .lines()
        .next()
        .and_then(|line| line.split_once("(page size of "))
        .and_then(|(_, value)| value.strip_suffix(" bytes)"))
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| value.is_power_of_two())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid vm_stat page size"))?;
    let mut pages = 0u64;
    // vm_stat subtracts speculative pages from its displayed free count.
    // Purgeable pages can overlap other categories and must not be added.
    for name in ["Pages free:", "Pages inactive:", "Pages speculative:"] {
        let value = field(text, name)?;
        let value = value.strip_suffix('.').unwrap_or(value);
        let count = value
            .parse::<u64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        pages = pages
            .checked_add(count)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "vm_stat page overflow"))?;
    }
    pages
        .checked_mul(page_size)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "vm_stat byte overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn total_memory_is_physical_and_rejects_invalid_observations() {
        assert_eq!(
            linux_bytes(
                "MemTotal: 4194304 kB\nSwapTotal: 99999999 kB\n",
                "MemTotal:"
            )
            .unwrap(),
            4 << 30
        );
        assert_eq!(parse_total_bytes("4294967296\n").unwrap(), 4 << 30);
        for text in ["", "0", "-1", "4096 kB", "18446744073709551616", "12\n13"] {
            assert!(parse_total_bytes(text).is_err(), "{text}");
        }
        assert!(linux_bytes("MemTotal: 1 kB\nMemTotal: 2 kB\n", "MemTotal:").is_err());
    }

    #[test]
    #[cfg(windows)]
    fn native_windows_observes_available_and_total_physical_memory() {
        let (total, available) = windows_memory().unwrap();
        assert!(total > 0 && available <= total);
        assert!(total_bytes().unwrap() > 0);
        assert!(available_bytes().unwrap() <= total);
    }

    #[test]
    fn linux_uses_available_not_total_or_free_memory() {
        let text = "MemTotal: 16777216 kB\nMemFree: 1024 kB\nMemAvailable: 8388608 kB\n";
        assert_eq!(linux_available(text).unwrap(), 8 * 1024 * 1024 * 1024);
        assert_eq!(linux_available("MemAvailable: 0 kB\n").unwrap(), 0);
    }

    #[test]
    fn linux_rejects_missing_malformed_units_duplicates_and_overflow() {
        for text in [
            "MemTotal: 1024 kB\n",
            "MemAvailable: -1 kB\n",
            "MemAvailable: 8 GB\n",
            "MemAvailable: 8 kB extra\n",
            "MemAvailable: 1 kB\nMemAvailable: 2 kB\n",
            "MemAvailable: 18446744073709551615 kB\n",
        ] {
            assert!(linux_available(text).is_err(), "{text}");
        }
    }

    #[test]
    fn macos_counts_reclaimable_pages_with_the_reported_page_size() {
        for page_size in [4096u64, 16384] {
            let text = format!(
                "Mach Virtual Memory Statistics: (page size of {page_size} bytes)\n\
                 Pages free: 10.\nPages inactive: 20.\nPages speculative: 3.\n\
                 Pages active: 100.\nPages purgeable: 999.\n"
            );
            assert_eq!(macos_available(&text).unwrap(), 33 * page_size);
        }
    }

    #[test]
    fn macos_rejects_incomplete_invalid_and_overflowing_observations() {
        for text in [
            "Mach Virtual Memory Statistics: (page size of 0 bytes)\n",
            "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages free: 1.\n",
            "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages free: 1..\nPages inactive: 1.\nPages speculative: 1.\n",
            "Mach Virtual Memory Statistics: (page size of 4096 bytes)\nPages free: 18446744073709551615.\nPages inactive: 1.\nPages speculative: 1.\n",
        ] {
            assert!(macos_available(text).is_err(), "{text}");
        }
    }
}
