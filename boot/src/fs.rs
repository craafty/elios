//! Reading files from the volume eliboot itself was loaded from.

use core::ffi::c_void;
use core::ptr;

use crate::memory::{self, PAGE_SIZE};
use crate::uefi::{
    self, Error, FILE_INFO, FILE_MODE_READ, File, FileInfo, Handle, LOADED_IMAGE_PROTOCOL, LoadedImage, MemoryType,
    SIMPLE_FILE_SYSTEM_PROTOCOL, SimpleFileSystem,
};

/// Reads a whole file into freshly allocated pages. `path` uses `\` as the
/// separator and is relative to the root of the boot volume.
pub fn read_file(image: Handle, path: &str) -> Result<&'static [u8], Error> {
    let bs = uefi::boot_services();

    let mut loaded_image: *mut LoadedImage = ptr::null_mut();
    // SAFETY: firmware calls with valid out-pointers; the protocols they
    // return stay valid while boot services run.
    unsafe {
        (bs.handle_protocol)(image, &LOADED_IMAGE_PROTOCOL, (&raw mut loaded_image).cast::<*mut c_void>())
            .check("could not find eliboot's own image")?;

        let mut file_system: *mut SimpleFileSystem = ptr::null_mut();
        (bs.handle_protocol)(
            (*loaded_image).device_handle,
            &SIMPLE_FILE_SYSTEM_PROTOCOL,
            (&raw mut file_system).cast::<*mut c_void>(),
        )
        .check("boot device has no readable file system")?;

        let mut root: *mut File = ptr::null_mut();
        ((*file_system).open_volume)(file_system, &mut root).check("could not open boot volume")?;

        let result = read_from(root, path);
        ((*root).close)(root);
        result
    }
}

/// # Safety
/// `root` must be an open directory handle.
unsafe fn read_from(root: *mut File, path: &str) -> Result<&'static [u8], Error> {
    let mut path_utf16 = [0u16; 256];
    for (i, unit) in path.encode_utf16().enumerate() {
        // Keep the last slot for the terminating NUL.
        if i + 1 >= path_utf16.len() {
            return Err(Error::new("file path too long"));
        }
        path_utf16[i] = unit;
    }

    unsafe {
        let mut file: *mut File = ptr::null_mut();
        ((*root).open)(root, &mut file, path_utf16.as_ptr(), FILE_MODE_READ, 0).check("could not open kernel file")?;
        let result = read_all(file);
        ((*file).close)(file);
        result
    }
}

/// # Safety
/// `file` must be an open file handle.
unsafe fn read_all(file: *mut File) -> Result<&'static [u8], Error> {
    unsafe {
        // FileInfo is followed by the file name, so leave plenty of room.
        let mut info = [0u64; 128];
        let mut info_size = size_of_val(&info);
        ((*file).get_info)(file, &FILE_INFO, &mut info_size, info.as_mut_ptr().cast())
            .check("could not get kernel file size")?;
        let size = (*info.as_ptr().cast::<FileInfo>()).file_size as usize;

        let buffer = memory::allocate_pages(MemoryType::LOADER_DATA, size.div_ceil(PAGE_SIZE as usize))? as *mut u8;
        let mut read = size;
        ((*file).read)(file, &mut read, buffer).check("could not read kernel file")?;
        if read != size {
            return Err(Error::new("kernel file was shorter than reported"));
        }
        Ok(core::slice::from_raw_parts(buffer, size))
    }
}
