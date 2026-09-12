//! Bounded bitmap working set. FAT reads stay with the font-file adapter;
//! incomplete reads never become cache hits.

const ARENA_BYTES: usize = 2048;
const SLOTS: usize = 32;

#[derive(Clone, Copy)]
struct Entry {
    offset: u32,
    start: u16,
    len: u16,
}

const EMPTY: Entry = Entry {
    offset: 0,
    start: 0,
    len: 0,
};

pub(super) struct BitmapCache {
    identity: u64,
    bytes: [u8; ARENA_BYTES],
    entries: [Entry; SLOTS],
    next_byte: usize,
    next_slot: usize,
}

impl BitmapCache {
    pub(super) const fn new() -> Self {
        Self {
            identity: 0,
            bytes: [0; ARENA_BYTES],
            entries: [EMPTY; SLOTS],
            next_byte: 0,
            next_slot: 0,
        }
    }

    pub(super) fn get_or_load(
        &mut self,
        identity: u64,
        offset: u32,
        len: usize,
        load: impl FnOnce(&mut [u8]) -> Option<()>,
    ) -> Option<&[u8]> {
        if len == 0 {
            return Some(&[]);
        }
        if len > ARENA_BYTES {
            return None;
        }
        if self.identity != identity {
            self.entries.fill(EMPTY);
            self.next_byte = 0;
            self.next_slot = 0;
            self.identity = identity;
        }
        if let Some(entry) = self
            .entries
            .iter()
            .find(|e| e.offset == offset && usize::from(e.len) == len)
        {
            let start = usize::from(entry.start);
            return Some(&self.bytes[start..start + len]);
        }
        let start = if self.next_byte + len > ARENA_BYTES {
            0
        } else {
            self.next_byte
        };
        let end = start + len;
        // Invalidate before loading: even a failed read may overwrite bytes.
        for entry in &mut self.entries {
            let occupied_start = usize::from(entry.start);
            let occupied_end = occupied_start + usize::from(entry.len);
            if occupied_start < end && start < occupied_end {
                *entry = EMPTY;
            }
        }
        load(&mut self.bytes[start..end])?;
        let slot = self
            .entries
            .iter()
            .position(|e| e.len == 0)
            .unwrap_or(self.next_slot);
        self.entries[slot] = Entry {
            offset,
            start: start as u16,
            len: len as u16,
        };
        self.next_slot = (slot + 1) % SLOTS;
        self.next_byte = end;
        Some(&self.bytes[start..end])
    }
}
