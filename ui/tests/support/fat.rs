//! In-memory FAT16 adapter exercising the pinned firmware filesystem code.
use embedded_sdmmc::{Block, BlockCount, BlockDevice, BlockIdx, TimeSource, Timestamp};
use std::cell::{Cell, RefCell};

pub struct Disk {
    pub bytes: RefCell<Vec<u8>>,
    pub fail_after: Cell<Option<usize>>,
    pub writes: Cell<usize>,
    pub fail_read_after: Cell<Option<usize>>,
    pub reads: Cell<usize>,
}
impl Disk {
    pub fn new() -> Self {
        let mut bytes = vec![0; 16385 * 512];
        // MBR: one 8 MiB FAT16 partition starting at sector 1.
        bytes[446 + 4] = 0x06;
        bytes[454..458].copy_from_slice(&1u32.to_le_bytes());
        bytes[458..462].copy_from_slice(&16384u32.to_le_bytes());
        bytes[510..512].copy_from_slice(&[0x55, 0xaa]);
        let boot = &mut bytes[512..1024];
        boot[0..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
        boot[3..11].copy_from_slice(b"MSDOS5.0");
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&1u16.to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&512u16.to_le_bytes());
        boot[19..21].copy_from_slice(&16384u16.to_le_bytes());
        boot[21] = 0xf8;
        boot[22..24].copy_from_slice(&64u16.to_le_bytes());
        boot[24..26].copy_from_slice(&32u16.to_le_bytes());
        boot[26..28].copy_from_slice(&64u16.to_le_bytes());
        boot[28..32].copy_from_slice(&1u32.to_le_bytes());
        boot[38] = 0x29;
        boot[43..54].copy_from_slice(b"TEST VOLUME");
        boot[54..62].copy_from_slice(b"FAT16   ");
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        for start in [2 * 512, 66 * 512] {
            bytes[start..start + 4].copy_from_slice(&[0xf8, 0xff, 0xff, 0xff]);
        }
        Self {
            bytes: RefCell::new(bytes),
            fail_after: Cell::new(None),
            writes: Cell::new(0),
            fail_read_after: Cell::new(None),
            reads: Cell::new(0),
        }
    }
}
impl BlockDevice for &Disk {
    type Error = &'static str;
    fn read(&self, blocks: &mut [Block], first: BlockIdx) -> Result<(), Self::Error> {
        if let Some(left) = self.fail_read_after.get() {
            if left == 0 {
                return Err("injected read failure");
            }
            self.fail_read_after.set(Some(left - 1));
        }
        self.reads.set(self.reads.get() + 1);
        let bytes = self.bytes.borrow();
        for (i, block) in blocks.iter_mut().enumerate() {
            let offset = (first.0 as usize + i) * 512;
            block
                .contents
                .copy_from_slice(bytes.get(offset..offset + 512).ok_or("read bounds")?);
        }
        Ok(())
    }
    fn write(&self, blocks: &[Block], first: BlockIdx) -> Result<(), Self::Error> {
        for (i, block) in blocks.iter().enumerate() {
            if let Some(left) = self.fail_after.get() {
                if left == 0 {
                    return Err("injected power loss");
                }
                self.fail_after.set(Some(left - 1));
            }
            let offset = (first.0 as usize + i) * 512;
            self.bytes
                .borrow_mut()
                .get_mut(offset..offset + 512)
                .ok_or("write bounds")?
                .copy_from_slice(&block.contents);
            self.writes.set(self.writes.get() + 1);
        }
        Ok(())
    }
    fn num_blocks(&self) -> Result<BlockCount, Self::Error> {
        Ok(BlockCount((self.bytes.borrow().len() / 512) as u32))
    }
}
pub struct Clock;
impl TimeSource for Clock {
    fn get_timestamp(&self) -> Timestamp {
        Timestamp {
            year_since_1970: 56,
            zero_indexed_month: 8,
            zero_indexed_day: 11,
            hours: 12,
            minutes: 0,
            seconds: 0,
        }
    }
}
