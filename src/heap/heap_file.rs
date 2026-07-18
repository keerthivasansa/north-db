use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::storage::{BufferPool, BufferPoolError, PageId, Rid, PAGE_HEADER_SIZE, PAGE_SIZE};

use super::{SlottedPage, SlottedPageError, SLOT_ENTRY_SIZE};

const MAX_RECORD_SIZE: usize = PAGE_SIZE - PAGE_HEADER_SIZE - SLOT_ENTRY_SIZE;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeapRecord {
    pub rid: Rid,
    pub bytes: Vec<u8>,
}

/// A linked sequence of slotted pages belonging to one table or system heap.
pub struct HeapFile {
    pages: Vec<PageId>,
}

impl HeapFile {
    pub fn create(pool: &BufferPool) -> Result<Self, HeapFileError> {
        let guard = pool.allocate_page()?;
        let page_id = guard.page_id();
        {
            let mut page = guard.write()?;
            SlottedPage::initialize(&mut page);
        }
        Ok(Self {
            pages: vec![page_id],
        })
    }

    pub fn open(pool: &BufferPool, first_page_id: PageId) -> Result<Self, HeapFileError> {
        let mut pages = Vec::new();
        let mut visited = HashSet::new();
        let mut current = first_page_id;

        loop {
            if !visited.insert(current) {
                return Err(HeapFileError::PageChainCycle(current));
            }
            pages.push(current);
            let next = {
                let guard = pool.pin(current)?;
                let page = guard.read()?;
                SlottedPage::from_page_ref(&page)?.next_page_id()
            };
            match next {
                Some(page_id) => current = page_id,
                None => break,
            }
        }
        Ok(Self { pages })
    }

    pub fn first_page_id(&self) -> PageId {
        self.pages[0]
    }

    pub fn last_page_id(&self) -> PageId {
        *self.pages.last().expect("heap file always has one page")
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    pub fn insert(&mut self, pool: &BufferPool, record: &[u8]) -> Result<Rid, HeapFileError> {
        if record.len() > MAX_RECORD_SIZE {
            return Err(HeapFileError::RecordTooLarge {
                size: record.len(),
                maximum: MAX_RECORD_SIZE,
            });
        }

        for page_id in self.pages.iter().copied() {
            let result = {
                let guard = pool.pin(page_id)?;
                let mut page = guard.write()?;
                let mut slotted = SlottedPage::from_page_mut(&mut page)?;
                slotted.insert(record)
            };
            match result {
                Ok(rid) => return Ok(rid),
                Err(SlottedPageError::NoSpace { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }

        self.append_page_with_record(pool, record)
    }

    pub fn get(&self, pool: &BufferPool, rid: Rid) -> Result<Vec<u8>, HeapFileError> {
        self.ensure_owned_rid(rid)?;
        let guard = pool.pin(rid.page_id())?;
        let page = guard.read()?;
        let slotted = SlottedPage::from_page_ref(&page)?;
        Ok(slotted.get(rid)?.to_vec())
    }

    pub fn delete(&mut self, pool: &BufferPool, rid: Rid) -> Result<(), HeapFileError> {
        self.ensure_owned_rid(rid)?;
        let guard = pool.pin(rid.page_id())?;
        let mut page = guard.write()?;
        SlottedPage::from_page_mut(&mut page)?.delete(rid)?;
        Ok(())
    }

    pub fn scan(&self, pool: &BufferPool) -> Result<Vec<HeapRecord>, HeapFileError> {
        let mut records = Vec::new();
        for page_id in &self.pages {
            let guard = pool.pin(*page_id)?;
            let page = guard.read()?;
            let slotted = SlottedPage::from_page_ref(&page)?;
            for rid in slotted.live_rids() {
                records.push(HeapRecord {
                    rid,
                    bytes: slotted.get(rid)?.to_vec(),
                });
            }
        }
        Ok(records)
    }

    fn append_page_with_record(
        &mut self,
        pool: &BufferPool,
        record: &[u8],
    ) -> Result<Rid, HeapFileError> {
        let new_guard = pool.allocate_page()?;
        let new_page_id = new_guard.page_id();
        let rid = {
            let mut page = new_guard.write()?;
            let mut slotted = SlottedPage::initialize(&mut page);
            slotted.insert(record)?
        };
        drop(new_guard);

        let previous_page_id = self.last_page_id();
        let previous_guard = pool.pin(previous_page_id)?;
        {
            let mut page = previous_guard.write()?;
            SlottedPage::from_page_mut(&mut page)?.set_next_page_id(Some(new_page_id))?;
        }
        self.pages.push(new_page_id);
        Ok(rid)
    }

    fn ensure_owned_rid(&self, rid: Rid) -> Result<(), HeapFileError> {
        if !self.pages.contains(&rid.page_id()) {
            return Err(HeapFileError::RidOutsideHeap(rid));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum HeapFileError {
    Buffer(Box<BufferPoolError>),
    Page(Box<SlottedPageError>),
    RecordTooLarge { size: usize, maximum: usize },
    PageChainCycle(PageId),
    RidOutsideHeap(Rid),
}

impl fmt::Display for HeapFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffer(error) => error.fmt(formatter),
            Self::Page(error) => error.fmt(formatter),
            Self::RecordTooLarge { size, maximum } => {
                write!(formatter, "record is {size} bytes; maximum is {maximum}")
            }
            Self::PageChainCycle(page_id) => write!(
                formatter,
                "heap page chain contains a cycle at page {}",
                page_id.get()
            ),
            Self::RidOutsideHeap(rid) => {
                write!(formatter, "RID belongs to a different heap file: {rid:?}")
            }
        }
    }
}

impl Error for HeapFileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Buffer(error) => Some(error.as_ref()),
            Self::Page(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<BufferPoolError> for HeapFileError {
    fn from(error: BufferPoolError) -> Self {
        Self::Buffer(Box::new(error))
    }
}

impl From<SlottedPageError> for HeapFileError {
    fn from(error: SlottedPageError) -> Self {
        Self::Page(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::storage::DiskManager;

    use super::*;

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestDatabasePath(PathBuf);

    impl TestDatabasePath {
        fn new() -> Self {
            let sequence = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "north-heap-file-test-{}-{sequence}.north",
                std::process::id()
            ));
            assert!(!path.exists());
            Self(path)
        }
    }

    impl AsRef<Path> for TestDatabasePath {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDatabasePath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    fn pool(path: &Path, capacity: usize) -> BufferPool {
        BufferPool::new(DiskManager::create(path).unwrap(), capacity).unwrap()
    }

    #[test]
    fn creates_inserts_reads_and_deletes() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref(), 2);
        let mut heap = HeapFile::create(&pool).unwrap();
        let rid = heap.insert(&pool, b"north row").unwrap();

        assert_eq!(heap.get(&pool, rid).unwrap(), b"north row");
        heap.delete(&pool, rid).unwrap();
        assert!(matches!(
            heap.get(&pool, rid),
            Err(HeapFileError::Page(error))
                if matches!(*error, SlottedPageError::InvalidRid(_))
        ));
    }

    #[test]
    fn grows_and_reopens_multi_page_chain() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref(), 2);
        let mut heap = HeapFile::create(&pool).unwrap();
        let first_page = heap.first_page_id();
        let first = heap.insert(&pool, &vec![1; 4_000]).unwrap();
        let second = heap.insert(&pool, &vec![2; 4_000]).unwrap();
        let third = heap.insert(&pool, &vec![3; 4_000]).unwrap();

        assert_eq!(first.page_id(), second.page_id());
        assert_ne!(third.page_id(), first.page_id());
        assert_eq!(heap.page_count(), 2);

        pool.sync().unwrap();
        drop(heap);
        let disk = pool.close().unwrap();
        drop(disk);
        let read_only = BufferPool::new(DiskManager::open_read_only(&path).unwrap(), 1).unwrap();
        let reopened = HeapFile::open(&read_only, first_page).unwrap();
        let records = reopened.scan(&read_only).unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].bytes, vec![1; 4_000]);
        assert_eq!(records[2].bytes, vec![3; 4_000]);
    }

    #[test]
    fn reuses_space_in_an_earlier_page() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref(), 2);
        let mut heap = HeapFile::create(&pool).unwrap();
        let first = heap.insert(&pool, &vec![1; 3_000]).unwrap();
        heap.insert(&pool, &vec![2; 3_000]).unwrap();
        heap.insert(&pool, &vec![3; 3_000]).unwrap();
        assert_eq!(heap.page_count(), 2);

        heap.delete(&pool, first).unwrap();
        let replacement = heap.insert(&pool, &vec![4; 2_500]).unwrap();
        assert_eq!(replacement.page_id(), first.page_id());
        assert_eq!(heap.page_count(), 2);
    }

    #[test]
    fn rejects_rids_from_another_heap() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref(), 3);
        let mut first = HeapFile::create(&pool).unwrap();
        let second = HeapFile::create(&pool).unwrap();
        let rid = first.insert(&pool, b"owned by first").unwrap();
        assert!(matches!(
            second.get(&pool, rid),
            Err(HeapFileError::RidOutsideHeap(_))
        ));
    }

    #[test]
    fn detects_page_chain_cycles() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref(), 2);
        let heap = HeapFile::create(&pool).unwrap();
        let first_page = heap.first_page_id();
        let guard = pool.pin(first_page).unwrap();
        {
            let mut page = guard.write().unwrap();
            SlottedPage::from_page_mut(&mut page)
                .unwrap()
                .set_next_page_id(Some(first_page))
                .unwrap();
        }
        drop(guard);

        assert!(matches!(
            HeapFile::open(&pool, first_page),
            Err(HeapFileError::PageChainCycle(page_id)) if page_id == first_page
        ));
    }
}
