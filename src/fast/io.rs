//! Read-only file mappings. Mapped once, never unmapped (the process exits);
//! `release_pages` lets go of their page-table entries once they are read.

use std::fs::File;
use std::io;
use std::path::Path;

pub struct Mapped {
    ptr: *const u8,
    len: usize,
}

// SAFETY: the mapping is read-only and lives for the rest of the process.
unsafe impl Send for Mapped {}
unsafe impl Sync for Mapped {}

impl Mapped {
    pub fn open(path: &Path) -> io::Result<Mapped> {
        let file = File::open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Ok(Mapped { ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(), len: 0 });
        }
        let ptr = sys::map(&file, len)?;
        Ok(Mapped { ptr, len })
    }

    #[inline]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` points at `len` mapped, readable bytes (or is dangling with len 0).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Address and length of the mapping, for `release_pages`.
    pub fn range(&self) -> (usize, usize) {
        (self.ptr as usize, self.len)
    }
}

/// Lets go of the page-table entries of mapped files (address, length). The
/// bytes stay readable: a later read maps its pages again from the page
/// cache. Otherwise taking the entries down is the kernel's last job when the
/// process exits: page by page, on one thread, before anybody is told that
/// the process has ended.
pub fn release_pages(ranges: &[(usize, usize)]) {
    for &(start, len) in ranges {
        if len > 0 {
            sys::release(start, len);
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    extern "C" {
        fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut u8;
        fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    }
    const PROT_READ: i32 = 1;
    const MAP_PRIVATE: i32 = 2;
    const MADV_DONTNEED: i32 = 4;

    pub fn release(start: usize, len: usize) {
        // SAFETY: a private read-only file mapping made by `map` and never
        // unmapped. The advice only drops page-table entries; the result does
        // not matter (on failure the entries stay until the process exits).
        unsafe {
            madvise(start as *mut u8, len, MADV_DONTNEED);
        }
    }

    pub fn map(file: &File, len: usize) -> io::Result<*const u8> {
        // SAFETY: a fresh private read-only mapping of an open file.
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ, MAP_PRIVATE, file.as_raw_fd(), 0) };
        if ptr as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(ptr as *const u8)
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    type Handle = *mut core::ffi::c_void;
    extern "system" {
        fn CreateFileMappingW(
            file: Handle,
            attributes: *mut core::ffi::c_void,
            protect: u32,
            maximum_size_high: u32,
            maximum_size_low: u32,
            name: *const u16,
        ) -> Handle;
        fn MapViewOfFile(
            mapping: Handle,
            access: u32,
            offset_high: u32,
            offset_low: u32,
            bytes: usize,
        ) -> *mut core::ffi::c_void;
    }
    const PAGE_READONLY: u32 = 0x02;
    const FILE_MAP_READ: u32 = 0x0004;

    pub fn release(_start: usize, _len: usize) {}

    pub fn map(file: &File, _len: usize) -> io::Result<*const u8> {
        // SAFETY: plain Win32 calls on a valid file handle. The mapping handle is
        // leaked on purpose: the view lives for the rest of the process.
        unsafe {
            let mapping = CreateFileMappingW(
                file.as_raw_handle() as Handle,
                std::ptr::null_mut(),
                PAGE_READONLY,
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return Err(io::Error::last_os_error());
            }
            let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
            if view.is_null() {
                return Err(io::Error::last_os_error());
            }
            Ok(view as *const u8)
        }
    }
}
