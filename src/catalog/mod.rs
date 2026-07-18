use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::fmt;

use crate::heap::{HeapFile, HeapFileError};
use crate::schema::{SchemaError, TableSchema, MAX_ROW_SIZE};
use crate::storage::{BufferPool, BufferPoolError, PageId};

const TABLE_RECORD_MAGIC: &[u8; 4] = b"NTBL";
const TABLE_RECORD_VERSION: u16 = 1;
const TABLE_RECORD_HEADER_SIZE: usize = 20;
const FIRST_TABLE_ID: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct TableId(u32);

impl TableId {
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableMetadata {
    id: TableId,
    schema: TableSchema,
    heap_first_page_id: PageId,
}

impl TableMetadata {
    pub const fn id(&self) -> TableId {
        self.id
    }

    pub const fn heap_first_page_id(&self) -> PageId {
        self.heap_first_page_id
    }

    pub const fn schema(&self) -> &TableSchema {
        &self.schema
    }
}

/// Persistent registry of immutable table definitions.
pub struct Catalog {
    heap: HeapFile,
    tables: BTreeMap<String, TableMetadata>,
    next_table_id: u32,
}

impl Catalog {
    pub fn create(pool: &BufferPool) -> Result<Self, CatalogError> {
        if let Some(root) = pool.database_header().catalog_root_page_id() {
            return Err(CatalogError::AlreadyInitialized(root));
        }

        let heap = HeapFile::create(pool)?;
        let root = heap.first_page_id();
        pool.flush_page(root)?;
        pool.set_catalog_root_page_id(root)?;
        Ok(Self {
            heap,
            tables: BTreeMap::new(),
            next_table_id: FIRST_TABLE_ID,
        })
    }

    pub fn open(pool: &BufferPool) -> Result<Self, CatalogError> {
        let root = pool
            .database_header()
            .catalog_root_page_id()
            .ok_or(CatalogError::NotInitialized)?;
        let heap = HeapFile::open(pool, root)?;
        let records = heap.scan(pool)?;
        let mut tables = BTreeMap::new();
        let mut table_ids = HashSet::new();
        let mut highest_table_id = 0_u32;

        for record in records {
            let metadata = decode_table_record(&record.bytes)?;
            if !table_ids.insert(metadata.id) {
                return Err(CatalogError::DuplicateTableId(metadata.id));
            }
            if tables.contains_key(metadata.schema.name()) {
                return Err(CatalogError::DuplicateTableName(
                    metadata.schema.name().to_owned(),
                ));
            }
            HeapFile::open(pool, metadata.heap_first_page_id)?;
            highest_table_id = highest_table_id.max(metadata.id.get());
            tables.insert(metadata.schema.name().to_owned(), metadata);
        }

        Ok(Self {
            heap,
            tables,
            next_table_id: highest_table_id.saturating_add(1).max(FIRST_TABLE_ID),
        })
    }

    pub fn root_page_id(&self) -> PageId {
        self.heap.first_page_id()
    }

    pub fn table_count(&self) -> usize {
        self.tables.len()
    }

    pub fn table(&self, name: &str) -> Option<&TableMetadata> {
        self.tables.get(name)
    }

    pub fn tables(&self) -> impl Iterator<Item = &TableMetadata> {
        self.tables.values()
    }

    pub fn create_table(
        &mut self,
        pool: &BufferPool,
        schema: TableSchema,
    ) -> Result<&TableMetadata, CatalogError> {
        if self.tables.contains_key(schema.name()) {
            return Err(CatalogError::DuplicateTableName(schema.name().to_owned()));
        }
        if self.next_table_id == u32::MAX {
            return Err(CatalogError::TableIdExhausted);
        }
        let record_size = TABLE_RECORD_HEADER_SIZE + schema.encode().len();
        if record_size > MAX_ROW_SIZE {
            return Err(CatalogError::TableRecordTooLarge {
                size: record_size,
                maximum: MAX_ROW_SIZE,
            });
        }

        let table_id = TableId::new(self.next_table_id);
        let table_heap = HeapFile::create(pool)?;
        let metadata = TableMetadata {
            id: table_id,
            heap_first_page_id: table_heap.first_page_id(),
            schema,
        };
        let encoded = encode_table_record(&metadata);
        self.heap.insert(pool, &encoded)?;

        self.next_table_id += 1;
        let name = metadata.schema.name().to_owned();
        self.tables.insert(name.clone(), metadata);
        Ok(self
            .tables
            .get(&name)
            .expect("newly inserted table metadata must exist"))
    }

    pub fn open_table_heap(&self, pool: &BufferPool, name: &str) -> Result<HeapFile, CatalogError> {
        let metadata = self
            .table(name)
            .ok_or_else(|| CatalogError::TableNotFound(name.to_owned()))?;
        Ok(HeapFile::open(pool, metadata.heap_first_page_id)?)
    }
}

fn encode_table_record(metadata: &TableMetadata) -> Vec<u8> {
    let schema = metadata.schema.encode();
    let mut bytes = Vec::with_capacity(TABLE_RECORD_HEADER_SIZE + schema.len());
    bytes.extend_from_slice(TABLE_RECORD_MAGIC);
    bytes.extend_from_slice(&TABLE_RECORD_VERSION.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes.extend_from_slice(&metadata.id.get().to_le_bytes());
    bytes.extend_from_slice(&metadata.heap_first_page_id.get().to_le_bytes());
    bytes.extend_from_slice(&(schema.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&schema);
    bytes
}

fn decode_table_record(bytes: &[u8]) -> Result<TableMetadata, CatalogError> {
    if bytes.len() < TABLE_RECORD_HEADER_SIZE {
        return Err(CatalogError::CorruptRecord("table record is truncated"));
    }
    if &bytes[0..4] != TABLE_RECORD_MAGIC {
        return Err(CatalogError::CorruptRecord("invalid table record magic"));
    }
    if read_u16(bytes, 4) != TABLE_RECORD_VERSION {
        return Err(CatalogError::CorruptRecord(
            "unsupported table record version",
        ));
    }
    if read_u16(bytes, 6) != 0 {
        return Err(CatalogError::CorruptRecord(
            "table record reserved flags are not zero",
        ));
    }

    let table_id = read_u32(bytes, 8);
    if table_id < FIRST_TABLE_ID {
        return Err(CatalogError::CorruptRecord("invalid table ID"));
    }
    let heap_first_page_id = read_u32(bytes, 12);
    if heap_first_page_id == 0 {
        return Err(CatalogError::CorruptRecord("invalid table heap root"));
    }
    let schema_length = read_u32(bytes, 16) as usize;
    let expected_length = TABLE_RECORD_HEADER_SIZE
        .checked_add(schema_length)
        .ok_or(CatalogError::CorruptRecord("table record length overflow"))?;
    if expected_length != bytes.len() {
        return Err(CatalogError::CorruptRecord(
            "table record schema length does not match record",
        ));
    }
    let schema = TableSchema::decode(&bytes[TABLE_RECORD_HEADER_SIZE..])?;
    Ok(TableMetadata {
        id: TableId::new(table_id),
        schema,
        heap_first_page_id: PageId::new(heap_first_page_id),
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

#[derive(Debug)]
pub enum CatalogError {
    Buffer(Box<BufferPoolError>),
    Heap(Box<HeapFileError>),
    Schema(Box<SchemaError>),
    AlreadyInitialized(PageId),
    NotInitialized,
    DuplicateTableName(String),
    DuplicateTableId(TableId),
    TableNotFound(String),
    TableIdExhausted,
    TableRecordTooLarge { size: usize, maximum: usize },
    CorruptRecord(&'static str),
}

impl fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffer(error) => error.fmt(formatter),
            Self::Heap(error) => error.fmt(formatter),
            Self::Schema(error) => error.fmt(formatter),
            Self::AlreadyInitialized(page_id) => write!(
                formatter,
                "database catalog is already initialized at page {}",
                page_id.get()
            ),
            Self::NotInitialized => write!(formatter, "database catalog is not initialized"),
            Self::DuplicateTableName(name) => write!(formatter, "table already exists: {name}"),
            Self::DuplicateTableId(table_id) => {
                write!(formatter, "duplicate table ID: {}", table_id.get())
            }
            Self::TableNotFound(name) => write!(formatter, "table does not exist: {name}"),
            Self::TableIdExhausted => write!(formatter, "catalog has exhausted table IDs"),
            Self::TableRecordTooLarge { size, maximum } => write!(
                formatter,
                "encoded table record is {size} bytes; maximum is {maximum}"
            ),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt catalog record: {reason}"),
        }
    }
}

impl Error for CatalogError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Buffer(error) => Some(error.as_ref()),
            Self::Heap(error) => Some(error.as_ref()),
            Self::Schema(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<BufferPoolError> for CatalogError {
    fn from(error: BufferPoolError) -> Self {
        Self::Buffer(Box::new(error))
    }
}

impl From<HeapFileError> for CatalogError {
    fn from(error: HeapFileError) -> Self {
        Self::Heap(Box::new(error))
    }
}

impl From<SchemaError> for CatalogError {
    fn from(error: SchemaError) -> Self {
        Self::Schema(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::schema::{ColumnSchema, DataType, Value, ValueRef};
    use crate::storage::DiskManager;

    use super::*;

    static NEXT_TEST_FILE: AtomicU64 = AtomicU64::new(0);

    struct TestDatabasePath(PathBuf);

    impl TestDatabasePath {
        fn new() -> Self {
            let sequence = NEXT_TEST_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "north-catalog-test-{}-{sequence}.north",
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

    fn schema(name: &str) -> TableSchema {
        TableSchema::new(
            name,
            vec![
                ColumnSchema::primary_key("id", DataType::Int),
                ColumnSchema::required("name", DataType::Text),
            ],
        )
        .unwrap()
    }

    fn pool(path: &Path) -> BufferPool {
        BufferPool::new(DiskManager::create(path).unwrap(), 3).unwrap()
    }

    #[test]
    fn creates_catalog_root_and_rejects_second_initialization() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref());
        let catalog = Catalog::create(&pool).unwrap();
        assert_eq!(
            pool.database_header().catalog_root_page_id(),
            Some(catalog.root_page_id())
        );
        assert!(matches!(
            Catalog::create(&pool),
            Err(CatalogError::AlreadyInitialized(_))
        ));
    }

    #[test]
    fn persists_immutable_table_metadata() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref());
        let mut catalog = Catalog::create(&pool).unwrap();
        let users = catalog.create_table(&pool, schema("users")).unwrap();
        assert_eq!(users.id(), TableId::new(1));
        assert_eq!(users.schema().primary_key().name(), "id");
        assert_eq!(catalog.table_count(), 1);
        assert!(matches!(
            catalog.create_table(&pool, schema("users")),
            Err(CatalogError::DuplicateTableName(name)) if name == "users"
        ));

        pool.sync().unwrap();
        drop(catalog);
        let disk = pool.close().unwrap();
        drop(disk);
        let read_only = BufferPool::new(DiskManager::open_read_only(&path).unwrap(), 2).unwrap();
        let reopened = Catalog::open(&read_only).unwrap();
        assert_eq!(reopened.table_count(), 1);
        assert_eq!(reopened.table("users").unwrap().schema(), &schema("users"));
    }

    #[test]
    fn table_rows_survive_catalog_and_heap_reopen() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref());
        let mut catalog = Catalog::create(&pool).unwrap();
        catalog.create_table(&pool, schema("users")).unwrap();
        let users_schema = catalog.table("users").unwrap().schema().clone();
        let mut heap = catalog.open_table_heap(&pool, "users").unwrap();
        let encoded = users_schema
            .encode_row(&[Value::Int(7), Value::Text("Ada".into())])
            .unwrap();
        heap.insert(&pool, &encoded).unwrap();
        pool.sync().unwrap();
        drop(heap);
        drop(catalog);
        let disk = pool.close().unwrap();
        drop(disk);

        let read_only = BufferPool::new(DiskManager::open_read_only(&path).unwrap(), 2).unwrap();
        let catalog = Catalog::open(&read_only).unwrap();
        let heap = catalog.open_table_heap(&read_only, "users").unwrap();
        let records = heap.scan(&read_only).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            catalog
                .table("users")
                .unwrap()
                .schema()
                .decode_row(&records[0].bytes)
                .unwrap(),
            vec![ValueRef::Int(7), ValueRef::Text("Ada")]
        );
    }

    #[test]
    fn reports_missing_tables_and_uninitialized_database() {
        let path = TestDatabasePath::new();
        let pool = pool(path.as_ref());
        assert!(matches!(
            Catalog::open(&pool),
            Err(CatalogError::NotInitialized)
        ));
        let catalog = Catalog::create(&pool).unwrap();
        assert!(matches!(
            catalog.open_table_heap(&pool, "missing"),
            Err(CatalogError::TableNotFound(name)) if name == "missing"
        ));
    }
}
