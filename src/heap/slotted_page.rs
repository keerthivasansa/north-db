use std::error::Error;
use std::fmt;

use crate::storage::{Page, PageError, PageId, Rid, SlotId, PAGE_HEADER_SIZE, PAGE_SIZE};

pub const SLOT_ENTRY_SIZE: usize = 8;

const MAGIC: &[u8; 4] = b"NHPG";
const FORMAT_VERSION: u8 = 1;
const HEAP_PAGE_KIND: u8 = 1;
const INVALID_PAGE_ID: u32 = u32::MAX;

const MAGIC_OFFSET: usize = 0;
const VERSION_OFFSET: usize = 4;
const PAGE_KIND_OFFSET: usize = 5;
const FLAGS_OFFSET: usize = 6;
const SLOT_COUNT_OFFSET: usize = 8;
const FREE_START_OFFSET: usize = 10;
const FREE_END_OFFSET: usize = 12;
const LIVE_COUNT_OFFSET: usize = 14;
const NEXT_PAGE_ID_OFFSET: usize = 16;
const PAGE_LSN_OFFSET: usize = 20;
const CHECKSUM_OFFSET: usize = 28;

const SLOT_RECORD_OFFSET: usize = 0;
const SLOT_RECORD_LENGTH: usize = 2;
const SLOT_GENERATION: usize = 4;
const SLOT_FLAGS: usize = 6;

const SLOT_LIVE: u16 = 1;
const SLOT_RETIRED: u16 = 2;

/// An owned 8 KiB heap page with a slot directory growing from the front and
/// row data growing from the back.
pub struct SlottedPage {
    page: Page,
}

impl SlottedPage {
    pub fn new(id: PageId) -> Self {
        let mut page = Page::zeroed(id);
        page.write(MAGIC_OFFSET, MAGIC)
            .expect("heap-page magic is in bounds");
        page.write(VERSION_OFFSET, &[FORMAT_VERSION])
            .expect("heap-page version is in bounds");
        page.write(PAGE_KIND_OFFSET, &[HEAP_PAGE_KIND])
            .expect("heap-page kind is in bounds");
        page.write_u16(FLAGS_OFFSET, 0)
            .expect("heap-page flags are in bounds");
        page.write_u16(SLOT_COUNT_OFFSET, 0)
            .expect("heap-page slot count is in bounds");
        page.write_u16(FREE_START_OFFSET, PAGE_HEADER_SIZE as u16)
            .expect("heap-page free start is in bounds");
        page.write_u16(FREE_END_OFFSET, PAGE_SIZE as u16)
            .expect("heap-page free end is in bounds");
        page.write_u16(LIVE_COUNT_OFFSET, 0)
            .expect("heap-page live count is in bounds");
        page.write_u32(NEXT_PAGE_ID_OFFSET, INVALID_PAGE_ID)
            .expect("heap-page link is in bounds");
        page.write(PAGE_LSN_OFFSET, &[0; 8])
            .expect("heap-page LSN is in bounds");
        page.write_u32(CHECKSUM_OFFSET, 0)
            .expect("heap-page checksum is in bounds");
        Self { page }
    }

    pub fn from_page(page: Page) -> Result<Self, SlottedPageError> {
        let slotted = Self { page };
        slotted.validate()?;
        Ok(slotted)
    }

    pub fn into_page(self) -> Page {
        self.page
    }

    pub fn page(&self) -> &Page {
        &self.page
    }

    pub fn page_id(&self) -> PageId {
        self.page.id()
    }

    pub fn slot_count(&self) -> u16 {
        self.header_u16(SLOT_COUNT_OFFSET)
    }

    pub fn live_count(&self) -> u16 {
        self.header_u16(LIVE_COUNT_OFFSET)
    }

    pub fn contiguous_free_space(&self) -> usize {
        self.free_end() - self.free_start()
    }

    pub fn next_page_id(&self) -> Option<PageId> {
        let raw = self
            .page
            .read_u32(NEXT_PAGE_ID_OFFSET)
            .expect("validated heap-page link is readable");
        (raw != INVALID_PAGE_ID).then(|| PageId::new(raw))
    }

    pub fn set_next_page_id(&mut self, next: Option<PageId>) {
        let raw = next.map_or(INVALID_PAGE_ID, PageId::get);
        self.page
            .write_u32(NEXT_PAGE_ID_OFFSET, raw)
            .expect("heap-page link is in bounds");
    }

    pub fn insert(&mut self, record: &[u8]) -> Result<Rid, SlottedPageError> {
        let record_length = u16::try_from(record.len())
            .map_err(|_| SlottedPageError::RecordTooLarge { size: record.len() })?;
        let reusable_slot = self.find_reusable_slot();
        let needs_new_slot = reusable_slot.is_none();
        let directory_growth = usize::from(needs_new_slot) * SLOT_ENTRY_SIZE;
        let required = record.len() + directory_growth;

        if self.contiguous_free_space() < required {
            if self.free_space_after_compaction(directory_growth)? < record.len() {
                return Err(SlottedPageError::NoSpace {
                    required,
                    available: self.free_space_after_compaction(0)?,
                });
            }
            self.compact()?;
        }

        if self.contiguous_free_space() < required {
            return Err(SlottedPageError::NoSpace {
                required,
                available: self.contiguous_free_space(),
            });
        }

        let slot_id = reusable_slot.unwrap_or_else(|| SlotId::new(self.slot_count()));
        let generation = if needs_new_slot {
            1
        } else {
            self.slot_u16(slot_id, SLOT_GENERATION)
        };

        let record_offset = self.free_end() - record.len();
        self.page.write(record_offset, record)?;
        self.write_slot_u16(slot_id, SLOT_RECORD_OFFSET, record_offset as u16)?;
        self.write_slot_u16(slot_id, SLOT_RECORD_LENGTH, record_length)?;
        self.write_slot_u16(slot_id, SLOT_GENERATION, generation)?;
        self.write_slot_u16(slot_id, SLOT_FLAGS, SLOT_LIVE)?;

        if needs_new_slot {
            let slot_count = self
                .slot_count()
                .checked_add(1)
                .ok_or(SlottedPageError::Corrupt("slot count overflow"))?;
            self.write_header_u16(SLOT_COUNT_OFFSET, slot_count)?;
            self.write_header_u16(
                FREE_START_OFFSET,
                (PAGE_HEADER_SIZE + usize::from(slot_count) * SLOT_ENTRY_SIZE) as u16,
            )?;
        }

        self.write_header_u16(FREE_END_OFFSET, record_offset as u16)?;
        self.write_header_u16(
            LIVE_COUNT_OFFSET,
            self.live_count()
                .checked_add(1)
                .ok_or(SlottedPageError::Corrupt("live slot count overflow"))?,
        )?;

        Ok(Rid::new(self.page_id(), slot_id, generation))
    }

    pub fn get(&self, rid: Rid) -> Result<&[u8], SlottedPageError> {
        self.validate_rid(rid)?;
        let offset = usize::from(self.slot_u16(rid.slot_id(), SLOT_RECORD_OFFSET));
        let length = usize::from(self.slot_u16(rid.slot_id(), SLOT_RECORD_LENGTH));
        Ok(self.page.read(offset, length)?)
    }

    pub fn delete(&mut self, rid: Rid) -> Result<(), SlottedPageError> {
        self.validate_rid(rid)?;
        let slot = rid.slot_id();
        let generation = rid.generation();
        let (next_generation, flags) = match generation.checked_add(1) {
            Some(next) => (next, 0),
            None => (generation, SLOT_RETIRED),
        };

        self.write_slot_u16(slot, SLOT_RECORD_OFFSET, 0)?;
        self.write_slot_u16(slot, SLOT_RECORD_LENGTH, 0)?;
        self.write_slot_u16(slot, SLOT_GENERATION, next_generation)?;
        self.write_slot_u16(slot, SLOT_FLAGS, flags)?;
        self.write_header_u16(
            LIVE_COUNT_OFFSET,
            self.live_count()
                .checked_sub(1)
                .ok_or(SlottedPageError::Corrupt("live slot count underflow"))?,
        )?;
        Ok(())
    }

    /// Rewrites live records contiguously at the end of the page. Slot IDs and
    /// generations remain unchanged, so all live RIDs stay valid.
    pub fn compact(&mut self) -> Result<(), SlottedPageError> {
        let free_start = self.free_start();
        let mut packed = Box::new([0_u8; PAGE_SIZE]);
        let mut new_offsets = Vec::with_capacity(usize::from(self.live_count()));
        let mut target_end = PAGE_SIZE;

        for raw_slot in 0..self.slot_count() {
            let slot = SlotId::new(raw_slot);
            if self.slot_u16(slot, SLOT_FLAGS) != SLOT_LIVE {
                continue;
            }

            let old_offset = usize::from(self.slot_u16(slot, SLOT_RECORD_OFFSET));
            let length = usize::from(self.slot_u16(slot, SLOT_RECORD_LENGTH));
            target_end = target_end
                .checked_sub(length)
                .ok_or(SlottedPageError::Corrupt("live records exceed page size"))?;
            let record = self.page.read(old_offset, length)?;
            packed[target_end..target_end + length].copy_from_slice(record);
            new_offsets.push((slot, target_end as u16));
        }

        if target_end < free_start {
            return Err(SlottedPageError::Corrupt(
                "slot directory overlaps packed records",
            ));
        }

        self.page.write(free_start, &packed[free_start..])?;
        for (slot, offset) in new_offsets {
            self.write_slot_u16(slot, SLOT_RECORD_OFFSET, offset)?;
        }
        self.write_header_u16(FREE_END_OFFSET, target_end as u16)?;
        Ok(())
    }

    fn validate(&self) -> Result<(), SlottedPageError> {
        if self.page.read(MAGIC_OFFSET, MAGIC.len())? != MAGIC {
            return Err(SlottedPageError::Corrupt("invalid heap-page magic"));
        }
        if self.page.read(VERSION_OFFSET, 1)?[0] != FORMAT_VERSION {
            return Err(SlottedPageError::Corrupt("unsupported heap-page version"));
        }
        if self.page.read(PAGE_KIND_OFFSET, 1)?[0] != HEAP_PAGE_KIND {
            return Err(SlottedPageError::Corrupt("page is not a heap page"));
        }

        let slot_count = usize::from(self.slot_count());
        let expected_free_start = PAGE_HEADER_SIZE
            .checked_add(
                slot_count
                    .checked_mul(SLOT_ENTRY_SIZE)
                    .ok_or(SlottedPageError::Corrupt("slot directory overflow"))?,
            )
            .ok_or(SlottedPageError::Corrupt("slot directory overflow"))?;
        if self.free_start() != expected_free_start || expected_free_start > PAGE_SIZE {
            return Err(SlottedPageError::Corrupt("invalid slot-directory boundary"));
        }
        if self.free_end() < self.free_start() || self.free_end() > PAGE_SIZE {
            return Err(SlottedPageError::Corrupt("invalid free-space boundary"));
        }

        let mut observed_live = 0_u16;
        let mut ranges = Vec::with_capacity(usize::from(self.live_count()));
        for raw_slot in 0..self.slot_count() {
            let slot = SlotId::new(raw_slot);
            let flags = self.slot_u16(slot, SLOT_FLAGS);
            let offset = usize::from(self.slot_u16(slot, SLOT_RECORD_OFFSET));
            let length = usize::from(self.slot_u16(slot, SLOT_RECORD_LENGTH));
            let generation = self.slot_u16(slot, SLOT_GENERATION);

            match flags {
                SLOT_LIVE => {
                    if generation == 0 {
                        return Err(SlottedPageError::Corrupt("live slot has generation zero"));
                    }
                    let end = offset
                        .checked_add(length)
                        .filter(|end| *end <= PAGE_SIZE)
                        .ok_or(SlottedPageError::Corrupt("record exceeds page boundary"))?;
                    if offset < self.free_end() {
                        return Err(SlottedPageError::Corrupt("record begins inside free space"));
                    }
                    if length != 0 {
                        ranges.push(offset..end);
                    }
                    observed_live = observed_live
                        .checked_add(1)
                        .ok_or(SlottedPageError::Corrupt("live slot count overflow"))?;
                }
                0 | SLOT_RETIRED => {
                    if offset != 0 || length != 0 {
                        return Err(SlottedPageError::Corrupt(
                            "non-live slot references record data",
                        ));
                    }
                }
                _ => return Err(SlottedPageError::Corrupt("invalid slot flags")),
            }
        }

        if observed_live != self.live_count() {
            return Err(SlottedPageError::Corrupt("incorrect live slot count"));
        }

        ranges.sort_unstable_by_key(|range| range.start);
        if ranges.windows(2).any(|pair| pair[0].end > pair[1].start) {
            return Err(SlottedPageError::Corrupt("live records overlap"));
        }
        Ok(())
    }

    fn validate_rid(&self, rid: Rid) -> Result<(), SlottedPageError> {
        if rid.page_id() != self.page_id() || rid.slot_id().get() >= self.slot_count() {
            return Err(SlottedPageError::InvalidRid(rid));
        }
        let slot = rid.slot_id();
        if self.slot_u16(slot, SLOT_FLAGS) != SLOT_LIVE
            || self.slot_u16(slot, SLOT_GENERATION) != rid.generation()
        {
            return Err(SlottedPageError::InvalidRid(rid));
        }
        Ok(())
    }

    fn find_reusable_slot(&self) -> Option<SlotId> {
        (0..self.slot_count())
            .map(SlotId::new)
            .find(|slot| self.slot_u16(*slot, SLOT_FLAGS) == 0)
    }

    fn free_space_after_compaction(
        &self,
        directory_growth: usize,
    ) -> Result<usize, SlottedPageError> {
        let live_bytes = (0..self.slot_count())
            .map(SlotId::new)
            .filter(|slot| self.slot_u16(*slot, SLOT_FLAGS) == SLOT_LIVE)
            .try_fold(0_usize, |total, slot| {
                total
                    .checked_add(usize::from(self.slot_u16(slot, SLOT_RECORD_LENGTH)))
                    .ok_or(SlottedPageError::Corrupt("live record size overflow"))
            })?;
        PAGE_SIZE
            .checked_sub(self.free_start())
            .and_then(|space| space.checked_sub(directory_growth))
            .and_then(|space| space.checked_sub(live_bytes))
            .ok_or(SlottedPageError::Corrupt(
                "page metadata and live records exceed page size",
            ))
    }

    fn free_start(&self) -> usize {
        usize::from(self.header_u16(FREE_START_OFFSET))
    }

    fn free_end(&self) -> usize {
        usize::from(self.header_u16(FREE_END_OFFSET))
    }

    fn header_u16(&self, offset: usize) -> u16 {
        self.page
            .read_u16(offset)
            .expect("validated heap-page header is readable")
    }

    fn write_header_u16(&mut self, offset: usize, value: u16) -> Result<(), SlottedPageError> {
        self.page.write_u16(offset, value)?;
        Ok(())
    }

    fn slot_offset(&self, slot: SlotId) -> usize {
        PAGE_HEADER_SIZE + usize::from(slot.get()) * SLOT_ENTRY_SIZE
    }

    fn slot_u16(&self, slot: SlotId, field_offset: usize) -> u16 {
        self.page
            .read_u16(self.slot_offset(slot) + field_offset)
            .expect("validated slot entry is readable")
    }

    fn write_slot_u16(
        &mut self,
        slot: SlotId,
        field_offset: usize,
        value: u16,
    ) -> Result<(), SlottedPageError> {
        self.page
            .write_u16(self.slot_offset(slot) + field_offset, value)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum SlottedPageError {
    Page(PageError),
    Corrupt(&'static str),
    RecordTooLarge { size: usize },
    NoSpace { required: usize, available: usize },
    InvalidRid(Rid),
}

impl fmt::Display for SlottedPageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Page(error) => error.fmt(formatter),
            Self::Corrupt(reason) => write!(formatter, "corrupt heap page: {reason}"),
            Self::RecordTooLarge { size } => {
                write!(
                    formatter,
                    "record of {size} bytes is too large for a heap page"
                )
            }
            Self::NoSpace {
                required,
                available,
            } => write!(
                formatter,
                "heap page has insufficient space: {required} bytes required, {available} available"
            ),
            Self::InvalidRid(rid) => write!(formatter, "RID does not identify a live row: {rid:?}"),
        }
    }
}

impl Error for SlottedPageError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Page(error) => Some(error),
            _ => None,
        }
    }
}

impl From<PageError> for SlottedPageError {
    fn from(error: PageError) -> Self {
        Self::Page(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_page_has_expected_layout() {
        let page = SlottedPage::new(PageId::new(7));
        assert_eq!(page.page_id(), PageId::new(7));
        assert_eq!(page.slot_count(), 0);
        assert_eq!(page.live_count(), 0);
        assert_eq!(page.contiguous_free_space(), PAGE_SIZE - PAGE_HEADER_SIZE);
        assert_eq!(page.next_page_id(), None);
    }

    #[test]
    fn inserts_and_reads_variable_length_records() {
        let mut page = SlottedPage::new(PageId::new(3));
        let first = page.insert(b"north").unwrap();
        let second = page.insert(b"database engine").unwrap();

        assert_eq!(page.get(first).unwrap(), b"north");
        assert_eq!(page.get(second).unwrap(), b"database engine");
        assert_eq!(page.slot_count(), 2);
        assert_eq!(page.live_count(), 2);
    }

    #[test]
    fn deletion_invalidates_old_rid_and_reuses_slot_safely() {
        let mut page = SlottedPage::new(PageId::new(9));
        let old = page.insert(b"old row").unwrap();
        page.delete(old).unwrap();
        assert!(matches!(
            page.get(old),
            Err(SlottedPageError::InvalidRid(_))
        ));

        let replacement = page.insert(b"new row").unwrap();
        assert_eq!(replacement.slot_id(), old.slot_id());
        assert_eq!(replacement.generation(), old.generation() + 1);
        assert_eq!(page.get(replacement).unwrap(), b"new row");
        assert!(matches!(
            page.get(old),
            Err(SlottedPageError::InvalidRid(_))
        ));
    }

    #[test]
    fn insertion_compacts_deleted_record_space() {
        let mut page = SlottedPage::new(PageId::new(11));
        let first = page.insert(&vec![1; 3_000]).unwrap();
        let second = page.insert(&vec![2; 3_000]).unwrap();
        page.delete(first).unwrap();

        let third = page.insert(&vec![3; 2_500]).unwrap();
        assert_eq!(page.get(second).unwrap(), &vec![2; 3_000]);
        assert_eq!(page.get(third).unwrap(), &vec![3; 2_500]);
        assert_eq!(page.live_count(), 2);
    }

    #[test]
    fn compaction_preserves_live_rids() {
        let mut page = SlottedPage::new(PageId::new(12));
        let first = page.insert(b"first").unwrap();
        let second = page.insert(b"second").unwrap();
        let third = page.insert(b"third").unwrap();
        page.delete(second).unwrap();

        page.compact().unwrap();
        assert_eq!(page.get(first).unwrap(), b"first");
        assert_eq!(page.get(third).unwrap(), b"third");
    }

    #[test]
    fn reports_no_space_without_modifying_live_rows() {
        let mut page = SlottedPage::new(PageId::new(13));
        let existing = page.insert(&vec![4; 7_000]).unwrap();
        let result = page.insert(&vec![5; 2_000]);
        assert!(matches!(result, Err(SlottedPageError::NoSpace { .. })));
        assert_eq!(page.get(existing).unwrap(), &vec![4; 7_000]);
        assert_eq!(page.live_count(), 1);
    }

    #[test]
    fn survives_conversion_to_and_from_raw_page() {
        let mut original = SlottedPage::new(PageId::new(14));
        let rid = original.insert(b"persistent bytes").unwrap();
        let reopened = SlottedPage::from_page(original.into_page()).unwrap();
        assert_eq!(reopened.get(rid).unwrap(), b"persistent bytes");
    }

    #[test]
    fn rejects_corrupt_page_magic() {
        let mut raw = Page::zeroed(PageId::new(15));
        raw.write(MAGIC_OFFSET, b"NOPE").unwrap();
        assert!(matches!(
            SlottedPage::from_page(raw),
            Err(SlottedPageError::Corrupt("invalid heap-page magic"))
        ));
    }

    #[test]
    fn stores_heap_page_link() {
        let mut page = SlottedPage::new(PageId::new(16));
        page.set_next_page_id(Some(PageId::new(17)));
        assert_eq!(page.next_page_id(), Some(PageId::new(17)));
        page.set_next_page_id(None);
        assert_eq!(page.next_page_id(), None);
    }

    #[test]
    fn generation_exhaustion_retires_slot() {
        let mut page = SlottedPage::new(PageId::new(18));
        let rid = page.insert(b"last generation").unwrap();
        page.write_slot_u16(rid.slot_id(), SLOT_GENERATION, u16::MAX)
            .unwrap();
        let exhausted = Rid::new(page.page_id(), rid.slot_id(), u16::MAX);

        page.delete(exhausted).unwrap();
        let next = page.insert(b"different slot").unwrap();
        assert_ne!(next.slot_id(), exhausted.slot_id());
    }
}
