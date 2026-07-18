use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use super::{DatabaseHeader, DiskManager, DiskManagerError, Page, PageId};

/// Observable counters for buffer-pool behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BufferPoolStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub disk_writes: u64,
}

/// A bounded, single-threaded page cache backed by `DiskManager`.
///
/// Page guards keep frames pinned. When capacity is reached, the least recently
/// used unpinned frame is evicted, with dirty data written first.
pub struct BufferPool {
    inner: RefCell<BufferPoolInner>,
}

struct BufferPoolInner {
    disk: DiskManager,
    capacity: usize,
    frames: HashMap<PageId, Rc<Frame>>,
    clock: u64,
    stats: BufferPoolStats,
}

struct Frame {
    page_id: PageId,
    page: RefCell<Page>,
    pin_count: Cell<u32>,
    last_used: Cell<u64>,
    writable: bool,
}

/// Pins one cached page until the guard is dropped.
///
/// Multiple read borrows may coexist. A mutable borrow is exclusive and marks
/// the page dirty when its contents are changed through the `Page` API.
pub struct PageGuard<'pool> {
    frame: Rc<Frame>,
    _pool: PhantomData<&'pool BufferPool>,
}

impl BufferPool {
    pub fn new(disk: DiskManager, capacity: usize) -> Result<Self, BufferPoolError> {
        if capacity == 0 {
            return Err(BufferPoolError::ZeroCapacity);
        }
        Ok(Self {
            inner: RefCell::new(BufferPoolInner {
                disk,
                capacity,
                frames: HashMap::with_capacity(capacity),
                clock: 0,
                stats: BufferPoolStats::default(),
            }),
        })
    }

    pub fn pin(&self, page_id: PageId) -> Result<PageGuard<'_>, BufferPoolError> {
        let mut inner = self.borrow_inner_mut()?;
        let tick = inner.next_tick();

        let frame = if let Some(frame) = inner.frames.get(&page_id).cloned() {
            inner.stats.hits = inner.stats.hits.saturating_add(1);
            frame
        } else {
            inner.stats.misses = inner.stats.misses.saturating_add(1);
            inner.ensure_space()?;
            let page = inner.disk.read_page(page_id)?;
            let frame = Rc::new(Frame {
                page_id,
                page: RefCell::new(page),
                pin_count: Cell::new(0),
                last_used: Cell::new(tick),
                writable: !inner.disk.is_read_only(),
            });
            inner.frames.insert(page_id, Rc::clone(&frame));
            frame
        };

        let next_pin_count = frame
            .pin_count
            .get()
            .checked_add(1)
            .ok_or(BufferPoolError::PinCountExhausted(page_id))?;
        frame.pin_count.set(next_pin_count);
        frame.last_used.set(tick);
        drop(inner);

        Ok(PageGuard {
            frame,
            _pool: PhantomData,
        })
    }

    /// Allocates a new disk page and returns it already pinned in the cache.
    pub fn allocate_page(&self) -> Result<PageGuard<'_>, BufferPoolError> {
        let mut inner = self.borrow_inner_mut()?;
        if inner.disk.is_read_only() {
            return Err(BufferPoolError::ReadOnly);
        }

        inner.ensure_space()?;
        let page = inner.disk.allocate_page()?;
        let page_id = page.id();
        let tick = inner.next_tick();
        let frame = Rc::new(Frame {
            page_id,
            page: RefCell::new(page),
            pin_count: Cell::new(1),
            last_used: Cell::new(tick),
            writable: true,
        });
        inner.frames.insert(page_id, Rc::clone(&frame));
        drop(inner);

        Ok(PageGuard {
            frame,
            _pool: PhantomData,
        })
    }

    pub fn flush_page(&self, page_id: PageId) -> Result<(), BufferPoolError> {
        let mut inner = self.borrow_inner_mut()?;
        let Some(frame) = inner.frames.get(&page_id).cloned() else {
            return Err(BufferPoolError::PageNotCached(page_id));
        };
        inner.flush_frame(&frame)
    }

    /// Writes every dirty cached page but does not force the operating system's
    /// file cache to durable storage.
    pub fn flush_all(&self) -> Result<(), BufferPoolError> {
        self.borrow_inner_mut()?.flush_all()
    }

    /// Flushes dirty frames and requests durable file synchronization.
    pub fn sync(&self) -> Result<(), BufferPoolError> {
        let mut inner = self.borrow_inner_mut()?;
        inner.flush_all()?;
        inner.disk.sync()?;
        Ok(())
    }

    /// Flushes, synchronizes, and returns the underlying disk manager.
    pub fn close(self) -> Result<DiskManager, BufferPoolError> {
        let mut inner = self.inner.into_inner();
        inner.flush_all()?;
        inner.disk.sync()?;
        Ok(inner.disk)
    }

    pub fn contains_page(&self, page_id: PageId) -> bool {
        self.inner.borrow().frames.contains_key(&page_id)
    }

    pub fn cached_page_count(&self) -> usize {
        self.inner.borrow().frames.len()
    }

    pub fn capacity(&self) -> usize {
        self.inner.borrow().capacity
    }

    pub fn stats(&self) -> BufferPoolStats {
        self.inner.borrow().stats
    }

    pub fn database_header(&self) -> DatabaseHeader {
        self.inner.borrow().disk.header()
    }

    pub fn set_catalog_root_page_id(&self, page_id: PageId) -> Result<(), BufferPoolError> {
        self.borrow_inner_mut()?
            .disk
            .set_catalog_root_page_id(page_id)?;
        Ok(())
    }

    fn borrow_inner_mut(&self) -> Result<RefMut<'_, BufferPoolInner>, BufferPoolError> {
        self.inner
            .try_borrow_mut()
            .map_err(|_| BufferPoolError::PoolBorrowed)
    }
}

impl BufferPoolInner {
    fn ensure_space(&mut self) -> Result<(), BufferPoolError> {
        if self.frames.len() < self.capacity {
            return Ok(());
        }

        let victim_id = self
            .frames
            .iter()
            .filter(|(_, frame)| frame.pin_count.get() == 0)
            .min_by_key(|(_, frame)| frame.last_used.get())
            .map(|(page_id, _)| *page_id)
            .ok_or(BufferPoolError::AllFramesPinned)?;
        let victim = self
            .frames
            .get(&victim_id)
            .cloned()
            .expect("selected victim must remain cached");

        self.flush_frame(&victim)?;
        self.frames.remove(&victim_id);
        self.stats.evictions = self.stats.evictions.saturating_add(1);
        Ok(())
    }

    fn flush_all(&mut self) -> Result<(), BufferPoolError> {
        let frames: Vec<_> = self.frames.values().cloned().collect();
        for frame in frames {
            self.flush_frame(&frame)?;
        }
        Ok(())
    }

    fn flush_frame(&mut self, frame: &Rc<Frame>) -> Result<(), BufferPoolError> {
        let page_id = frame.page_id;
        let mut page = frame
            .page
            .try_borrow_mut()
            .map_err(|_| BufferPoolError::PageBorrowed(page_id))?;
        if page.is_dirty() {
            self.disk.write_page(&mut page)?;
            self.stats.disk_writes = self.stats.disk_writes.saturating_add(1);
        }
        Ok(())
    }

    fn next_tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        if self.clock == 0 {
            for frame in self.frames.values() {
                frame.last_used.set(0);
            }
            self.clock = 1;
        }
        self.clock
    }
}

impl PageGuard<'_> {
    pub fn page_id(&self) -> PageId {
        self.frame.page_id
    }

    pub fn read(&self) -> Result<Ref<'_, Page>, BufferPoolError> {
        self.frame
            .page
            .try_borrow()
            .map_err(|_| BufferPoolError::PageBorrowed(self.page_id()))
    }

    pub fn write(&self) -> Result<RefMut<'_, Page>, BufferPoolError> {
        if !self.frame.writable {
            return Err(BufferPoolError::ReadOnly);
        }
        self.frame
            .page
            .try_borrow_mut()
            .map_err(|_| BufferPoolError::PageBorrowed(self.page_id()))
    }
}

impl Drop for PageGuard<'_> {
    fn drop(&mut self) {
        let pin_count = self.frame.pin_count.get();
        debug_assert!(pin_count > 0, "page guard pin count underflow");
        self.frame.pin_count.set(pin_count.saturating_sub(1));
    }
}

#[derive(Debug)]
pub enum BufferPoolError {
    Disk(Box<DiskManagerError>),
    ZeroCapacity,
    AllFramesPinned,
    PoolBorrowed,
    PageBorrowed(PageId),
    PageNotCached(PageId),
    PinCountExhausted(PageId),
    ReadOnly,
}

impl fmt::Display for BufferPoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disk(error) => error.fmt(formatter),
            Self::ZeroCapacity => {
                write!(formatter, "buffer pool capacity must be greater than zero")
            }
            Self::AllFramesPinned => write!(formatter, "all buffer-pool frames are pinned"),
            Self::PoolBorrowed => write!(formatter, "buffer pool is already mutably borrowed"),
            Self::PageBorrowed(page_id) => {
                write!(
                    formatter,
                    "cached page {} is already borrowed",
                    page_id.get()
                )
            }
            Self::PageNotCached(page_id) => {
                write!(formatter, "page {} is not cached", page_id.get())
            }
            Self::PinCountExhausted(page_id) => {
                write!(formatter, "page {} pin count is exhausted", page_id.get())
            }
            Self::ReadOnly => write!(formatter, "database was opened read-only"),
        }
    }
}

impl Error for BufferPoolError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Disk(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<DiskManagerError> for BufferPoolError {
    fn from(error: DiskManagerError) -> Self {
        Self::Disk(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestDatabasePath(PathBuf);

    impl TestDatabasePath {
        fn new() -> Self {
            let sequence = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "north-buffer-test-{}-{sequence}.north",
                std::process::id()
            ));
            assert!(!path.exists(), "test database path already exists");
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

    fn database_with_pages(path: &Path, count: usize) -> DiskManager {
        let mut disk = DiskManager::create(path).unwrap();
        for _ in 0..count {
            disk.allocate_page().unwrap();
        }
        disk
    }

    #[test]
    fn rejects_zero_capacity() {
        let path = TestDatabasePath::new();
        let disk = DiskManager::create(&path).unwrap();
        assert!(matches!(
            BufferPool::new(disk, 0),
            Err(BufferPoolError::ZeroCapacity)
        ));
    }

    #[test]
    fn cache_hit_avoids_second_disk_read() {
        let path = TestDatabasePath::new();
        let disk = database_with_pages(path.as_ref(), 1);
        let pool = BufferPool::new(disk, 2).unwrap();

        drop(pool.pin(PageId::new(1)).unwrap());
        drop(pool.pin(PageId::new(1)).unwrap());
        assert_eq!(
            pool.stats(),
            BufferPoolStats {
                hits: 1,
                misses: 1,
                evictions: 0,
                disk_writes: 0,
            }
        );
    }

    #[test]
    fn pinned_frame_cannot_be_evicted() {
        let path = TestDatabasePath::new();
        let disk = database_with_pages(path.as_ref(), 2);
        let pool = BufferPool::new(disk, 1).unwrap();
        let _first = pool.pin(PageId::new(1)).unwrap();

        assert!(matches!(
            pool.pin(PageId::new(2)),
            Err(BufferPoolError::AllFramesPinned)
        ));
    }

    #[test]
    fn evicts_least_recently_used_unpinned_frame() {
        let path = TestDatabasePath::new();
        let disk = database_with_pages(path.as_ref(), 3);
        let pool = BufferPool::new(disk, 2).unwrap();

        drop(pool.pin(PageId::new(1)).unwrap());
        drop(pool.pin(PageId::new(2)).unwrap());
        drop(pool.pin(PageId::new(1)).unwrap());
        drop(pool.pin(PageId::new(3)).unwrap());

        assert!(pool.contains_page(PageId::new(1)));
        assert!(!pool.contains_page(PageId::new(2)));
        assert!(pool.contains_page(PageId::new(3)));
        assert_eq!(pool.stats().evictions, 1);
    }

    #[test]
    fn dirty_eviction_writes_page_before_reuse() {
        let path = TestDatabasePath::new();
        let disk = database_with_pages(path.as_ref(), 2);
        let pool = BufferPool::new(disk, 1).unwrap();

        let first = pool.pin(PageId::new(1)).unwrap();
        first.write().unwrap().write(0, b"dirty data").unwrap();
        drop(first);
        drop(pool.pin(PageId::new(2)).unwrap());

        let first_again = pool.pin(PageId::new(1)).unwrap();
        assert_eq!(
            first_again.read().unwrap().read(0, 10).unwrap(),
            b"dirty data"
        );
        assert_eq!(pool.stats().disk_writes, 1);
    }

    #[test]
    fn allocate_returns_cached_pinned_page() {
        let path = TestDatabasePath::new();
        let disk = DiskManager::create(&path).unwrap();
        let pool = BufferPool::new(disk, 1).unwrap();

        let page = pool.allocate_page().unwrap();
        assert_eq!(page.page_id(), PageId::new(1));
        assert!(pool.contains_page(PageId::new(1)));
        assert_eq!(pool.cached_page_count(), 1);
    }

    #[test]
    fn sync_persists_dirty_cached_pages() {
        let path = TestDatabasePath::new();
        let disk = database_with_pages(path.as_ref(), 1);
        let pool = BufferPool::new(disk, 2).unwrap();
        let page = pool.pin(PageId::new(1)).unwrap();
        page.write().unwrap().write(32, b"synced").unwrap();
        drop(page);

        pool.sync().unwrap();
        drop(pool);
        let mut reopened = DiskManager::open(&path).unwrap();
        assert_eq!(
            reopened
                .read_page(PageId::new(1))
                .unwrap()
                .read(32, 6)
                .unwrap(),
            b"synced"
        );
    }

    #[test]
    fn read_only_pool_rejects_mutable_guard() {
        let path = TestDatabasePath::new();
        let disk = database_with_pages(path.as_ref(), 1);
        disk.sync().unwrap();
        drop(disk);
        let read_only = DiskManager::open_read_only(&path).unwrap();
        let pool = BufferPool::new(read_only, 1).unwrap();
        let page = pool.pin(PageId::new(1)).unwrap();

        assert!(matches!(page.write(), Err(BufferPoolError::ReadOnly)));
        assert!(matches!(
            pool.allocate_page(),
            Err(BufferPoolError::ReadOnly)
        ));
    }

    #[test]
    fn simultaneous_read_guards_are_allowed() {
        let path = TestDatabasePath::new();
        let disk = database_with_pages(path.as_ref(), 1);
        let pool = BufferPool::new(disk, 1).unwrap();
        let first = pool.pin(PageId::new(1)).unwrap();
        let second = pool.pin(PageId::new(1)).unwrap();

        let first_read = first.read().unwrap();
        let second_read = second.read().unwrap();
        assert_eq!(first_read.id(), second_read.id());
    }
}
