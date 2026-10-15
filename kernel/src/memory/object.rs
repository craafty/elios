//! Memory objects: numbered pages of memory that can be mapped into
//! address spaces and read and written like a file.
//!
//! One object can back several things at once. An in-memory file's data
//! lives in one, so `read` and `write` and every program that maps the
//! file with `MAP_SHARED` see the same bytes. `memfd_create` and shared
//! anonymous memory (`MAP_SHARED | MAP_ANONYMOUS`) are objects with no
//! file at all. Files on disks get one as their *page cache*: pages are
//! read from the file the first time they're needed, and every program
//! that maps a library shares them instead of each having its own copy.
//!
//! Pages are created when first touched (zero-filled, or read from the
//! file behind a page cache). The object owns one reference to each of
//! its frames; a mapping adds its own (see [`frames::share`]), so a frame
//! stays alive while mapped even if the object drops it (on truncate).

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use super::{PAGE_SIZE, frames, phys_to_virt};
use crate::errno::{Errno, Result};
use crate::fs::Inode;
use crate::sync::SpinLock;

pub struct MemoryObject {
    /// Page index -> frame.
    pages: SpinLock<BTreeMap<u64, u64>>,
    /// The size in bytes, for files (pages past it read as end of file).
    size: AtomicU64,
    /// For a page cache: the file pages are read from.
    source: Option<Arc<dyn Inode>>,
}

impl MemoryObject {
    /// An empty object (an in-memory file, a memfd, shared anonymous
    /// memory).
    pub fn new() -> Arc<Self> {
        Arc::new(Self { pages: SpinLock::new(BTreeMap::new()), size: AtomicU64::new(0), source: None })
    }

    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Relaxed)
    }

    /// Whether getting a page may read from a disk.
    pub fn needs_io(&self) -> bool {
        self.source.is_some()
    }

    /// Bytes of memory its pages take.
    #[cfg_attr(not(test), allow(dead_code, reason = "used by tests"))]
    pub fn resident_bytes(&self) -> u64 {
        self.pages.lock().len() as u64 * PAGE_SIZE
    }

    /// The frame holding page `index`, creating it if it doesn't exist yet:
    /// zeroed, or read from the source file. The caller gets its own
    /// reference to the frame, and must [`frames::free`] it when done.
    ///
    /// May read from a disk, so it must not be called with spinlocks held.
    pub fn page(&self, index: u64) -> Result<u64> {
        if let Some(frame) = self.existing_page(index) {
            return Ok(frame);
        }
        let frame = frames::allocate_zeroed().ok_or(Errno::ENOMEM)?;
        if let Some(source) = &self.source {
            // SAFETY: a fresh frame, ours alone.
            let buffer = unsafe { core::slice::from_raw_parts_mut(phys_to_virt(frame), PAGE_SIZE as usize) };
            let mut done = 0;
            while done < buffer.len() {
                match source.read_at(index * PAGE_SIZE + done as u64, &mut buffer[done..]) {
                    Ok(0) => break,
                    Ok(count) => done += count,
                    Err(error) => {
                        frames::free(frame);
                        return Err(error);
                    }
                }
            }
        }
        let mut pages = self.pages.lock();
        if let Some(&existing) = pages.get(&index) {
            // Someone else made it meanwhile.
            frames::share(existing);
            drop(pages);
            frames::free(frame);
            return Ok(existing);
        }
        pages.insert(index, frame);
        frames::share(frame);
        Ok(frame)
    }

    /// Page `index` if it exists, with a reference for the caller.
    fn existing_page(&self, index: u64) -> Option<u64> {
        let pages = self.pages.lock();
        let frame = *pages.get(&index)?;
        frames::share(frame);
        Some(frame)
    }

    /// Reads from the object as a file, up to its size.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let size = self.size();
        if offset >= size {
            return Ok(0);
        }
        let count = buf.len().min((size - offset) as usize);
        let mut done = 0;
        while done < count {
            let position = offset + done as u64;
            let in_page = (position % PAGE_SIZE) as usize;
            let chunk = (PAGE_SIZE as usize - in_page).min(count - done);
            match self.existing_page(position / PAGE_SIZE) {
                Some(frame) => {
                    // SAFETY: we hold a reference to the frame, so it stays
                    // ours to read while copying.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            phys_to_virt(frame).add(in_page),
                            buf[done..].as_mut_ptr(),
                            chunk,
                        );
                    }
                    frames::free(frame);
                }
                // A hole: never written, so zeros.
                None => buf[done..done + chunk].fill(0),
            }
            done += chunk;
        }
        Ok(count)
    }

    /// Writes to the object as a file, growing it if needed.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<usize> {
        let mut done = 0;
        while done < data.len() {
            let position = offset + done as u64;
            let in_page = (position % PAGE_SIZE) as usize;
            let chunk = (PAGE_SIZE as usize - in_page).min(data.len() - done);
            let frame = self.page(position / PAGE_SIZE)?;
            // SAFETY: as in `read`.
            unsafe {
                core::ptr::copy_nonoverlapping(data[done..].as_ptr(), phys_to_virt(frame).add(in_page), chunk);
            }
            frames::free(frame);
            done += chunk;
        }
        self.size.fetch_max(offset + data.len() as u64, Ordering::Relaxed);
        Ok(data.len())
    }

    /// Sets the size: pages wholly past the end are dropped (programs that
    /// still map them keep their own references), and the rest of the last
    /// page is zeroed, so growing again reads zeros.
    pub fn truncate(&self, size: u64) {
        let first_gone = size.div_ceil(PAGE_SIZE);
        let dropped: Vec<u64> = {
            let mut pages = self.pages.lock();
            let doomed: Vec<u64> = pages.range(first_gone..).map(|(&index, _)| index).collect();
            doomed.iter().filter_map(|index| pages.remove(index)).collect()
        };
        dropped.into_iter().for_each(frames::free);
        if !size.is_multiple_of(PAGE_SIZE)
            && let Some(frame) = self.existing_page(size / PAGE_SIZE)
        {
            let in_page = (size % PAGE_SIZE) as usize;
            // SAFETY: we hold a reference to the frame.
            unsafe { core::ptr::write_bytes(phys_to_virt(frame).add(in_page), 0, PAGE_SIZE as usize - in_page) };
            frames::free(frame);
        }
        self.size.store(size, Ordering::Relaxed);
    }
}

impl Drop for MemoryObject {
    fn drop(&mut self) {
        for (_, frame) in core::mem::take(self.pages.get_mut()) {
            frames::free(frame);
        }
    }
}

/// Page caches of files, by (file system, inode number), kept while
/// something uses them.
static PAGE_CACHES: SpinLock<BTreeMap<(usize, u64), Weak<MemoryObject>>> = SpinLock::new(BTreeMap::new());

/// The page cache for a file that has no memory object of its own (one on
/// a disk), made on first use.
pub fn page_cache(file: &Arc<dyn Inode>) -> Arc<MemoryObject> {
    let key = (file.filesystem(), file.metadata().inode);
    let mut caches = PAGE_CACHES.lock();
    if let Some(object) = caches.get(&key).and_then(Weak::upgrade) {
        return object;
    }
    caches.retain(|_, object| object.strong_count() > 0);
    let object = Arc::new(MemoryObject {
        pages: SpinLock::new(BTreeMap::new()),
        size: AtomicU64::new(file.metadata().size),
        source: Some(file.clone()),
    });
    caches.insert(key, Arc::downgrade(&object));
    object
}

/// Forgets a file's page cache after the file was written or truncated,
/// so later mappings see the new contents. (Programs that already map it
/// keep the old pages, a simplification compared with Linux, which keeps
/// mappings and the file in step.)
pub fn file_changed(file: &dyn Inode) {
    let key = (file.filesystem(), file.metadata().inode);
    let mut caches = PAGE_CACHES.lock();
    if !caches.is_empty() {
        caches.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn objects_read_and_write_like_files() {
            let object = MemoryObject::new();
            assert_eq!(object.write(4090, b"across a page boundary").unwrap(), 22);
            assert_eq!(object.size(), 4112);
            let mut buffer = [0u8; 30];
            assert_eq!(object.read(4085, &mut buffer).unwrap(), 27);
            assert_eq!(&buffer[..5], &[0; 5]);
            assert_eq!(&buffer[5..27], b"across a page boundary");
            // Holes read as zeros.
            object.write(5 * PAGE_SIZE, b"x").unwrap();
            assert_eq!(object.read(3 * PAGE_SIZE, &mut buffer).unwrap(), 30);
            assert_eq!(buffer, [0; 30]);
        }

        fn truncating_drops_pages_and_zeroes_the_tail() {
            let before = frames::stats().free_bytes;
            let object = MemoryObject::new();
            object.write(0, &[0xAA; 3 * 4096]).unwrap();
            // A mapping's reference keeps a dropped page alive.
            let kept = object.page(2).unwrap();
            object.truncate(100);
            assert_eq!(object.resident_bytes(), PAGE_SIZE);
            object.truncate(8192);
            let mut buffer = [0xFFu8; 8];
            object.read(100, &mut buffer).unwrap();
            assert_eq!(buffer, [0; 8]);
            frames::free(kept);
            drop(object);
            assert_eq!(frames::stats().free_bytes, before);
        }
    }
}
