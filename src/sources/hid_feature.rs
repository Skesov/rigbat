//! HID feature reports on a hidraw node: the `HIDIOCSFEATURE` / `HIDIOCGFEATURE`
//! ioctls from `linux/hidraw.h`. The only `unsafe` in the crate lives here,
//! behind two safe functions that take an open `File` and a borrowed buffer.

#![expect(
    unsafe_code,
    reason = "hidraw feature reports are ioctls, which libc exposes only as unsafe calls"
)]

use std::{fs::File, io, os::fd::AsRawFd as _};

/// Well under the ioctl size field (13 bits on some architectures, 14 on x86/arm)
/// and above any feature report rigbat sends.
pub const MAX_REPORT_LEN: usize = 4096;

const HIDRAW_IOC_MAGIC: u8 = b'H';
const HIDRAW_SET_FEATURE: u8 = 0x06;
const HIDRAW_GET_FEATURE: u8 = 0x07;

nix::ioctl_readwrite_buf!(hidiocsfeature, HIDRAW_IOC_MAGIC, HIDRAW_SET_FEATURE, u8);
nix::ioctl_readwrite_buf!(hidiocgfeature, HIDRAW_IOC_MAGIC, HIDRAW_GET_FEATURE, u8);

/// Sends `report` as a feature report; `report[0]` is the report id (0 when the
/// device uses none).
pub fn set_feature(file: &File, report: &[u8]) -> io::Result<()> {
    check_len(report.len())?;
    let mut buf = report.to_vec();
    // SAFETY: the fd is borrowed from `file`, so it is open for the whole call.
    // The request code encodes `buf.len()` (bounded by `check_len`), and the
    // kernel copies at most that many bytes from `buf`, which owns them.
    unsafe { hidiocsfeature(file.as_raw_fd(), &mut buf) }?;
    Ok(())
}

/// Reads feature report `buf[0]` into `buf` and returns the number of bytes the
/// kernel wrote, report id included.
pub fn get_feature(file: &File, buf: &mut [u8]) -> io::Result<usize> {
    check_len(buf.len())?;
    // SAFETY: the fd is borrowed from `file`, so it is open for the whole call.
    // The request code encodes `buf.len()` (bounded by `check_len`), and the
    // kernel writes at most that many bytes into `buf`, a unique borrow.
    let written = unsafe { hidiocgfeature(file.as_raw_fd(), buf) }?;
    usize::try_from(written)
        .ok()
        .filter(|&n| n <= buf.len())
        .ok_or_else(|| io::Error::other(format!("HIDIOCGFEATURE returned {written}")))
}

fn check_len(len: usize) -> io::Result<()> {
    if (1..=MAX_REPORT_LEN).contains(&len) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("feature report of {len} bytes, expected 1..={MAX_REPORT_LEN}"),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_oversized_buffers_are_rejected_before_the_ioctl() {
        let file = File::open("/dev/null").unwrap();
        assert_eq!(
            set_feature(&file, &[]).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut big = vec![0u8; MAX_REPORT_LEN + 1];
        assert_eq!(
            get_feature(&file, &mut big).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn a_non_hidraw_fd_fails_cleanly() {
        let file = File::open("/dev/null").unwrap();
        let mut buf = [0u8; 91];
        assert!(get_feature(&file, &mut buf).is_err());
        assert!(set_feature(&file, &buf).is_err());
    }
}
