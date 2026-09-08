//! Private journal storage. Reject symlinks and inherited/broad permissions on reopen.

use anyhow::{Result, ensure};
use std::{
    fs::{self, File},
    path::Path,
};

pub(crate) fn check(path: &Path, directory: bool) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        },
        "Unexpected session storage file type"
    );

    imp::check(path, &metadata)
}

pub(crate) fn directory(path: &Path, exclusive: bool) -> Result<()> {
    match imp::directory(path) {
        Ok(()) => (),
        Err(error) if !exclusive && error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(error.into()),
    }

    check(path, true)
}

pub(crate) fn file(path: &Path, create: bool) -> Result<File> {
    if !create {
        check(path, false)?;
    }

    let file = imp::file(path, create)?;
    check(path, false)?;

    Ok(file)
}

pub(crate) use imp::{rename, sync_directory};

#[cfg(unix)]
mod imp {
    use super::*;
    use std::{
        fs::{DirBuilder, OpenOptions},
        os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    };

    pub(super) fn check(_path: &Path, metadata: &fs::Metadata) -> Result<()> {
        // SAFETY: geteuid has no preconditions and returns the process identity.
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0,
            "Session storage must be owner-only"
        );

        Ok(())
    }

    pub(super) fn directory(path: &Path) -> std::io::Result<()> {
        DirBuilder::new().mode(0o700).create(path)
    }

    pub(super) fn file(path: &Path, create: bool) -> std::io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
    }

    pub(crate) fn sync_directory(path: &Path) -> std::io::Result<()> {
        File::open(path)?.sync_all()
    }

    pub(crate) fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
        fs::rename(from, to)
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::{
        os::windows::{ffi::OsStrExt, io::FromRawHandle},
        ptr::{null, null_mut},
    };

    use windows_sys::Win32::{
        Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE, LocalFree},
        Security::{
            Authorization::{
                ConvertSecurityDescriptorToStringSecurityDescriptorW,
                ConvertStringSecurityDescriptorToSecurityDescriptorW,
            },
            DACL_SECURITY_INFORMATION, GetFileSecurityW, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
        },
        Storage::FileSystem::{
            CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL,
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            MOVEFILE_WRITE_THROUGH, MoveFileExW, OPEN_EXISTING,
        },
    };

    struct Descriptor(PSECURITY_DESCRIPTOR);

    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn descriptor() -> std::io::Result<Descriptor> {
        // Protected DACL: full access only for the object's owner, no inherited grants.
        let text: Vec<u16> = "D:P(A;;FA;;;OW)\0".encode_utf16().collect();
        let mut pointer = null_mut();

        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                text.as_ptr(),
                1,
                &mut pointer,
                null_mut(),
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Descriptor(pointer))
    }

    fn text(descriptor: PSECURITY_DESCRIPTOR) -> std::io::Result<Vec<u16>> {
        let mut pointer = null_mut();
        let mut length = 0;
        if unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor,
                1,
                DACL_SECURITY_INFORMATION,
                &mut pointer,
                &mut length,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        let value = unsafe { std::slice::from_raw_parts(pointer, length as usize).to_vec() };
        unsafe {
            LocalFree(pointer.cast());
        }

        Ok(value)
    }

    pub(super) fn check(path: &Path, _: &fs::Metadata) -> Result<()> {
        let path = wide(path);
        let mut length = 0;

        unsafe {
            GetFileSecurityW(
                path.as_ptr(),
                DACL_SECURITY_INFORMATION,
                null_mut(),
                0,
                &mut length,
            );
        }

        ensure!(length > 0 && length <= 65536, "Cannot inspect session DACL");
        let mut buffer = vec![0u32; (length as usize).div_ceil(4)];

        ensure!(
            unsafe {
                GetFileSecurityW(
                    path.as_ptr(),
                    DACL_SECURITY_INFORMATION,
                    buffer.as_mut_ptr().cast(),
                    length,
                    &mut length,
                )
            } != 0,
            "Cannot read session DACL"
        );

        ensure!(
            text(buffer.as_mut_ptr().cast())? == text(descriptor()?.0)?,
            "Session storage must have a protected owner-only DACL"
        );

        Ok(())
    }
    pub(super) fn directory(path: &Path) -> std::io::Result<()> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };

        if unsafe { CreateDirectoryW(wide(path).as_ptr(), &attributes) } == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }

    pub(super) fn file(path: &Path, create: bool) -> std::io::Result<File> {
        let descriptor = descriptor()?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };

        let handle = unsafe {
            CreateFileW(
                wide(path).as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                if create { &attributes } else { null() },
                if create { CREATE_NEW } else { OPEN_EXISTING },
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                null_mut(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: CreateFileW returned a new owned handle; File closes it exactly once.
        Ok(unsafe { File::from_raw_handle(handle) })
    }

    pub(crate) fn sync_directory(_: &Path) -> std::io::Result<()> {
        Ok(())
    }

    pub(crate) fn rename(from: &Path, to: &Path) -> std::io::Result<()> {
        if unsafe {
            MoveFileExW(
                wide(from).as_ptr(),
                wide(to).as_ptr(),
                MOVEFILE_WRITE_THROUGH,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }
}
