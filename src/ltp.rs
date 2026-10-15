//! LTP layer ([MS-PST] section 2.3): heap-on-node, BTree-on-heap, property
//! contexts (PC) and table contexts (TC).

use std::borrow::Cow;
use std::cell::OnceCell;

use crate::error::{Result, corrupt};
use crate::ndb::{Bid, Format, Ndb, Nid, SubEntry, u16_at, u32_at, u64_at};

pub const PT_SHORT: u16 = 0x0002;
pub const PT_LONG: u16 = 0x0003;
pub const PT_FLOAT: u16 = 0x0004;
pub const PT_DOUBLE: u16 = 0x0005;
pub const PT_CURRENCY: u16 = 0x0006;
pub const PT_APPTIME: u16 = 0x0007;
pub const PT_ERROR: u16 = 0x000A;
pub const PT_BOOLEAN: u16 = 0x000B;
pub const PT_I8: u16 = 0x0014;
pub const PT_STRING8: u16 = 0x001E;
pub const PT_UNICODE: u16 = 0x001F;
pub const PT_SYSTIME: u16 = 0x0040;
pub const PT_CLSID: u16 = 0x0048;
pub const PT_BINARY: u16 = 0x0102;
pub const PT_MV_LONG: u16 = 0x1003;
pub const PT_MV_I8: u16 = 0x1014;
pub const PT_MV_STRING8: u16 = 0x101E;
pub const PT_MV_UNICODE: u16 = 0x101F;
pub const PT_MV_BINARY: u16 = 0x1102;

#[derive(Debug, Clone)]
pub enum Value<'a> {
    Int(i64),
    Bool(bool),
    Float(f64),
    /// FILETIME: 100 ns intervals since 1601-01-01 UTC.
    Time(i64),
    Str(String),
    Bin(Cow<'a, [u8]>),
    MultiStr(Vec<String>),
    MultiInt(Vec<i64>),
    MultiBin(Vec<Vec<u8>>),
    Other(u16),
}

impl<'a> Value<'a> {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn into_string(self) -> Option<String> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }
    pub fn as_time(&self) -> Option<i64> {
        match self {
            Value::Time(t) => Some(*t),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            _ => None,
        }
    }
    pub fn into_bytes(self) -> Option<Cow<'a, [u8]>> {
        match self {
            Value::Bin(b) => Some(b),
            Value::Str(s) => Some(Cow::Owned(s.into_bytes())),
            _ => None,
        }
    }
}

pub fn utf16(b: &[u8]) -> String {
    let mut units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    while units.last() == Some(&0) {
        units.pop();
    }
    String::from_utf16_lossy(&units)
}

/// 8-bit strings are decoded as Windows-1252, the default ANSI code page of
/// Western-European Outlook installations.
pub fn ansi(b: &[u8]) -> String {
    const HI: [char; 32] = [
        '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}', '\u{90}', '‘',
        '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
    ];
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..end].iter().map(|&c| if (0x80..0xA0).contains(&c) { HI[(c - 0x80) as usize] } else { c as char }).collect()
}

/// A BTree-on-heap record: (key, value).
pub type Record<'h> = (&'h [u8], &'h [u8]);

/// Heap-on-node: the allocations of a node's data tree. Blocks are read
/// (and decompressed) on first access: a large folder's contents table has
/// tens of megabytes of heap, of which a typical query touches a few blocks.
pub struct Heap<'a> {
    ndb: &'a Ndb,
    bids: Vec<Bid>,
    blocks: Vec<OnceCell<Cow<'a, [u8]>>>,
    /// Bit position of the block index inside a HID: the 4K format uses
    /// 14-bit allocation indexes (its heap pages are 64 KiB) instead of 11.
    block_shift: u32,
    pub client_sig: u8,
    pub user_root: u32,
}

impl<'a> Heap<'a> {
    pub fn load(ndb: &'a Ndb, data: Bid) -> Result<Heap<'a>> {
        let bids = ndb.data_tree(data)?;
        let blocks = (0..bids.len()).map(|_| OnceCell::new()).collect();
        let block_shift = match ndb.format {
            Format::Unicode => 16,
            Format::Unicode4k => 19,
        };
        let mut heap = Heap { ndb, bids, blocks, block_shift, client_sig: 0, user_root: 0 };
        let first = heap.block(0)?;
        if first.len() < 12 || first[2] != 0xEC {
            return Err(corrupt("bad heap signature"));
        }
        (heap.client_sig, heap.user_root) = (first[3], u32_at(first, 4));
        Ok(heap)
    }

    fn block(&self, i: usize) -> Result<&[u8]> {
        let cell = self.blocks.get(i).ok_or_else(|| corrupt(format!("heap block {i} out of range")))?;
        if let Some(b) = cell.get() {
            return Ok(b);
        }
        let _ = cell.set(self.ndb.block(self.bids[i])?);
        Ok(cell.get().unwrap())
    }

    pub fn get(&self, hid: u32) -> Result<&[u8]> {
        if hid == 0 {
            return Ok(&[]);
        }
        if hid & 0x1F != 0 {
            return Err(corrupt(format!("{hid:#x} is not a HID")));
        }
        let index = ((hid & ((1 << self.block_shift) - 1)) >> 5) as usize;
        let b = self.block((hid >> self.block_shift) as usize)?;
        let map = u16_at(b, 0) as usize;
        if map + 4 > b.len() {
            return Err(corrupt("bad heap page map"));
        }
        let count = u16_at(b, map) as usize;
        if index == 0 || index > count || map + 4 + (count + 1) * 2 > b.len() {
            return Err(corrupt(format!("HID {hid:#x}: bad index")));
        }
        let start = u16_at(b, map + 4 + (index - 1) * 2) as usize;
        let end = u16_at(b, map + 4 + index * 2) as usize;
        b.get(start..end).ok_or_else(|| corrupt(format!("HID {hid:#x}: bad range")))
    }

    /// All (key, value) records of a BTree-on-heap.
    pub fn bth(&self, hid: u32) -> Result<(usize, usize, Vec<Record<'_>>)> {
        let h = self.get(hid)?;
        if h.len() < 8 || h[0] != 0xB5 {
            return Err(corrupt("bad BTH header"));
        }
        let (kb, eb, levels, root) = (h[1] as usize, h[2] as usize, h[3], u32_at(h, 4));
        let mut out = Vec::new();
        if root != 0 {
            self.bth_walk(root, levels, kb, eb, &mut out)?;
        }
        Ok((kb, eb, out))
    }

    fn bth_walk<'s>(
        &'s self,
        hid: u32,
        level: u8,
        kb: usize,
        eb: usize,
        out: &mut Vec<(&'s [u8], &'s [u8])>,
    ) -> Result<()> {
        let d = self.get(hid)?;
        if level == 0 {
            if kb + eb == 0 {
                return Err(corrupt("bad BTH record size"));
            }
            for r in d.chunks_exact(kb + eb) {
                out.push((&r[..kb], &r[kb..]));
            }
        } else {
            for r in d.chunks_exact(kb + 4) {
                self.bth_walk(u32_at(r, kb), level - 1, kb, eb, out)?;
            }
        }
        Ok(())
    }
}

/// Shared by PC and TC: resolves HNIDs into the heap or into subnodes.
pub struct NodeCtx<'a> {
    pub ndb: &'a Ndb,
    pub heap: Heap<'a>,
    pub subs: Vec<SubEntry>,
}

impl<'a> NodeCtx<'a> {
    pub fn load(ndb: &'a Ndb, data: Bid, sub: Bid) -> Result<NodeCtx<'a>> {
        Ok(NodeCtx { ndb, heap: Heap::load(ndb, data)?, subs: ndb.subnodes(sub)? })
    }

    pub fn sub(&self, nid: Nid) -> Option<SubEntry> {
        self.subs.iter().find(|s| s.nid == nid).copied()
    }

    pub fn hnid_bytes(&self, hnid: u32) -> Result<Cow<'_, [u8]>> {
        if hnid & 0x1F == 0 {
            Ok(Cow::Borrowed(self.heap.get(hnid)?))
        } else {
            let s = self.sub(hnid).ok_or_else(|| corrupt(format!("subnode {hnid:#x} missing")))?;
            Ok(Cow::Owned(self.ndb.read_all(s.data)?))
        }
    }

    /// Decode a value stored out of line (or inline for <=4-byte types).
    fn decode(&self, ptype: u16, raw: &[u8]) -> Result<Value<'_>> {
        let inline = u32_at(raw, 0);
        Ok(match ptype {
            PT_SHORT => Value::Int(inline as u16 as i16 as i64),
            PT_LONG | PT_ERROR => Value::Int(inline as i32 as i64),
            PT_FLOAT => Value::Float(f32::from_bits(inline) as f64),
            PT_BOOLEAN => Value::Bool(inline & 0xFF != 0),
            _ => {
                if raw.len() >= 8 {
                    // TC cells hold 8-byte fixed values in the row itself.
                    return Ok(decode_fixed(ptype, raw).unwrap_or(Value::Other(ptype)));
                }
                let b = self.hnid_bytes(inline)?;
                decode_var(ptype, b)?
            }
        })
    }
}

fn decode_fixed(ptype: u16, raw: &[u8]) -> Option<Value<'static>> {
    let v = u64_at(raw, 0);
    Some(match ptype {
        PT_I8 | PT_CURRENCY => Value::Int(v as i64),
        PT_SYSTIME => Value::Time(v as i64),
        PT_DOUBLE | PT_APPTIME => Value::Float(f64::from_bits(v)),
        _ => return None,
    })
}

fn decode_var(ptype: u16, b: Cow<'_, [u8]>) -> Result<Value<'_>> {
    Ok(match ptype {
        PT_UNICODE => Value::Str(utf16(&b)),
        PT_STRING8 => Value::Str(ansi(&b)),
        PT_BINARY | PT_CLSID => Value::Bin(b),
        PT_I8 | PT_SYSTIME | PT_DOUBLE | PT_APPTIME | PT_CURRENCY if b.len() >= 8 => {
            decode_fixed(ptype, &b).unwrap_or(Value::Other(ptype))
        }
        PT_MV_LONG => Value::MultiInt(b.as_chunks::<4>().0.iter().map(|c| u32_at(c, 0) as i32 as i64).collect()),
        PT_MV_I8 => Value::MultiInt(b.as_chunks::<8>().0.iter().map(|c| u64_at(c, 0) as i64).collect()),
        PT_MV_UNICODE | PT_MV_STRING8 | PT_MV_BINARY => {
            let items = mv_items(&b)?;
            match ptype {
                PT_MV_UNICODE => Value::MultiStr(items.iter().map(|i| utf16(i)).collect()),
                PT_MV_STRING8 => Value::MultiStr(items.iter().map(|i| ansi(i)).collect()),
                _ => Value::MultiBin(items.iter().map(|i| i.to_vec()).collect()),
            }
        }
        t => Value::Other(t),
    })
}

fn mv_items(b: &[u8]) -> Result<Vec<&[u8]>> {
    if b.len() < 4 {
        return Ok(Vec::new());
    }
    let n = u32_at(b, 0) as usize;
    if 4 + n * 4 > b.len() {
        return Err(corrupt("bad multi-value"));
    }
    let offs: Vec<usize> = (0..n).map(|i| u32_at(b, 4 + i * 4) as usize).collect();
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let end = if i + 1 < n { offs[i + 1] } else { b.len() };
        out.push(b.get(offs[i]..end).ok_or_else(|| corrupt("bad multi-value"))?);
    }
    Ok(out)
}

/// Property context: a node's property bag (message, folder, attachment...).
pub struct Pc<'a> {
    pub ctx: NodeCtx<'a>,
    /// (prop id, prop type, raw 4-byte value), sorted by id.
    props: Vec<(u16, u16, [u8; 4])>,
}

impl<'a> Pc<'a> {
    pub fn load(ndb: &'a Ndb, data: Bid, sub: Bid) -> Result<Pc<'a>> {
        Self::from_ctx(NodeCtx::load(ndb, data, sub)?)
    }

    pub fn from_ctx(ctx: NodeCtx<'a>) -> Result<Pc<'a>> {
        if ctx.heap.client_sig != 0xBC {
            return Err(corrupt("node is not a property context"));
        }
        let (kb, eb, recs) = ctx.heap.bth(ctx.heap.user_root)?;
        if kb != 2 || eb != 6 {
            return Err(corrupt("bad PC BTH sizes"));
        }
        let mut props: Vec<_> =
            recs.iter().map(|(k, v)| (u16_at(k, 0), u16_at(v, 0), v[2..6].try_into().unwrap())).collect();
        props.sort_unstable_by_key(|p| p.0);
        Ok(Pc { ctx, props })
    }

    pub fn has(&self, id: u16) -> bool {
        self.props.binary_search_by_key(&id, |p| p.0).is_ok()
    }

    pub fn get(&self, id: u16) -> Option<Value<'_>> {
        let i = self.props.binary_search_by_key(&id, |p| p.0).ok()?;
        let (_, t, raw) = &self.props[i];
        self.ctx.decode(*t, raw).ok()
    }

    pub fn str(&self, id: u16) -> Option<String> {
        self.get(id).and_then(|v| v.into_string()).filter(|s| !s.is_empty())
    }
    pub fn int(&self, id: u16) -> Option<i64> {
        self.get(id).and_then(|v| v.as_int())
    }
    pub fn time(&self, id: u16) -> Option<i64> {
        self.get(id).and_then(|v| v.as_time())
    }
    pub fn bytes(&self, id: u16) -> Option<Vec<u8>> {
        self.get(id).and_then(|v| v.into_bytes()).map(|b| b.into_owned())
    }
    pub fn ids(&self) -> impl Iterator<Item = (u16, u16)> + '_ {
        self.props.iter().map(|p| (p.0, p.1))
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Column {
    pub tag: u32,
    pub off: usize,
    pub size: usize,
    pub bit: usize,
}

/// Table context: folder hierarchy/contents tables, recipients, attachments.
pub struct Tc<'a> {
    pub ctx: NodeCtx<'a>,
    pub cols: Vec<Column>,
    ceb: usize,
    row_size: usize,
    pub row_count: usize,
    rows: RowSource,
}

enum RowSource {
    Heap(u32),
    Blocks(Vec<Bid>),
}

impl<'a> Tc<'a> {
    pub fn load(ndb: &'a Ndb, data: Bid, sub: Bid) -> Result<Tc<'a>> {
        let ctx = NodeCtx::load(ndb, data, sub)?;
        if ctx.heap.client_sig != 0x7C {
            return Err(corrupt("node is not a table context"));
        }
        let info = ctx.heap.get(ctx.heap.user_root)?;
        if info.len() < 22 || info[0] != 0x7C {
            return Err(corrupt("bad TCINFO"));
        }
        let ncols = info[1] as usize;
        if info.len() < 22 + ncols * 8 {
            return Err(corrupt("bad TCINFO"));
        }
        let ceb = u16_at(info, 6) as usize;
        let row_size = u16_at(info, 8) as usize;
        let row_index = u32_at(info, 10);
        let rows_hnid = u32_at(info, 14);
        let cols: Vec<Column> = (0..ncols)
            .map(|i| {
                let c = &info[22 + i * 8..30 + i * 8];
                Column { tag: u32_at(c, 0), off: u16_at(c, 4) as usize, size: c[6] as usize, bit: c[7] as usize }
            })
            .collect();
        for c in &cols {
            if c.off + c.size > row_size || ceb + c.bit / 8 >= row_size {
                return Err(corrupt("bad TC column"));
            }
        }
        let rows = if rows_hnid & 0x1F == 0 {
            RowSource::Heap(rows_hnid)
        } else {
            let s = ctx.sub(rows_hnid).ok_or_else(|| corrupt("TC row matrix subnode missing"))?;
            RowSource::Blocks(ndb.data_tree(s.data)?)
        };
        // The row count is the row matrix capacity: walking the row index
        // BTH instead would touch heap blocks all over a large table.
        let mut tc = Tc { ctx, cols, ceb, row_size, row_count: 0, rows };
        if row_index != 0 {
            tc.row_count = tc.matrix_rows()?;
        }
        Ok(tc)
    }

    /// Rows the row matrix can hold (whole rows per block).
    pub fn matrix_rows(&self) -> Result<usize> {
        if self.row_size == 0 {
            return Ok(0);
        }
        Ok(match &self.rows {
            RowSource::Heap(hid) => self.ctx.heap.get(*hid)?.len() / self.row_size,
            RowSource::Blocks(bids) => {
                let mut n = 0;
                for &b in bids {
                    n += self.ctx.ndb.block_len(b)? / self.row_size;
                }
                n
            }
        })
    }

    pub fn col(&self, tag: u32) -> Option<usize> {
        self.cols.iter().position(|c| c.tag == tag)
    }

    /// Column lookup by property id, whatever its type.
    pub fn col_id(&self, id: u16) -> Option<usize> {
        self.cols.iter().position(|c| (c.tag >> 16) as u16 == id)
    }

    /// Visit rows in storage order; return `false` from `f` to stop early.
    pub fn for_each_row(&self, f: &mut dyn FnMut(&Row<'_, 'a>) -> Result<bool>) -> Result<()> {
        if self.row_size == 0 || self.row_count == 0 {
            return Ok(());
        }
        let mut left = self.row_count;
        let mut visit = |blk: &[u8]| -> Result<bool> {
            for r in blk.chunks_exact(self.row_size) {
                if left == 0 {
                    return Ok(false);
                }
                left -= 1;
                if u32_at(r, 0) != 0 && !f(&Row { tc: self, data: r })? {
                    return Ok(false);
                }
            }
            Ok(left > 0)
        };
        match &self.rows {
            RowSource::Heap(hid) => {
                visit(self.ctx.heap.get(*hid)?)?;
            }
            RowSource::Blocks(bids) => {
                // Rows never span blocks; each block holds as many whole rows as fit.
                for &b in bids {
                    if !visit(&self.ctx.ndb.block(b)?)? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
}

pub struct Row<'t, 'a> {
    tc: &'t Tc<'a>,
    pub data: &'t [u8],
}

impl<'t, 'a> Row<'t, 'a> {
    pub fn id(&self) -> u32 {
        u32_at(self.data, 0)
    }

    pub fn exists(&self, col: usize) -> bool {
        let bit = self.tc.cols[col].bit;
        self.data[self.tc.ceb + bit / 8] & (0x80 >> (bit % 8)) != 0
    }

    pub fn get(&self, col: Option<usize>) -> Option<Value<'t>> {
        let col = col?;
        if !self.exists(col) {
            return None;
        }
        let c = self.tc.cols[col];
        let raw = &self.data[c.off..c.off + c.size];
        let ptype = c.tag as u16;
        let mut buf = [0u8; 8];
        let n = raw.len().min(8);
        buf[..n].copy_from_slice(&raw[..n]);
        self.tc.ctx.decode(ptype, &buf[..n.max(4)]).ok()
    }

    pub fn str(&self, col: Option<usize>) -> Option<String> {
        self.get(col).and_then(|v| v.into_string()).filter(|s| !s.is_empty())
    }
    pub fn int(&self, col: Option<usize>) -> Option<i64> {
        self.get(col).and_then(|v| v.as_int())
    }
    pub fn time(&self, col: Option<usize>) -> Option<i64> {
        self.get(col).and_then(|v| v.as_time())
    }
}
