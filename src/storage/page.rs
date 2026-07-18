use std::error::Error;
use std::fmt;

pub const PAGE_SIZE: usize = 8 * 1024;
pub const PAGE_HEADER_SIZE: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct PageId(u32);

impl PageId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }

    pub const fn file_offset(self) -> u64 {
        self.0 as u64 * PAGE_SIZE as u64
    }

    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }

    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
}

pub struct Page {
    id: PageId,
    bytes: Box<[u8; PAGE_SIZE]>,
    dirty: bool,
}

impl Page {
    pub fn zeroed(id: PageId) -> Self {
        Self {
            id,
            bytes: Box::new([0; PAGE_SIZE]),
            dirty: false,
        }
    }

    pub fn from_bytes(id: PageId, bytes: Box<[u8; PAGE_SIZE]>) -> Self {
        Self {
            id,
            bytes,
            dirty: false,
        }
    }

    pub const fn id(&self) -> PageId {
        self.id
    }

    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.bytes
    }

    pub fn read(&self, offset: usize, length: usize) -> Result<&[u8], PageError> {
        let range = checked_range(offset, length)?;
        Ok(&self.bytes[range])
    }

    pub fn write(&mut self, offset: usize, source: &[u8]) -> Result<(), PageError> {
        let range = checked_range(offset, source.len())?;
        self.bytes[range].copy_from_slice(source);
        self.dirty = true;
        Ok(())
    }

    pub fn read_u16(&self, offset: usize) -> Result<u16, PageError> {
        let bytes = self.read(offset, 2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    pub fn write_u16(&mut self, offset: usize, value: u16) -> Result<(), PageError> {
        self.write(offset, &value.to_le_bytes())
    }

    pub fn read_u32(&self, offset: usize) -> Result<u32, PageError> {
        let bytes = self.read(offset, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    pub fn write_u32(&mut self, offset: usize, value: u32) -> Result<(), PageError> {
        self.write(offset, &value.to_le_bytes())
    }
}

fn checked_range(offset: usize, length: usize) -> Result<std::ops::Range<usize>, PageError> {
    let end = offset
        .checked_add(length)
        .filter(|end| *end <= PAGE_SIZE)
        .ok_or(PageError::OutOfBounds { offset, length })?;
    Ok(offset..end)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageError {
    OutOfBounds { offset: usize, length: usize },
}

impl fmt::Display for PageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfBounds { offset, length } => write!(
                formatter,
                "page access at offset {offset} with length {length} exceeds {PAGE_SIZE} bytes"
            ),
        }
    }
}

impl Error for PageError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_id_maps_to_file_offset() {
        assert_eq!(PageId::new(3).file_offset(), 24 * 1024);
    }

    #[test]
    fn integer_access_is_little_endian_and_marks_page_dirty() {
        let mut page = Page::zeroed(PageId::new(1));
        page.write_u32(20, 0x1234_5678).unwrap();
        assert_eq!(&page.as_bytes()[20..24], &[0x78, 0x56, 0x34, 0x12]);
        assert_eq!(page.read_u32(20).unwrap(), 0x1234_5678);
        assert!(page.is_dirty());
    }

    #[test]
    fn rejects_access_beyond_page_boundary() {
        let mut page = Page::zeroed(PageId::new(0));
        assert!(page.write(PAGE_SIZE - 1, &[1, 2]).is_err());
        assert!(page.read(PAGE_SIZE, 1).is_err());
    }
}
