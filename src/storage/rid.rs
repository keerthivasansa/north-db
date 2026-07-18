use super::PageId;

pub const RID_SIZE: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct SlotId(u16);

impl SlotId {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Rid {
    page_id: PageId,
    slot_id: SlotId,
    generation: u16,
}

impl Rid {
    pub const fn new(page_id: PageId, slot_id: SlotId, generation: u16) -> Self {
        Self {
            page_id,
            slot_id,
            generation,
        }
    }

    pub const fn page_id(self) -> PageId {
        self.page_id
    }

    pub const fn slot_id(self) -> SlotId {
        self.slot_id
    }

    pub const fn generation(self) -> u16 {
        self.generation
    }

    /// Encodes the RID as PageId, SlotId, then generation, all little-endian.
    pub const fn encode(self) -> [u8; RID_SIZE] {
        let page = self.page_id.to_le_bytes();
        let slot = self.slot_id.0.to_le_bytes();
        let generation = self.generation.to_le_bytes();
        [
            page[0],
            page[1],
            page[2],
            page[3],
            slot[0],
            slot[1],
            generation[0],
            generation[1],
        ]
    }

    pub const fn decode(bytes: [u8; RID_SIZE]) -> Self {
        Self {
            page_id: PageId::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            slot_id: SlotId(u16::from_le_bytes([bytes[4], bytes[5]])),
            generation: u16::from_le_bytes([bytes[6], bytes[7]]),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rid_round_trips_through_stable_encoding() {
        let rid = Rid::new(PageId::new(0x1234_5678), SlotId::new(0x9abc), 0xdef0);
        let encoded = rid.encode();
        assert_eq!(encoded, [0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xf0, 0xde]);
        assert_eq!(Rid::decode(encoded), rid);
    }
}
