//! ZIP archives in RAM for host tests of firmware code that reads books.
use proto::epub::ReadAt;
use std::cell::{Cell, RefCell};

/// A ZIP in RAM that records where it was read, and fails reads that
/// start in `fails`, as a card's can.
pub struct Archive<'a> {
    pub bytes: &'a [u8],
    pub reads: &'a RefCell<Vec<(u32, usize)>>,
    pub fails: &'a Cell<Option<(u32, u32)>>,
}

impl ReadAt for Archive<'_> {
    type Error = ();

    fn len(&mut self) -> Result<u32, ()> {
        Ok(self.bytes.len() as u32)
    }

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<usize, ()> {
        if self
            .fails
            .get()
            .is_some_and(|(start, end)| (start..end).contains(&offset))
        {
            return Err(());
        }
        let rest = self.bytes.get(offset as usize..).unwrap_or(&[]);
        let count = out.len().min(rest.len());
        out[..count].copy_from_slice(&rest[..count]);
        self.reads.borrow_mut().push((offset, count));
        Ok(count)
    }
}

/// A ZIP of `files`: each stored, or deflated as one stored deflate block
/// (enough for the inflater to stream it), with where each entry's local
/// header and data sit.
pub fn zip(files: &[(&str, bool, &[u8])]) -> (Vec<u8>, Vec<(u32, u32)>) {
    zip_raw(
        &files
            .iter()
            .map(|(name, deflate, plain)| {
                let data = if *deflate {
                    let mut block = vec![0x01];
                    block.extend_from_slice(&(plain.len() as u16).to_le_bytes());
                    block.extend_from_slice(&(!(plain.len() as u16)).to_le_bytes());
                    block.extend_from_slice(plain);
                    block
                } else {
                    plain.to_vec()
                };
                (*name, u16::from(*deflate) * 8, data, plain.len() as u32)
            })
            .collect::<Vec<_>>(),
    )
}

/// A ZIP of entries already in their stored form: name, method, data, and
/// uncompressed length.
pub fn zip_raw(files: &[(&str, u16, Vec<u8>, u32)]) -> (Vec<u8>, Vec<(u32, u32)>) {
    let mut bytes = Vec::new();
    let mut central = Vec::new();
    let mut spans = Vec::new();
    let push16 = |out: &mut Vec<u8>, value: u16| out.extend_from_slice(&value.to_le_bytes());
    let push32 = |out: &mut Vec<u8>, value: u32| out.extend_from_slice(&value.to_le_bytes());
    for (name, method, data, plain_len) in files {
        let offset = bytes.len() as u32;
        push32(&mut bytes, 0x0403_4b50);
        for value in [20, 0, *method, 0, 0] {
            push16(&mut bytes, value);
        }
        push32(&mut bytes, 0);
        push32(&mut bytes, data.len() as u32);
        push32(&mut bytes, *plain_len);
        push16(&mut bytes, name.len() as u16);
        push16(&mut bytes, 0);
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(data);
        spans.push((offset, bytes.len() as u32));

        push32(&mut central, 0x0201_4b50);
        for value in [20, 20, 0, *method, 0, 0] {
            push16(&mut central, value);
        }
        push32(&mut central, 0);
        push32(&mut central, data.len() as u32);
        push32(&mut central, *plain_len);
        push16(&mut central, name.len() as u16);
        for _ in 0..4 {
            push16(&mut central, 0);
        }
        push32(&mut central, 0);
        push32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = bytes.len() as u32;
    bytes.extend_from_slice(&central);
    push32(&mut bytes, 0x0605_4b50);
    for value in [0, 0, files.len() as u16, files.len() as u16] {
        push16(&mut bytes, value);
    }
    push32(&mut bytes, central.len() as u32);
    push32(&mut bytes, central_offset);
    push16(&mut bytes, 0);
    (bytes, spans)
}
