//! Builds ZIP archives in memory for tests, including deliberately malformed ones.
//!
//! Shared by the unit tests and the end-to-end tests.

#![allow(dead_code)]

struct Item {
    name: Vec<u8>,
    data: Vec<u8>,
    method: u16,
    flags: u16,
    crc32: u32,
    size: u32,
}

#[derive(Default)]
pub struct ZipBuilder {
    items: Vec<Item>,
    comment: Vec<u8>,
}

impl ZipBuilder {
    pub fn new() -> ZipBuilder {
        ZipBuilder::default()
    }

    fn push(
        mut self,
        name: &str,
        data: Vec<u8>,
        method: u16,
        flags: u16,
        crc32: u32,
        size: u32,
    ) -> Self {
        self.items.push(Item {
            name: name.as_bytes().to_vec(),
            data,
            method,
            flags,
            crc32,
            size,
        });
        self
    }

    /// A file stored without compression.
    pub fn stored(self, name: &str, content: &[u8]) -> Self {
        let crc = crc32fast::hash(content);
        self.push(name, content.to_vec(), 0, 0, crc, content.len() as u32)
    }

    /// A deflate-compressed file.
    pub fn deflated(self, name: &str, content: &[u8]) -> Self {
        let crc = crc32fast::hash(content);
        let packed = miniz_oxide::deflate::compress_to_vec(content, 6);
        self.push(name, packed, 8, 0, crc, content.len() as u32)
    }

    /// A deflate-compressed file whose headers lie about the uncompressed size.
    pub fn deflated_claiming(self, name: &str, content: &[u8], claimed_size: u32) -> Self {
        let crc = crc32fast::hash(content);
        let packed = miniz_oxide::deflate::compress_to_vec(content, 6);
        self.push(name, packed, 8, 0, crc, claimed_size)
    }

    /// A folder entry.
    pub fn dir(self, name: &str) -> Self {
        self.push(name, Vec::new(), 0, 0, 0, 0)
    }

    /// An entry with an arbitrary compression method and flags.
    pub fn raw(self, name: &str, data: &[u8], method: u16, flags: u16) -> Self {
        let crc = crc32fast::hash(data);
        self.push(name, data.to_vec(), method, flags, crc, data.len() as u32)
    }

    pub fn comment(mut self, comment: &[u8]) -> Self {
        self.comment = comment.to_vec();
        self
    }

    pub fn build(self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for item in &self.items {
            let offset = out.len() as u32;
            // Local file header.
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&item.flags.to_le_bytes());
            out.extend_from_slice(&item.method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0x5b6cu16.to_le_bytes());
            out.extend_from_slice(&item.crc32.to_le_bytes());
            out.extend_from_slice(&(item.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&item.size.to_le_bytes());
            out.extend_from_slice(&(item.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&item.name);
            out.extend_from_slice(&item.data);

            // Central directory header.
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&0x031eu16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&item.flags.to_le_bytes());
            central.extend_from_slice(&item.method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0x5b6cu16.to_le_bytes());
            central.extend_from_slice(&item.crc32.to_le_bytes());
            central.extend_from_slice(&(item.data.len() as u32).to_le_bytes());
            central.extend_from_slice(&item.size.to_le_bytes());
            central.extend_from_slice(&(item.name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(&item.name);
        }
        let directory_offset = out.len() as u32;
        out.extend_from_slice(&central);
        // End of central directory.
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(self.items.len() as u16).to_le_bytes());
        out.extend_from_slice(&(self.items.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&directory_offset.to_le_bytes());
        out.extend_from_slice(&(self.comment.len() as u16).to_le_bytes());
        out.extend_from_slice(&self.comment);
        out
    }
}
