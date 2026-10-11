//! USB mass storage: USB sticks, card readers and external disks.
//!
//! Nearly all of them speak the "Bulk-Only Transport": every command is a
//! 31-byte Command Block Wrapper sent to the bulk OUT endpoint, carrying a
//! SCSI command; then the data, if any, on the bulk IN or OUT endpoint;
//! then a 13-byte Command Status Wrapper from the bulk IN endpoint saying
//! whether it worked. The SCSI commands are the ones every disk since the
//! 1980s understands: INQUIRY (who are you), TEST UNIT READY, READ
//! CAPACITY, READ and WRITE.
//!
//! The controller's thread finds the device and asks it these questions;
//! after that, the disk is used from whatever thread reads a file, through
//! [`UsbDisk`], which takes the controller's lock for each command.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use super::xhci::Controller;
use crate::drivers::block::Disk;
use crate::sync::Mutex;

const CBW_SIGNATURE: u32 = 0x4342_5355;
const CSW_SIGNATURE: u32 = 0x5342_5355;
pub const CBW_LENGTH: usize = 31;
pub const CSW_LENGTH: usize = 13;
/// The direction flag in a CBW: data goes from the device to us.
const CBW_IN: u8 = 0x80;

// SCSI operation codes.
pub const TEST_UNIT_READY: u8 = 0x00;
pub const REQUEST_SENSE: u8 = 0x03;
pub const INQUIRY: u8 = 0x12;
pub const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;
const READ_16: u8 = 0x88;
const WRITE_16: u8 = 0x8A;
pub const SERVICE_ACTION_IN: u8 = 0x9E;
pub const READ_CAPACITY_16: u8 = 0x10;

/// A Command Block Wrapper.
pub fn command_block(tag: u32, length: u32, input: bool, lun: u8, command: &[u8]) -> [u8; CBW_LENGTH] {
    let mut block = [0u8; CBW_LENGTH];
    block[0..4].copy_from_slice(&CBW_SIGNATURE.to_le_bytes());
    block[4..8].copy_from_slice(&tag.to_le_bytes());
    block[8..12].copy_from_slice(&length.to_le_bytes());
    block[12] = if input { CBW_IN } else { 0 };
    block[13] = lun;
    block[14] = command.len() as u8;
    block[15..15 + command.len()].copy_from_slice(command);
    block
}

/// How a command ended, from its Command Status Wrapper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Passed,
    /// The command failed; REQUEST SENSE says why.
    Failed,
    /// The device lost track of the protocol and needs a reset.
    PhaseError,
}

/// Checks a Command Status Wrapper against the command's tag.
pub fn command_status(status: &[u8], tag: u32) -> Option<Status> {
    let word = |at: usize| u32::from_le_bytes(status[at..at + 4].try_into().unwrap());
    if status.len() < CSW_LENGTH || word(0) != CSW_SIGNATURE || word(4) != tag {
        return None;
    }
    Some(match status[12] {
        0 => Status::Passed,
        1 => Status::Failed,
        _ => Status::PhaseError,
    })
}

/// READ or WRITE of `count` blocks at `block`: the 10-byte forms when they
/// reach, the 16-byte forms beyond 2 TiB.
pub fn read_write(write: bool, block: u64, count: u32) -> Vec<u8> {
    if block + u64::from(count) <= u64::from(u32::MAX) && count <= u32::from(u16::MAX) {
        let mut command = vec![if write { WRITE_10 } else { READ_10 }, 0];
        command.extend_from_slice(&(block as u32).to_be_bytes());
        command.push(0);
        command.extend_from_slice(&(count as u16).to_be_bytes());
        command.push(0);
        command
    } else {
        let mut command = vec![if write { WRITE_16 } else { READ_16 }, 0];
        command.extend_from_slice(&block.to_be_bytes());
        command.extend_from_slice(&count.to_be_bytes());
        command.extend_from_slice(&[0, 0]);
        command
    }
}

/// A disk (one logical unit of a USB storage device) the controller found,
/// waiting to be registered once the controller's lock is released.
pub struct Found {
    pub slot: u8,
    /// Which device in that slot (slots are reused).
    pub device: u64,
    pub lun: u8,
    pub block_size: u32,
    pub blocks: u64,
    pub model: String,
}

/// A USB disk, as the block layer sees it.
pub struct UsbDisk {
    name: String,
    model: String,
    controller: Arc<Mutex<Controller>>,
    slot: u8,
    device: u64,
    lun: u8,
    block_size: u32,
    sectors: u64,
}

impl UsbDisk {
    pub fn new(name: String, controller: Arc<Mutex<Controller>>, found: Found) -> Self {
        let sectors = found.blocks * u64::from(found.block_size) / 512;
        Self {
            name,
            model: found.model,
            controller,
            slot: found.slot,
            device: found.device,
            lun: found.lun,
            block_size: found.block_size,
            sectors,
        }
    }

    /// Reads or writes whole device blocks.
    fn transfer(&self, block: u64, data: &mut [u8], write: bool) -> Result<(), &'static str> {
        let mut controller = self.controller.lock();
        controller.storage_io(self.slot, self.device, self.lun, block, self.block_size, data, write)
    }
}

impl Disk for UsbDisk {
    fn name(&self) -> &str {
        &self.name
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn removable(&self) -> bool {
        true
    }

    /// Reads 512-byte sectors. A disk with bigger blocks (some 4 KiB-sector
    /// drives) has whole blocks read and the sectors taken out of them.
    fn read(&self, lba: u64, buffer: &mut [u8]) -> Result<(), &'static str> {
        let per_block = u64::from(self.block_size / 512);
        if per_block == 1 {
            return self.transfer(lba, buffer, false);
        }
        let first = lba / per_block;
        let last = (lba + (buffer.len() / 512) as u64).div_ceil(per_block);
        let mut blocks = vec![0u8; ((last - first) * u64::from(self.block_size)) as usize];
        self.transfer(first, &mut blocks, false)?;
        let skip = ((lba % per_block) * 512) as usize;
        buffer.copy_from_slice(&blocks[skip..skip + buffer.len()]);
        Ok(())
    }

    fn write(&self, lba: u64, data: &[u8]) -> Result<(), &'static str> {
        let per_block = u64::from(self.block_size / 512);
        if per_block == 1 {
            let mut copy = data.to_vec();
            return self.transfer(lba, &mut copy, true);
        }
        // Whole blocks: read the ones only partly written, change, write.
        let first = lba / per_block;
        let last = (lba + (data.len() / 512) as u64).div_ceil(per_block);
        let mut blocks = vec![0u8; ((last - first) * u64::from(self.block_size)) as usize];
        self.transfer(first, &mut blocks, false)?;
        let skip = ((lba % per_block) * 512) as usize;
        blocks[skip..skip + data.len()].copy_from_slice(data);
        self.transfer(first, &mut blocks, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::ktest! {
        fn command_blocks_follow_the_bulk_only_layout() {
            let block = command_block(7, 512, true, 0, &read_write(false, 0x1234, 1));
            assert_eq!(&block[..15], &[0x55, 0x53, 0x42, 0x43, 7, 0, 0, 0, 0, 2, 0, 0, 0x80, 0, 10]);
            assert_eq!(&block[15..25], &[READ_10, 0, 0, 0, 0x12, 0x34, 0, 0, 1, 0]);
            let mut status = [0u8; CSW_LENGTH];
            status[..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
            status[4] = 7;
            assert_eq!(command_status(&status, 7), Some(Status::Passed));
            assert_eq!(command_status(&status, 8), None);
            // Past 2 TiB of 512-byte blocks, the 16-byte form.
            assert_eq!(read_write(true, 1 << 33, 8)[0], WRITE_16);
        }
    }
}
