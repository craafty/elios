//! Hand-written bindings for the parts of the UEFI specification eliboot uses.
//!
//! Layouts follow the UEFI 2.10 specification. Function pointers we never call
//! are `usize` placeholders: they only need to occupy the right slot so the
//! fields after them land at the right offsets.

use core::ffi::c_void;
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};

pub type Handle = *mut c_void;

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Status(pub usize);

impl Status {
    const ERROR_BIT: usize = 1 << (usize::BITS - 1);

    pub const BUFFER_TOO_SMALL: Self = Self(Self::ERROR_BIT | 5);

    pub fn is_error(self) -> bool {
        self.0 & Self::ERROR_BIT != 0
    }

    pub fn check(self, context: &'static str) -> Result<(), Error> {
        if self.is_error() { Err(Error::uefi(context, self)) } else { Ok(()) }
    }

    fn name(self) -> Option<&'static str> {
        if !self.is_error() {
            return (self.0 == 0).then_some("SUCCESS");
        }
        Some(match self.0 & !Self::ERROR_BIT {
            1 => "LOAD_ERROR",
            2 => "INVALID_PARAMETER",
            3 => "UNSUPPORTED",
            4 => "BAD_BUFFER_SIZE",
            5 => "BUFFER_TOO_SMALL",
            6 => "NOT_READY",
            7 => "DEVICE_ERROR",
            8 => "WRITE_PROTECTED",
            9 => "OUT_OF_RESOURCES",
            14 => "NOT_FOUND",
            15 => "ACCESS_DENIED",
            _ => return None,
        })
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "{:#x}", self.0),
        }
    }
}

/// Why booting failed: what we were doing, and the firmware's status code if
/// the firmware is the one that failed.
pub struct Error {
    context: &'static str,
    status: Option<Status>,
}

impl Error {
    pub fn new(context: &'static str) -> Self {
        Self { context, status: None }
    }

    pub fn uefi(context: &'static str, status: Status) -> Self {
        Self { context, status: Some(status) }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(self.context)?;
        match self.status {
            Some(status) => write!(f, " (status {status})"),
            None => Ok(()),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Guid(pub u32, pub u16, pub u16, pub [u8; 8]);

pub const LOADED_IMAGE_PROTOCOL: Guid =
    Guid(0x5B1B31A1, 0x9562, 0x11D2, [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B]);
pub const SIMPLE_FILE_SYSTEM_PROTOCOL: Guid =
    Guid(0x964E5B22, 0x6459, 0x11D2, [0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B]);
pub const GRAPHICS_OUTPUT_PROTOCOL: Guid =
    Guid(0x9042A9DE, 0x23DC, 0x4A38, [0x96, 0xFB, 0x7A, 0xDE, 0xD0, 0x80, 0x51, 0x6A]);
pub const DEVICE_PATH_PROTOCOL: Guid =
    Guid(0x09576E91, 0x6D3F, 0x11D2, [0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B]);
pub const FILE_INFO: Guid = Guid(0x09576E92, 0x6D3F, 0x11D2, [0x8E, 0x39, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B]);
pub const ACPI_20_TABLE: Guid = Guid(0x8868E871, 0xE4F1, 0x11D3, [0xBC, 0x22, 0x00, 0x80, 0xC7, 0x3C, 0x88, 0x34]);
pub const ACPI_10_TABLE: Guid = Guid(0xEB9D2D30, 0x2D88, 0x11D3, [0x9A, 0x16, 0x00, 0x90, 0x27, 0x3F, 0xC1, 0x4D]);

#[repr(C)]
pub struct TableHeader {
    pub signature: u64,
    pub revision: u32,
    pub header_size: u32,
    pub crc32: u32,
    pub reserved: u32,
}

#[repr(C)]
pub struct SystemTable {
    pub hdr: TableHeader,
    pub firmware_vendor: *const u16,
    pub firmware_revision: u32,
    pub console_in_handle: Handle,
    pub con_in: *mut c_void,
    pub console_out_handle: Handle,
    pub con_out: *mut SimpleTextOutput,
    pub standard_error_handle: Handle,
    pub std_err: *mut SimpleTextOutput,
    pub runtime_services: *mut c_void,
    pub boot_services: *mut BootServices,
    pub number_of_table_entries: usize,
    pub configuration_table: *const ConfigurationTable,
}

impl SystemTable {
    pub fn configuration_tables(&self) -> &[ConfigurationTable] {
        // SAFETY: the firmware provides a valid array of this length.
        unsafe { core::slice::from_raw_parts(self.configuration_table, self.number_of_table_entries) }
    }
}

#[repr(C)]
pub struct ConfigurationTable {
    pub vendor_guid: Guid,
    pub vendor_table: *mut c_void,
}

#[repr(C)]
pub struct SimpleTextOutput {
    pub reset: unsafe extern "efiapi" fn(*mut Self, bool) -> Status,
    pub output_string: unsafe extern "efiapi" fn(*mut Self, *const u16) -> Status,
    test_string: usize,
    query_mode: usize,
    set_mode: usize,
    pub set_attribute: unsafe extern "efiapi" fn(*mut Self, usize) -> Status,
    pub clear_screen: unsafe extern "efiapi" fn(*mut Self) -> Status,
    set_cursor_position: usize,
    enable_cursor: usize,
    mode: *mut c_void,
}

#[repr(C)]
pub struct BootServices {
    pub hdr: TableHeader,
    raise_tpl: usize,
    restore_tpl: usize,
    pub allocate_pages: unsafe extern "efiapi" fn(AllocateType, MemoryType, usize, *mut u64) -> Status,
    pub free_pages: unsafe extern "efiapi" fn(u64, usize) -> Status,
    pub get_memory_map:
        unsafe extern "efiapi" fn(*mut usize, *mut MemoryDescriptor, *mut usize, *mut usize, *mut u32) -> Status,
    allocate_pool: usize,
    free_pool: usize,
    create_event: usize,
    set_timer: usize,
    wait_for_event: usize,
    signal_event: usize,
    close_event: usize,
    check_event: usize,
    install_protocol_interface: usize,
    reinstall_protocol_interface: usize,
    uninstall_protocol_interface: usize,
    pub handle_protocol: unsafe extern "efiapi" fn(Handle, *const Guid, *mut *mut c_void) -> Status,
    reserved: usize,
    register_protocol_notify: usize,
    locate_handle: usize,
    locate_device_path: usize,
    install_configuration_table: usize,
    load_image: usize,
    start_image: usize,
    exit: usize,
    unload_image: usize,
    pub exit_boot_services: unsafe extern "efiapi" fn(Handle, usize) -> Status,
    get_next_monotonic_count: usize,
    pub stall: unsafe extern "efiapi" fn(usize) -> Status,
    pub set_watchdog_timer: unsafe extern "efiapi" fn(usize, u64, usize, *const u16) -> Status,
    connect_controller: usize,
    disconnect_controller: usize,
    open_protocol: usize,
    close_protocol: usize,
    open_protocol_information: usize,
    protocols_per_handle: usize,
    locate_handle_buffer: usize,
    pub locate_protocol: unsafe extern "efiapi" fn(*const Guid, *mut c_void, *mut *mut c_void) -> Status,
    // The table continues, but eliboot uses nothing beyond this point.
}

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AllocateType(pub u32);

impl AllocateType {
    pub const ANY_PAGES: Self = Self(0);
}

#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MemoryType(pub u32);

impl MemoryType {
    pub const RESERVED: Self = Self(0);
    pub const LOADER_CODE: Self = Self(1);
    pub const LOADER_DATA: Self = Self(2);
    pub const BOOT_SERVICES_CODE: Self = Self(3);
    pub const BOOT_SERVICES_DATA: Self = Self(4);
    pub const RUNTIME_SERVICES_CODE: Self = Self(5);
    pub const RUNTIME_SERVICES_DATA: Self = Self(6);
    pub const CONVENTIONAL: Self = Self(7);
    pub const UNUSABLE: Self = Self(8);
    pub const ACPI_RECLAIM: Self = Self(9);
    pub const ACPI_NVS: Self = Self(10);
    pub const MMIO: Self = Self(11);
    pub const MMIO_PORT_SPACE: Self = Self(12);
    pub const PERSISTENT: Self = Self(14);
}

#[repr(C)]
pub struct MemoryDescriptor {
    pub ty: MemoryType,
    pub physical_start: u64,
    pub virtual_start: u64,
    pub number_of_pages: u64,
    pub attribute: u64,
}

#[repr(C)]
pub struct LoadedImage {
    pub revision: u32,
    pub parent_handle: Handle,
    pub system_table: *mut SystemTable,
    pub device_handle: Handle,
    pub file_path: *mut c_void,
    reserved: *mut c_void,
    pub load_options_size: u32,
    pub load_options: *mut c_void,
    pub image_base: *mut c_void,
    pub image_size: u64,
    pub image_code_type: MemoryType,
    pub image_data_type: MemoryType,
    unload: usize,
}

#[repr(C)]
pub struct SimpleFileSystem {
    pub revision: u64,
    pub open_volume: unsafe extern "efiapi" fn(*mut Self, *mut *mut File) -> Status,
}

pub const FILE_MODE_READ: u64 = 1;

#[repr(C)]
pub struct File {
    pub revision: u64,
    pub open: unsafe extern "efiapi" fn(*mut Self, *mut *mut Self, *const u16, u64, u64) -> Status,
    pub close: unsafe extern "efiapi" fn(*mut Self) -> Status,
    delete: usize,
    pub read: unsafe extern "efiapi" fn(*mut Self, *mut usize, *mut u8) -> Status,
    write: usize,
    get_position: usize,
    set_position: usize,
    pub get_info: unsafe extern "efiapi" fn(*mut Self, *const Guid, *mut usize, *mut u8) -> Status,
    set_info: usize,
    flush: usize,
}

/// Header of `EFI_FILE_INFO`; the variable-length file name follows it.
#[repr(C)]
pub struct FileInfo {
    pub size: u64,
    pub file_size: u64,
    pub physical_size: u64,
    pub create_time: [u8; 16],
    pub last_access_time: [u8; 16],
    pub modification_time: [u8; 16],
    pub attribute: u64,
}

#[repr(C)]
pub struct GraphicsOutput {
    query_mode: usize,
    set_mode: usize,
    blt: usize,
    pub mode: *const GraphicsMode,
}

#[repr(C)]
pub struct GraphicsMode {
    pub max_mode: u32,
    pub mode: u32,
    pub info: *const GraphicsModeInfo,
    pub size_of_info: usize,
    pub frame_buffer_base: u64,
    pub frame_buffer_size: usize,
}

#[repr(C)]
pub struct GraphicsModeInfo {
    pub version: u32,
    pub horizontal_resolution: u32,
    pub vertical_resolution: u32,
    pub pixel_format: u32,
    pub pixel_bitmask: [u32; 4],
    pub pixels_per_scan_line: u32,
}

pub const PIXEL_RGB_RESERVED_8BIT: u32 = 0;
pub const PIXEL_BGR_RESERVED_8BIT: u32 = 1;

static SYSTEM_TABLE: AtomicPtr<SystemTable> = AtomicPtr::new(ptr::null_mut());

/// # Safety
/// `system_table` must be the pointer the firmware passed to `efi_main`.
pub unsafe fn init(system_table: *mut SystemTable) {
    SYSTEM_TABLE.store(system_table, Ordering::Release);
}

/// The system table, or `None` once boot services have been exited.
pub fn system_table() -> Option<&'static SystemTable> {
    // SAFETY: the pointer is either null or the firmware's system table.
    unsafe { SYSTEM_TABLE.load(Ordering::Acquire).as_ref() }
}

pub fn boot_services() -> &'static BootServices {
    let system_table = system_table().expect("UEFI boot services used after ExitBootServices");
    // SAFETY: boot services stay valid until ExitBootServices, and we forget
    // the system table at that point.
    unsafe { &*system_table.boot_services }
}

/// Called after ExitBootServices succeeds, so that nothing (including the
/// console) calls into firmware that is no longer there.
pub fn forget_system_table() {
    SYSTEM_TABLE.store(ptr::null_mut(), Ordering::Release);
}
