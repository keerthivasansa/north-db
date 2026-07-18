use std::error::Error;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::{Page, PageId, PAGE_SIZE};

const DATABASE_MAGIC: &[u8; 8] = b"NORTHDB\0";
const DATABASE_FORMAT_VERSION: u16 = 1;
const DATABASE_HEADER_PAGE_ID: PageId = PageId::new(0);
const FIRST_DATA_PAGE_ID: u32 = 1;
const NO_CATALOG_ROOT: u32 = u32::MAX;

const MAGIC_OFFSET: usize = 0;
const FORMAT_VERSION_OFFSET: usize = 8;
const PAGE_SIZE_OFFSET: usize = 12;
const NEXT_PAGE_ID_OFFSET: usize = 16;
const CATALOG_ROOT_PAGE_ID_OFFSET: usize = 20;
const HEADER_FLAGS_OFFSET: usize = 24;
const HEADER_CHECKSUM_OFFSET: usize = 28;

/// The validated metadata stored in database page zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatabaseHeader {
    next_page_id: u32,
    catalog_root_page_id: Option<PageId>,
}

impl DatabaseHeader {
    pub const fn next_page_id(self) -> PageId {
        PageId::new(self.next_page_id)
    }

    pub const fn allocated_page_count(self) -> u32 {
        self.next_page_id - FIRST_DATA_PAGE_ID
    }

    pub const fn catalog_root_page_id(self) -> Option<PageId> {
        self.catalog_root_page_id
    }
}

/// Owns the database file and provides exact 8 KiB page I/O.
///
/// Page zero stores `DatabaseHeader`; all allocatable pages begin at page one.
pub struct DiskManager {
    path: PathBuf,
    file: File,
    header: DatabaseHeader,
    read_only: bool,
}

impl DiskManager {
    /// Creates a new database file. Existing files are never overwritten.
    pub fn create(path: impl AsRef<Path>) -> Result<Self, DiskManagerError> {
        let path = path.as_ref().to_path_buf();
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|source| DiskManagerError::Io {
                operation: "create database file",
                path: path.clone(),
                source,
            })?;

        let header = DatabaseHeader {
            next_page_id: FIRST_DATA_PAGE_ID,
            catalog_root_page_id: None,
        };
        write_header_to_file(&mut file, &path, header)?;
        file.sync_all().map_err(|source| DiskManagerError::Io {
            operation: "synchronize new database file",
            path: path.clone(),
            source,
        })?;

        Ok(Self {
            path,
            file,
            header,
            read_only: false,
        })
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, DiskManagerError> {
        Self::open_with_mode(path, false)
    }

    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, DiskManagerError> {
        Self::open_with_mode(path, true)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn header(&self) -> DatabaseHeader {
        self.header
    }

    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Sets the catalog root exactly once and persists it in page zero.
    pub fn set_catalog_root_page_id(&mut self, page_id: PageId) -> Result<(), DiskManagerError> {
        self.ensure_writable()?;
        self.ensure_allocated_data_page(page_id)?;
        if let Some(existing) = self.header.catalog_root_page_id {
            return Err(DiskManagerError::CatalogRootAlreadySet(existing));
        }
        self.header.catalog_root_page_id = Some(page_id);
        self.persist_header()
    }

    /// Appends one zeroed page, persists the new allocation high-water mark,
    /// and returns the newly allocated page.
    pub fn allocate_page(&mut self) -> Result<Page, DiskManagerError> {
        self.ensure_writable()?;

        if self.header.next_page_id == u32::MAX {
            return Err(DiskManagerError::PageIdExhausted);
        }

        let page_id = PageId::new(self.header.next_page_id);
        let page = Page::zeroed(page_id);
        self.write_page_bytes(page_id, page.as_bytes())?;

        self.header.next_page_id += 1;
        self.persist_header()?;
        Ok(page)
    }

    pub fn read_page(&mut self, page_id: PageId) -> Result<Page, DiskManagerError> {
        self.ensure_allocated_data_page(page_id)?;
        self.seek_to_page(page_id)?;

        let mut bytes = Box::new([0; PAGE_SIZE]);
        self.file
            .read_exact(bytes.as_mut())
            .map_err(|source| DiskManagerError::Io {
                operation: "read database page",
                path: self.path.clone(),
                source,
            })?;
        Ok(Page::from_bytes(page_id, bytes))
    }

    pub fn write_page(&mut self, page: &mut Page) -> Result<(), DiskManagerError> {
        self.ensure_writable()?;
        self.ensure_allocated_data_page(page.id())?;
        self.write_page_bytes(page.id(), page.as_bytes())?;
        page.mark_clean();
        Ok(())
    }

    /// Forces completed file writes to stable storage.
    pub fn sync(&self) -> Result<(), DiskManagerError> {
        self.file.sync_all().map_err(|source| DiskManagerError::Io {
            operation: "synchronize database file",
            path: self.path.clone(),
            source,
        })
    }

    fn open_with_mode(path: impl AsRef<Path>, read_only: bool) -> Result<Self, DiskManagerError> {
        let path = path.as_ref().to_path_buf();
        let mut options = OpenOptions::new();
        options.read(true);
        if !read_only {
            options.write(true);
        }
        let mut file = options.open(&path).map_err(|source| DiskManagerError::Io {
            operation: "open database file",
            path: path.clone(),
            source,
        })?;

        let file_length = file
            .metadata()
            .map_err(|source| DiskManagerError::Io {
                operation: "read database file metadata",
                path: path.clone(),
                source,
            })?
            .len();
        validate_file_length(file_length)?;

        let header = read_header_from_file(&mut file, &path)?;
        let physical_page_count = file_length / PAGE_SIZE as u64;
        if physical_page_count < u64::from(header.next_page_id) {
            return Err(DiskManagerError::Corrupt(
                "database header references pages beyond the end of the file",
            ));
        }

        Ok(Self {
            path,
            file,
            header,
            read_only,
        })
    }

    fn ensure_writable(&self) -> Result<(), DiskManagerError> {
        if self.read_only {
            return Err(DiskManagerError::ReadOnly);
        }
        Ok(())
    }

    fn ensure_allocated_data_page(&self, page_id: PageId) -> Result<(), DiskManagerError> {
        if page_id == DATABASE_HEADER_PAGE_ID {
            return Err(DiskManagerError::HeaderPageIsReserved);
        }
        if page_id.get() >= self.header.next_page_id {
            return Err(DiskManagerError::PageNotAllocated(page_id));
        }
        Ok(())
    }

    fn persist_header(&mut self) -> Result<(), DiskManagerError> {
        write_header_to_file(&mut self.file, &self.path, self.header)
    }

    fn seek_to_page(&mut self, page_id: PageId) -> Result<(), DiskManagerError> {
        self.file
            .seek(SeekFrom::Start(page_id.file_offset()))
            .map_err(|source| DiskManagerError::Io {
                operation: "seek database page",
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }

    fn write_page_bytes(
        &mut self,
        page_id: PageId,
        bytes: &[u8; PAGE_SIZE],
    ) -> Result<(), DiskManagerError> {
        self.seek_to_page(page_id)?;
        self.file
            .write_all(bytes)
            .map_err(|source| DiskManagerError::Io {
                operation: "write database page",
                path: self.path.clone(),
                source,
            })
    }
}

fn write_header_to_file(
    file: &mut File,
    path: &Path,
    header: DatabaseHeader,
) -> Result<(), DiskManagerError> {
    let bytes = encode_header(header);
    file.seek(SeekFrom::Start(DATABASE_HEADER_PAGE_ID.file_offset()))
        .map_err(|source| DiskManagerError::Io {
            operation: "seek database header",
            path: path.to_path_buf(),
            source,
        })?;
    file.write_all(&bytes)
        .map_err(|source| DiskManagerError::Io {
            operation: "write database header",
            path: path.to_path_buf(),
            source,
        })
}

fn read_header_from_file(file: &mut File, path: &Path) -> Result<DatabaseHeader, DiskManagerError> {
    let mut bytes = [0; PAGE_SIZE];
    file.seek(SeekFrom::Start(DATABASE_HEADER_PAGE_ID.file_offset()))
        .map_err(|source| DiskManagerError::Io {
            operation: "seek database header",
            path: path.to_path_buf(),
            source,
        })?;
    file.read_exact(&mut bytes)
        .map_err(|source| DiskManagerError::Io {
            operation: "read database header",
            path: path.to_path_buf(),
            source,
        })?;
    decode_header(&bytes)
}

fn encode_header(header: DatabaseHeader) -> [u8; PAGE_SIZE] {
    let mut bytes = [0; PAGE_SIZE];
    bytes[MAGIC_OFFSET..MAGIC_OFFSET + DATABASE_MAGIC.len()].copy_from_slice(DATABASE_MAGIC);
    bytes[FORMAT_VERSION_OFFSET..FORMAT_VERSION_OFFSET + 2]
        .copy_from_slice(&DATABASE_FORMAT_VERSION.to_le_bytes());
    bytes[PAGE_SIZE_OFFSET..PAGE_SIZE_OFFSET + 4]
        .copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    bytes[NEXT_PAGE_ID_OFFSET..NEXT_PAGE_ID_OFFSET + 4]
        .copy_from_slice(&header.next_page_id.to_le_bytes());
    bytes[CATALOG_ROOT_PAGE_ID_OFFSET..CATALOG_ROOT_PAGE_ID_OFFSET + 4].copy_from_slice(
        &header
            .catalog_root_page_id
            .map_or(NO_CATALOG_ROOT, PageId::get)
            .to_le_bytes(),
    );
    bytes[HEADER_FLAGS_OFFSET..HEADER_FLAGS_OFFSET + 4].copy_from_slice(&0_u32.to_le_bytes());
    bytes[HEADER_CHECKSUM_OFFSET..HEADER_CHECKSUM_OFFSET + 4].copy_from_slice(&0_u32.to_le_bytes());
    bytes
}

fn decode_header(bytes: &[u8; PAGE_SIZE]) -> Result<DatabaseHeader, DiskManagerError> {
    if &bytes[MAGIC_OFFSET..MAGIC_OFFSET + DATABASE_MAGIC.len()] != DATABASE_MAGIC {
        return Err(DiskManagerError::Corrupt("invalid database magic"));
    }
    if read_u16(bytes, FORMAT_VERSION_OFFSET) != DATABASE_FORMAT_VERSION {
        return Err(DiskManagerError::Corrupt(
            "unsupported database format version",
        ));
    }
    if read_u32(bytes, PAGE_SIZE_OFFSET) != PAGE_SIZE as u32 {
        return Err(DiskManagerError::Corrupt("incompatible database page size"));
    }

    let next_page_id = read_u32(bytes, NEXT_PAGE_ID_OFFSET);
    if next_page_id < FIRST_DATA_PAGE_ID {
        return Err(DiskManagerError::Corrupt("invalid next page ID"));
    }
    let catalog_root = read_u32(bytes, CATALOG_ROOT_PAGE_ID_OFFSET);
    let catalog_root_page_id = (catalog_root != NO_CATALOG_ROOT).then(|| PageId::new(catalog_root));
    Ok(DatabaseHeader {
        next_page_id,
        catalog_root_page_id,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn validate_file_length(length: u64) -> Result<(), DiskManagerError> {
    if length < PAGE_SIZE as u64 || !length.is_multiple_of(PAGE_SIZE as u64) {
        return Err(DiskManagerError::InvalidFileLength { length });
    }
    Ok(())
}

#[derive(Debug)]
pub enum DiskManagerError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    InvalidFileLength {
        length: u64,
    },
    Corrupt(&'static str),
    ReadOnly,
    HeaderPageIsReserved,
    PageNotAllocated(PageId),
    PageIdExhausted,
    CatalogRootAlreadySet(PageId),
}

impl fmt::Display for DiskManagerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "failed to {operation} at {}: {source}", path.display()),
            Self::InvalidFileLength { length } => write!(
                formatter,
                "invalid database file length {length}; it must be a positive multiple of {PAGE_SIZE}"
            ),
            Self::Corrupt(reason) => write!(formatter, "corrupt database header: {reason}"),
            Self::ReadOnly => write!(formatter, "database was opened read-only"),
            Self::HeaderPageIsReserved => write!(formatter, "database header page is reserved"),
            Self::PageNotAllocated(page_id) => write!(
                formatter,
                "page {} has not been allocated",
                page_id.get()
            ),
            Self::PageIdExhausted => write!(formatter, "database has exhausted allocatable page IDs"),
            Self::CatalogRootAlreadySet(page_id) => write!(
                formatter,
                "database catalog root is already page {}",
                page_id.get()
            ),
        }
    }
}

impl Error for DiskManagerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestDatabasePath(PathBuf);

    impl TestDatabasePath {
        fn new() -> Self {
            let sequence = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "north-disk-test-{}-{sequence}.north",
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

    #[test]
    fn creates_a_single_header_page() {
        let path = TestDatabasePath::new();
        let manager = DiskManager::create(&path).unwrap();

        assert_eq!(fs::metadata(path.as_ref()).unwrap().len(), PAGE_SIZE as u64);
        assert_eq!(manager.header().allocated_page_count(), 0);
        assert_eq!(manager.header().next_page_id(), PageId::new(1));
        assert_eq!(manager.header().catalog_root_page_id(), None);
    }

    #[test]
    fn allocates_writes_and_reopens_pages() {
        let path = TestDatabasePath::new();
        let mut manager = DiskManager::create(&path).unwrap();
        let mut first = manager.allocate_page().unwrap();
        let second = manager.allocate_page().unwrap();
        assert_eq!(first.id(), PageId::new(1));
        assert_eq!(second.id(), PageId::new(2));

        first.write(0, b"north persists pages").unwrap();
        manager.write_page(&mut first).unwrap();
        manager.sync().unwrap();
        drop(manager);

        let mut reopened = DiskManager::open(&path).unwrap();
        assert_eq!(reopened.header().allocated_page_count(), 2);
        let page = reopened.read_page(PageId::new(1)).unwrap();
        assert_eq!(page.read(0, 20).unwrap(), b"north persists pages");
    }

    #[test]
    fn rejects_header_page_and_unallocated_pages() {
        let path = TestDatabasePath::new();
        let mut manager = DiskManager::create(&path).unwrap();
        assert!(matches!(
            manager.read_page(PageId::new(0)),
            Err(DiskManagerError::HeaderPageIsReserved)
        ));
        assert!(matches!(
            manager.read_page(PageId::new(1)),
            Err(DiskManagerError::PageNotAllocated(_))
        ));
    }

    #[test]
    fn read_only_manager_rejects_mutations() {
        let path = TestDatabasePath::new();
        let mut writable = DiskManager::create(&path).unwrap();
        let page = writable.allocate_page().unwrap();
        writable.sync().unwrap();
        drop(writable);

        let mut read_only = DiskManager::open_read_only(&path).unwrap();
        assert!(read_only.is_read_only());
        assert!(matches!(
            read_only.allocate_page(),
            Err(DiskManagerError::ReadOnly)
        ));
        let mut page = page;
        assert!(matches!(
            read_only.write_page(&mut page),
            Err(DiskManagerError::ReadOnly)
        ));
    }

    #[test]
    fn rejects_misaligned_and_corrupt_files() {
        let path = TestDatabasePath::new();
        fs::write(path.as_ref(), b"not a page").unwrap();
        assert!(matches!(
            DiskManager::open(&path),
            Err(DiskManagerError::InvalidFileLength { .. })
        ));

        fs::write(path.as_ref(), [0; PAGE_SIZE]).unwrap();
        assert!(matches!(
            DiskManager::open(&path),
            Err(DiskManagerError::Corrupt("invalid database magic"))
        ));
    }

    #[test]
    fn creation_never_overwrites_an_existing_file() {
        let path = TestDatabasePath::new();
        fs::write(path.as_ref(), b"keep me").unwrap();
        assert!(matches!(
            DiskManager::create(&path),
            Err(DiskManagerError::Io { .. })
        ));
        assert_eq!(fs::read(path.as_ref()).unwrap(), b"keep me");
    }

    #[test]
    fn persists_catalog_root_exactly_once() {
        let path = TestDatabasePath::new();
        let mut manager = DiskManager::create(&path).unwrap();
        let root = manager.allocate_page().unwrap().id();
        manager.set_catalog_root_page_id(root).unwrap();
        assert!(matches!(
            manager.set_catalog_root_page_id(root),
            Err(DiskManagerError::CatalogRootAlreadySet(existing)) if existing == root
        ));
        manager.sync().unwrap();
        drop(manager);

        let reopened = DiskManager::open(&path).unwrap();
        assert_eq!(reopened.header().catalog_root_page_id(), Some(root));
    }
}
