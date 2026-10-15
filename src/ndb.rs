//! NDB layer of the PST/OST format ([MS-PST] section 2.2): file header, the
//! node and block B-trees, data blocks (with OST 2013 zlib compression), data
//! trees and subnode trees.
//!
//! The file is memory-mapped. On Windows this matters: Outlook holds byte-range
//! locks on the header of an open OST, which makes `ReadFile` fail, but locks
//! are not enforced on mapped views.

use std::borrow::Cow;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use memmap2::Mmap;

use crate::error::{Error, Result, corrupt};

pub type Nid = u32;
pub type Bid = u64;

const PTYPE_BBT: u8 = 0x80;
const PTYPE_NBT: u8 = 0x81;

/// On-disk layout parameters that differ between the Unicode format
/// (PST/OST 2003-2010, wVer 23) and the 4K-page format (OST 2013+, wVer 36).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Unicode,
    Unicode4k,
}

impl Format {
    fn page_size(self) -> usize {
        match self {
            Format::Unicode => 512,
            Format::Unicode4k => 4096,
        }
    }
    /// Offset of cEnt/cEntMax/cbEnt/cLevel inside a B-tree page.
    fn page_meta(self) -> usize {
        match self {
            Format::Unicode => 488,
            Format::Unicode4k => 4056,
        }
    }
    fn page_trailer(self) -> usize {
        match self {
            Format::Unicode => 496,
            Format::Unicode4k => 4072,
        }
    }
    fn block_align(self) -> u64 {
        match self {
            Format::Unicode => 64,
            Format::Unicode4k => 512,
        }
    }
    fn block_trailer(self) -> u64 {
        match self {
            Format::Unicode => 16,
            Format::Unicode4k => 24,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Format::Unicode => "unicode",
            Format::Unicode4k => "unicode-4k",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct NbtEntry {
    pub nid: Nid,
    pub data: Bid,
    pub sub: Bid,
    pub parent: Nid,
}

#[derive(Clone, Copy, Debug)]
pub struct SubEntry {
    pub nid: Nid,
    pub data: Bid,
    pub sub: Bid,
}

pub struct Ndb {
    map: Mmap,
    pub format: Format,
    pub is_ost: bool,
    nbt_root: u64,
    bbt_root: u64,
    crypt: u8,
    /// Identifies the committed state of the file: changes on every write.
    pub revision: (Bid, Bid, u64),
}

#[inline]
pub(crate) fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
#[inline]
pub(crate) fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline]
pub(crate) fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

struct Page<'a> {
    data: &'a [u8],
    count: usize,
    entry_size: usize,
    level: u8,
}

impl<'a> Page<'a> {
    fn entry(&self, i: usize) -> &'a [u8] {
        &self.data[i * self.entry_size..(i + 1) * self.entry_size]
    }
}

impl Ndb {
    pub fn open(path: &Path) -> Result<Ndb> {
        let file = File::open(path).map_err(|e| Error::Io(format!("{}: {e}", path.display())))?;
        #[cfg(not(windows))]
        {
            // Through WSL's drvfs, Outlook's byte-range lock on the header turns
            // reads of a mapped page into SIGBUS; probe with a plain read first.
            let mut probe = [0u8; 564];
            if let Err(e) = (&file).read_exact(&mut probe) {
                return Err(Error::Io(format!(
                    "{}: {e} (if Outlook has this file open, run the Windows build outlook-cli.exe, \
                     which can also be called from WSL)",
                    path.display()
                )));
            }
        }
        // SAFETY: the file is opened read-only. Outlook may modify it while we
        // read; every structure read is bounds-checked and validated, and the
        // caller retries on inconsistency.
        let map = unsafe { Mmap::map(&file) }.map_err(|e| Error::Io(format!("mmap {}: {e}", path.display())))?;
        Self::from_map(map)
    }

    fn from_map(map: Mmap) -> Result<Ndb> {
        let h = &map[..map.len().min(564)];
        if h.len() < 564 {
            return Err(Error::Format("file too small to be a PST/OST".into()));
        }
        if &h[0..4] != b"!BDN" {
            return Err(Error::Format("not a PST/OST file (bad magic)".into()));
        }
        let client = u16_at(h, 8);
        let ver = u16_at(h, 10);
        let format = match ver {
            14 | 15 => return Err(Error::Format("ANSI (Outlook 97-2002) PST files are not supported".into())),
            23 => Format::Unicode,
            36 => Format::Unicode4k,
            v => return Err(Error::Format(format!("unsupported file version {v}"))),
        };
        // HEADER.root starts at 0xB4; BREFs are (bid, ib).
        let root = 0xB4;
        let nbt_bid = u64_at(h, root + 36);
        let nbt_root = u64_at(h, root + 44);
        let bbt_bid = u64_at(h, root + 52);
        let bbt_root = u64_at(h, root + 60);
        let eof = u64_at(h, root + 4);
        let crypt = h[513];
        if crypt > 2 {
            return Err(Error::Format(format!("unknown encoding (bCryptMethod={crypt})")));
        }
        Ok(Ndb { map, format, is_ost: client == 0x4F53, nbt_root, bbt_root, crypt, revision: (nbt_bid, bbt_bid, eof) })
    }

    pub fn file_len(&self) -> u64 {
        self.map.len() as u64
    }

    fn slice(&self, off: u64, len: usize) -> Result<&[u8]> {
        let off = usize::try_from(off).map_err(|_| corrupt("offset overflow"))?;
        self.map
            .get(off..off.checked_add(len).ok_or_else(|| corrupt("offset overflow"))?)
            .ok_or_else(|| corrupt(format!("read past end of file at {off:#x}")))
    }

    fn page(&self, ib: u64, ptype: u8) -> Result<Page<'_>> {
        let f = self.format;
        let p = self.slice(ib, f.page_size())?;
        let t = f.page_trailer();
        if p[t] != ptype || p[t + 1] != ptype {
            return Err(corrupt(format!("bad page type at {ib:#x}")));
        }
        let m = f.page_meta();
        let (count, entry_size, level) = match f {
            Format::Unicode => (p[m] as usize, p[m + 2] as usize, p[m + 3]),
            Format::Unicode4k => (u16_at(p, m) as usize, p[m + 4] as usize, p[m + 5]),
        };
        // Intermediate and BBT entries are 24 bytes, NBT leaf entries 32.
        let min = if ptype == PTYPE_NBT && level == 0 { 32 } else { 24 };
        if entry_size < min || count * entry_size > m {
            return Err(corrupt(format!("bad page header at {ib:#x}")));
        }
        Ok(Page { data: p, count, entry_size, level })
    }

    /// Find the leaf entry with the given key in a B-tree (NBT or BBT).
    fn bt_find(&self, root: u64, ptype: u8, key: u64) -> Result<Option<&[u8]>> {
        let mut page = self.page(root, ptype)?;
        let mut depth = 0;
        while page.level > 0 {
            // Intermediate entries: (btkey u64, BREF{bid, ib}); find the last key <= target.
            let mut lo = 0usize;
            let mut hi = page.count;
            while lo < hi {
                let mid = (lo + hi) / 2;
                if u64_at(page.entry(mid), 0) <= key {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            if lo == 0 {
                return Ok(None);
            }
            let ib = u64_at(page.entry(lo - 1), 16);
            page = self.page(ib, ptype)?;
            depth += 1;
            if depth > 16 {
                return Err(corrupt("B-tree too deep"));
            }
        }
        let mask = if ptype == PTYPE_BBT { !1u64 } else { !0u64 };
        let (mut lo, mut hi) = (0usize, page.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let k = u64_at(page.entry(mid), 0) & mask;
            if k == key {
                return Ok(Some(page.entry(mid)));
            } else if k < key {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(None)
    }

    pub fn node(&self, nid: Nid) -> Result<Option<NbtEntry>> {
        Ok(self.bt_find(self.nbt_root, PTYPE_NBT, nid as u64)?.map(parse_nbt_entry))
    }

    /// Visit every NBT leaf entry in key order.
    pub fn for_each_node(&self, f: &mut dyn FnMut(NbtEntry)) -> Result<()> {
        self.walk(self.nbt_root, 0, f)
    }

    fn walk(&self, ib: u64, depth: u32, f: &mut dyn FnMut(NbtEntry)) -> Result<()> {
        if depth > 16 {
            return Err(corrupt("B-tree too deep"));
        }
        let page = self.page(ib, PTYPE_NBT)?;
        for i in 0..page.count {
            let e = page.entry(i);
            if page.level > 0 {
                self.walk(u64_at(e, 16), depth + 1, f)?;
            } else {
                f(parse_nbt_entry(e));
            }
        }
        Ok(())
    }

    /// Size of a block's (decompressed) content, from the BBT alone.
    pub fn block_len(&self, bid: Bid) -> Result<usize> {
        let e = self
            .bt_find(self.bbt_root, PTYPE_BBT, bid & !1)?
            .ok_or_else(|| corrupt(format!("block {bid:#x} not found")))?;
        Ok(match self.format {
            Format::Unicode => u16_at(e, 16),
            Format::Unicode4k => u16_at(e, 18).max(u16_at(e, 16)),
        } as usize)
    }

    /// Read one block, validating its trailer and decompressing it if needed.
    pub fn block(&self, bid: Bid) -> Result<Cow<'_, [u8]>> {
        let key = bid & !1;
        let e =
            self.bt_find(self.bbt_root, PTYPE_BBT, key)?.ok_or_else(|| corrupt(format!("block {bid:#x} not found")))?;
        let ib = u64_at(e, 8);
        let cb = u16_at(e, 16) as usize;
        let inflated = match self.format {
            Format::Unicode => cb,
            Format::Unicode4k => u16_at(e, 18) as usize,
        };
        let f = self.format;
        let tsize = f.block_trailer();
        let total = (cb as u64 + tsize).div_ceil(f.block_align()) * f.block_align();
        let raw = self.slice(ib, total as usize)?;
        let tr = &raw[(total - tsize) as usize..];
        if u16_at(tr, 0) as usize != cb || u64_at(tr, 8) & !1 != key {
            return Err(corrupt(format!("block {bid:#x} trailer mismatch")));
        }
        let data = &raw[..cb];
        // Only data blocks are encoded; internal blocks (XBLOCK, SLBLOCK...) are not.
        if self.crypt != 0 && bid & 2 == 0 {
            let mut d = data.to_vec();
            if self.crypt == 1 {
                crate::crypt::decode_permute(&mut d);
            } else {
                crate::crypt::decode_cyclic(&mut d, u64_at(e, 0));
            }
            return Ok(Cow::Owned(d));
        }
        if inflated > cb {
            let mut out = Vec::with_capacity(inflated);
            flate2::read::ZlibDecoder::new(data)
                .read_to_end(&mut out)
                .map_err(|e| corrupt(format!("block {bid:#x}: zlib: {e}")))?;
            if out.len() != inflated {
                return Err(corrupt(format!("block {bid:#x}: bad inflated size")));
            }
            Ok(Cow::Owned(out))
        } else {
            Ok(Cow::Borrowed(data))
        }
    }

    /// The leaf block ids of a data tree, in order.
    pub fn data_tree(&self, bid: Bid) -> Result<Vec<Bid>> {
        let mut out = Vec::new();
        if bid != 0 {
            self.collect_leaves(bid, 0, &mut out)?;
        }
        Ok(out)
    }

    fn collect_leaves(&self, bid: Bid, depth: u32, out: &mut Vec<Bid>) -> Result<()> {
        if bid & 2 == 0 {
            out.push(bid);
            return Ok(());
        }
        if depth > 2 {
            return Err(corrupt("data tree too deep"));
        }
        let b = self.block(bid)?;
        if b.len() < 8 || b[0] != 1 {
            return Err(corrupt(format!("bad XBLOCK {bid:#x}")));
        }
        let n = u16_at(&b, 2) as usize;
        if 8 + n * 8 > b.len() {
            return Err(corrupt(format!("bad XBLOCK {bid:#x}")));
        }
        for i in 0..n {
            let child = u64_at(&b, 8 + i * 8);
            if b[1] == 1 {
                out.push(child);
            } else {
                self.collect_leaves(child, depth + 1, out)?;
            }
        }
        Ok(())
    }

    /// Whole content of a data tree (concatenated blocks).
    pub fn read_all(&self, bid: Bid) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for b in self.data_tree(bid)? {
            out.extend_from_slice(&self.block(b)?);
        }
        Ok(out)
    }

    /// Flatten a subnode tree (SLBLOCK/SIBLOCK) into its entries.
    pub fn subnodes(&self, bid: Bid) -> Result<Vec<SubEntry>> {
        let mut out = Vec::new();
        if bid != 0 {
            self.collect_subnodes(bid, 0, &mut out)?;
        }
        Ok(out)
    }

    fn collect_subnodes(&self, bid: Bid, depth: u32, out: &mut Vec<SubEntry>) -> Result<()> {
        if depth > 4 {
            return Err(corrupt("subnode tree too deep"));
        }
        let b = self.block(bid)?;
        if b.len() < 8 || b[0] != 2 {
            return Err(corrupt(format!("bad subnode block {bid:#x}")));
        }
        let n = u16_at(&b, 2) as usize;
        let leaf = b[1] == 0;
        let es = if leaf { 24 } else { 16 };
        if 8 + n * es > b.len() {
            return Err(corrupt(format!("bad subnode block {bid:#x}")));
        }
        for i in 0..n {
            let e = &b[8 + i * es..8 + (i + 1) * es];
            if leaf {
                out.push(SubEntry { nid: u64_at(e, 0) as Nid, data: u64_at(e, 8), sub: u64_at(e, 16) });
            } else {
                self.collect_subnodes(u64_at(e, 8), depth + 1, out)?;
            }
        }
        Ok(())
    }
}

fn parse_nbt_entry(e: &[u8]) -> NbtEntry {
    NbtEntry { nid: u64_at(e, 0) as Nid, data: u64_at(e, 8), sub: u64_at(e, 16), parent: u32_at(e, 24) }
}
