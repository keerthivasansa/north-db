mod heap_file;
mod slotted_page;

pub use heap_file::{HeapFile, HeapFileError, HeapRecord};
pub use slotted_page::{SlottedPage, SlottedPageError, SLOT_ENTRY_SIZE};
