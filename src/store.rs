//! Messaging layer ([MS-PST] section 2.4): folders, messages, recipients,
//! attachments.

use crate::error::{Error, Result};
use crate::ltp::{Pc, Tc, Value};
use crate::ndb::{Ndb, Nid};

pub const NID_MESSAGE_STORE: Nid = 0x21;
pub const NID_ROOT_FOLDER: Nid = 0x122;
const NID_RECIPIENT_TABLE: Nid = 0x692;
const NID_ATTACHMENT_TABLE: Nid = 0x671;

const NID_TYPE_NORMAL_FOLDER: u32 = 0x02;
const NID_TYPE_HIERARCHY_TABLE: u32 = 0x0D;
const NID_TYPE_CONTENTS_TABLE: u32 = 0x0E;

pub mod tag {
    pub const MESSAGE_CLASS: u16 = 0x001A;
    pub const SUBJECT: u16 = 0x0037;
    pub const CLIENT_SUBMIT_TIME: u16 = 0x0039;
    pub const SENT_REPRESENTING_NAME: u16 = 0x0042;
    pub const SENT_REPRESENTING_ADDRTYPE: u16 = 0x0064;
    pub const SENT_REPRESENTING_EMAIL: u16 = 0x0065;
    pub const CONVERSATION_TOPIC: u16 = 0x0070;
    pub const TRANSPORT_HEADERS: u16 = 0x007D;
    pub const IMPORTANCE: u16 = 0x0017;
    pub const SENDER_NAME: u16 = 0x0C1A;
    pub const SENDER_ADDRTYPE: u16 = 0x0C1E;
    pub const SENDER_EMAIL: u16 = 0x0C1F;
    pub const DISPLAY_CC: u16 = 0x0E03;
    pub const DISPLAY_TO: u16 = 0x0E04;
    pub const MESSAGE_DELIVERY_TIME: u16 = 0x0E06;
    pub const MESSAGE_FLAGS: u16 = 0x0E07;
    pub const MESSAGE_SIZE: u16 = 0x0E08;
    pub const ATTACH_SIZE: u16 = 0x0E20;
    pub const BODY: u16 = 0x1000;
    pub const RTF_COMPRESSED: u16 = 0x1009;
    pub const HTML: u16 = 0x1013;
    pub const INTERNET_MESSAGE_ID: u16 = 0x1035;
    pub const IN_REPLY_TO: u16 = 0x1042;
    pub const DISPLAY_NAME: u16 = 0x3001;
    pub const ADDRTYPE: u16 = 0x3002;
    pub const EMAIL_ADDRESS: u16 = 0x3003;
    pub const CREATION_TIME: u16 = 0x3007;
    pub const CONTENT_COUNT: u16 = 0x3602;
    pub const CONTENT_UNREAD: u16 = 0x3603;
    pub const CONTAINER_CLASS: u16 = 0x3613;
    pub const IPM_APPOINTMENT_ENTRYID: u16 = 0x36D0;
    pub const IPM_SUBTREE_ENTRYID: u16 = 0x35E0;
    pub const IPM_WASTEBASKET_ENTRYID: u16 = 0x35E3;
    pub const ADDITIONAL_REN_ENTRYIDS: u16 = 0x36D8;
    pub const SOURCE_KEY: u16 = 0x65E0;
    pub const ATTACH_DATA: u16 = 0x3701;
    pub const ATTACH_FILENAME: u16 = 0x3704;
    pub const ATTACH_METHOD: u16 = 0x3705;
    pub const ATTACH_LONG_FILENAME: u16 = 0x3707;
    pub const ATTACH_MIME_TAG: u16 = 0x370E;
    pub const ATTACH_CONTENT_ID: u16 = 0x3712;
    pub const SMTP_ADDRESS: u16 = 0x39FE;
    pub const PREVIEW: u16 = 0x3FD9;
    pub const INTERNET_CPID: u16 = 0x3FDE;
    pub const RECIPIENT_TYPE: u16 = 0x0C15;
    pub const SENDER_SMTP: u16 = 0x5D01;
    pub const SENT_REPRESENTING_SMTP: u16 = 0x5D02;
    pub const ATTACHMENT_HIDDEN: u16 = 0x7FFE;
}

pub const MSGFLAG_READ: i64 = 0x01;
pub const MSGFLAG_HASATTACH: i64 = 0x10;

/// FILETIME of the Unix epoch.
const UNIX_EPOCH_FT: i64 = 116_444_736_000_000_000;

pub fn filetime_to_unix_nanos(ft: i64) -> i128 {
    (ft as i128 - UNIX_EPOCH_FT as i128) * 100
}

pub fn unix_nanos_to_filetime(ns: i128) -> i64 {
    (ns / 100 + UNIX_EPOCH_FT as i128) as i64
}

#[derive(Debug, Clone)]
pub struct Folder {
    pub nid: Nid,
    pub parent: Option<Nid>,
    pub name: String,
    /// Slash-separated path below the top of the mailbox ("Inbox/Projects").
    pub path: String,
    pub class: Option<String>,
    pub count: i64,
    pub unread: i64,
    pub depth: usize,
}

pub struct Store {
    pub ndb: Ndb,
}

/// A well-known folder and how it was found: `entry_id_nid`,
/// `entry_id_source_key`, `name` or `not_found` (the Inbox: `entry_ids` when
/// it carries the well-known folder entry ids).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub nid: Option<Nid>,
    pub method: &'static str,
}

impl Default for Found {
    fn default() -> Self {
        Found { nid: None, method: "not_found" }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WellKnown {
    pub inbox: Found,
    pub deleted_items: Found,
    pub junk: Found,
}

/// Default localized names (lowercase), the last resort.
pub const DELETED_ITEMS_NAMES: &[&str] = &[
    "deleted items",
    "éléments supprimés",
    "elements supprimes",
    "gelöschte elemente",
    "verwijderde items",
    "elementos eliminados",
    "elementos borrados",
    "posta eliminata",
    "itens excluídos",
    "itens eliminados",
];
pub const JUNK_NAMES: &[&str] = &[
    "junk email",
    "junk e-mail",
    "courrier indésirable",
    "courrier indesirable",
    "junk-e-mail",
    "ongewenste e-mail",
    "correo no deseado",
    "posta indesiderata",
    "lixo eletrônico",
    "lixo eletrónico",
    "correio eletrónico de lixo",
];

/// Folder NID of a PST-style entry id (flags 4, provider UID 16, NID 4).
pub fn entry_id_nid(e: &[u8]) -> Option<Nid> {
    (e.len() == 24)
        .then(|| u32::from_le_bytes(e[20..24].try_into().unwrap()))
        .filter(|n| n & 0x1F == NID_TYPE_NORMAL_FOLDER)
}

/// Source key (PidTagSourceKey: database GUID 16 + global counter 6) of an
/// Exchange folder entry id (flags 4, provider UID 16, folder type 2,
/// database GUID 16, global counter 6, pad 2).
pub fn entry_id_source_key(e: &[u8]) -> Option<&[u8]> {
    (e.len() == 46).then(|| &e[22..44])
}

/// The folder of `entry_id`, by NID then by source key (`source_key` reads a
/// folder's PidTagSourceKey), else a top-level folder named one of `names`.
pub fn locate(
    folders: &[Folder],
    entry_id: Option<&[u8]>,
    names: &[&str],
    source_key: &mut dyn FnMut(Nid) -> Result<Option<Vec<u8>>>,
) -> Result<Found> {
    let found = |nid, method| Ok(Found { nid: Some(nid), method });
    if let Some(e) = entry_id {
        if let Some(n) = entry_id_nid(e).filter(|n| folders.iter().any(|f| f.nid == *n)) {
            return found(n, "entry_id_nid");
        }
        if let Some(k) = entry_id_source_key(e) {
            for f in folders {
                if source_key(f.nid)?.as_deref() == Some(k) {
                    return found(f.nid, "entry_id_source_key");
                }
            }
        }
    }
    match folders.iter().find(|f| f.depth == 0 && names.contains(&f.name.to_lowercase().as_str())) {
        Some(f) => found(f.nid, "name"),
        None => Ok(Found::default()),
    }
}

/// Remove the subject-prefix marker MAPI may put in front of PT_UNICODE
/// subjects ("\u{1}" followed by the prefix length).
pub fn clean_subject(s: String) -> String {
    if s.starts_with('\u{1}') { s.chars().skip(2).collect() } else { s }
}

impl Store {
    pub fn open(path: &std::path::Path) -> Result<Store> {
        Ok(Store { ndb: Ndb::open(path)? })
    }

    fn table(&self, nid: Nid) -> Result<Option<Tc<'_>>> {
        match self.ndb.node(nid)? {
            Some(n) => Ok(Some(Tc::load(&self.ndb, n.data, n.sub)?)),
            None => Ok(None),
        }
    }

    pub fn pc(&self, nid: Nid) -> Result<Pc<'_>> {
        let n = self.ndb.node(nid)?.ok_or_else(|| Error::NotFound(format!("node {nid:#x}")))?;
        Pc::load(&self.ndb, n.data, n.sub)
    }

    /// The folder tree, depth-first. Paths are relative to IPM_SUBTREE (the
    /// visible top of the mailbox); folders outside it are omitted unless
    /// `all` is set.
    pub fn folders(&self, all: bool) -> Result<Vec<Folder>> {
        let mut out = Vec::new();
        let ipm = self.ipm_subtree()?;
        self.walk_folders(NID_ROOT_FOLDER, None, "", 0, all, false, ipm, &mut out)?;
        Ok(out)
    }

    /// NID of the IPM subtree from the store's PR_IPM_SUBTREE_ENTRYID, whose
    /// last 4 bytes are a NID in PST-style entry ids. OST files carry no such
    /// entry id; their subtree is found by name ("IPM_SUBTREE").
    fn ipm_subtree(&self) -> Result<Option<Nid>> {
        let pc = self.pc(NID_MESSAGE_STORE)?;
        Ok(pc.bytes(tag::IPM_SUBTREE_ENTRYID).and_then(|e| entry_id_nid(&e)))
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_folders(
        &self,
        nid: Nid,
        parent: Option<Nid>,
        prefix: &str,
        depth: usize,
        all: bool,
        in_ipm: bool,
        ipm: Option<Nid>,
        out: &mut Vec<Folder>,
    ) -> Result<()> {
        if depth > 64 {
            return Ok(());
        }
        let Some(tc) = self.table((nid & !0x1F) | NID_TYPE_HIERARCHY_TABLE)? else {
            return Ok(());
        };
        let (c_name, c_count, c_unread, c_class) = (
            tc.col_id(tag::DISPLAY_NAME),
            tc.col_id(tag::CONTENT_COUNT),
            tc.col_id(tag::CONTENT_UNREAD),
            tc.col_id(tag::CONTAINER_CLASS),
        );
        let mut children = Vec::new();
        tc.for_each_row(&mut |r| {
            let id = r.id();
            if id & 0x1F == NID_TYPE_NORMAL_FOLDER && id != nid {
                children.push((
                    id,
                    r.str(c_name).unwrap_or_default(),
                    r.int(c_count).unwrap_or(0),
                    r.int(c_unread).unwrap_or(0),
                    r.str(c_class),
                ));
            }
            Ok(true)
        })?;
        for (id, name, count, unread, class) in children {
            let is_ipm_root = !in_ipm && (Some(id) == ipm || (ipm.is_none() && name == "IPM_SUBTREE"));
            let visible = in_ipm || all;
            let path = if prefix.is_empty() {
                name.replace('/', "\\/")
            } else {
                format!("{prefix}/{}", name.replace('/', "\\/"))
            };
            if visible {
                out.push(Folder {
                    nid: id,
                    parent: parent.filter(|_| in_ipm || all),
                    name,
                    path: path.clone(),
                    class,
                    count,
                    unread,
                    depth,
                });
            }
            let (child_prefix, child_depth) = if is_ipm_root && !all { (String::new(), 0) } else { (path, depth + 1) };
            self.walk_folders(id, Some(id), &child_prefix, child_depth, all, in_ipm || is_ipm_root, ipm, out)?;
        }
        Ok(())
    }

    /// The Inbox: the mail folder carrying the well-known folder entry ids
    /// (PR_IPM_APPOINTMENT_ENTRYID...), else the usual localized names.
    pub fn inbox(&self, folders: &[Folder]) -> Option<Nid> {
        self.find_inbox(folders).nid
    }

    fn find_inbox(&self, folders: &[Folder]) -> Found {
        let mail = |f: &&Folder| {
            f.depth == 0 && f.class.as_deref().is_none_or(|c| c.starts_with("IPF.Note") || c.starts_with("IPF.Imap"))
        };
        for f in folders.iter().filter(mail) {
            if let Ok(pc) = self.pc(f.nid)
                && pc.has(tag::IPM_APPOINTMENT_ENTRYID)
            {
                return Found { nid: Some(f.nid), method: "entry_ids" };
            }
        }
        const NAMES: &[&str] = &[
            "inbox",
            "boîte de réception",
            "boite de réception",
            "posteingang",
            "postvak in",
            "bandeja de entrada",
            "posta in arrivo",
            "caixa de entrada",
            "inkorgen",
            "indbakke",
            "innboks",
            "saapuneet",
        ];
        match folders.iter().filter(mail).find(|f| NAMES.contains(&f.name.to_lowercase().as_str())) {
            Some(f) => Found { nid: Some(f.nid), method: "name" },
            None => Found::default(),
        }
    }

    /// Inbox, Deleted Items and Junk E-mail among `folders`. Deleted Items:
    /// PidTagIpmWastebasketEntryId of the store; Junk: entry 4 of
    /// PidTagAdditionalRenEntryIds of the Inbox, else of the root folder;
    /// see `locate`.
    pub fn well_known(&self, folders: &[Folder]) -> Result<WellKnown> {
        let inbox = self.find_inbox(folders);
        let waste = self.pc(NID_MESSAGE_STORE)?.bytes(tag::IPM_WASTEBASKET_ENTRYID);
        let mut junk = None;
        for nid in inbox.nid.into_iter().chain([NID_ROOT_FOLDER]) {
            if let Some(pc) = self.pc_opt(nid)?
                && let Some(Value::MultiBin(mut ids)) = pc.get(tag::ADDITIONAL_REN_ENTRYIDS)
                && ids.get(4).is_some_and(|e| !e.is_empty())
            {
                junk = Some(ids.swap_remove(4));
                break;
            }
        }
        let mut key = |nid| Ok(self.pc_opt(nid)?.and_then(|p| p.bytes(tag::SOURCE_KEY)));
        Ok(WellKnown {
            inbox,
            deleted_items: locate(folders, waste.as_deref(), DELETED_ITEMS_NAMES, &mut key)?,
            junk: locate(folders, junk.as_deref(), JUNK_NAMES, &mut key)?,
        })
    }

    /// The node's property context, None when the node is missing.
    fn pc_opt(&self, nid: Nid) -> Result<Option<Pc<'_>>> {
        match self.pc(nid) {
            Ok(p) => Ok(Some(p)),
            Err(Error::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Visit the rows of a folder's contents table.
    pub fn scan_folder(&self, folder: Nid, f: &mut dyn FnMut(&RowSummary) -> bool) -> Result<()> {
        let Some(tc) = self.table((folder & !0x1F) | NID_TYPE_CONTENTS_TABLE)? else {
            return Ok(());
        };
        let c = [
            tc.col_id(tag::MESSAGE_DELIVERY_TIME),
            tc.col_id(tag::CLIENT_SUBMIT_TIME),
            tc.col_id(tag::CREATION_TIME),
            tc.col_id(tag::MESSAGE_FLAGS),
            tc.col_id(tag::SUBJECT),
            tc.col_id(tag::MESSAGE_CLASS),
            tc.col_id(tag::SENT_REPRESENTING_NAME),
            tc.col_id(tag::INTERNET_MESSAGE_ID),
            tc.col_id(tag::MESSAGE_SIZE),
        ];
        tc.for_each_row(&mut |r| {
            let received = r.time(c[0]).or_else(|| r.time(c[1])).or_else(|| r.time(c[2])).unwrap_or(0);
            let row = RowSummary {
                nid: r.id(),
                folder,
                received,
                submitted: r.time(c[1]).unwrap_or(0),
                size: r.int(c[8]).unwrap_or(0),
                flags: r.int(c[3]).unwrap_or(0),
                subject: LazyStr(&|| r.str(c[4]).map(clean_subject)),
                class: LazyStr(&|| r.str(c[5])),
                from_name: LazyStr(&|| r.str(c[6])),
                message_id: LazyStr(&|| r.str(c[7])),
            };
            Ok(f(&row))
        })
    }

    pub fn message(&self, nid: Nid) -> Result<Message<'_>> {
        let node = self.ndb.node(nid)?.ok_or_else(|| Error::NotFound(format!("message {nid}")))?;
        if node.nid & 0x1F != 0x04 {
            return Err(Error::NotFound(format!("{nid} is not a message id")));
        }
        Ok(Message { nid, folder: node.parent, pc: Pc::load(&self.ndb, node.data, node.sub)? })
    }
}

/// A string column decoded only when a filter or the output needs it.
pub struct LazyStr<'f>(pub &'f dyn Fn() -> Option<String>);

impl LazyStr<'_> {
    pub fn get(&self) -> Option<String> {
        (self.0)()
    }
}

pub struct RowSummary<'f> {
    pub nid: Nid,
    pub folder: Nid,
    pub received: i64,
    /// PR_CLIENT_SUBMIT_TIME, 0 when absent.
    pub submitted: i64,
    /// PR_MESSAGE_SIZE, 0 when absent.
    pub size: i64,
    pub flags: i64,
    pub subject: LazyStr<'f>,
    pub class: LazyStr<'f>,
    pub from_name: LazyStr<'f>,
    pub message_id: LazyStr<'f>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Address {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Recipient {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(flatten)]
    pub addr: Address,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Attachment {
    pub index: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_id: Option<String>,
    /// by_value, embedded_message, by_reference, ole...
    pub method: &'static str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub inline: bool,
    #[serde(skip)]
    pub nid: Nid,
}

pub struct Message<'a> {
    pub nid: Nid,
    pub folder: Nid,
    pub pc: Pc<'a>,
}

fn smtp_or_none(addrtype: Option<String>, email: Option<String>) -> Option<String> {
    match (addrtype.as_deref(), email) {
        (Some(t), Some(e)) if t.eq_ignore_ascii_case("SMTP") => Some(e),
        (None, Some(e)) if e.contains('@') && !e.starts_with('/') => Some(e),
        _ => None,
    }
}

/// Extract the address of an Exchange X500 DN as a fallback identifier
/// ("/O=.../CN=RECIPIENTS/CN=abc-jdoe" -> "jdoe").
fn ex_alias(dn: &str) -> Option<String> {
    let last = dn.rsplit("/CN=").next().or_else(|| dn.rsplit("/cn=").next())?;
    Some(last.split_once('-').map(|(_, a)| a).unwrap_or(last).to_string())
}

impl<'a> Message<'a> {
    pub fn received(&self) -> Option<i64> {
        self.pc
            .time(tag::MESSAGE_DELIVERY_TIME)
            .or_else(|| self.pc.time(tag::CLIENT_SUBMIT_TIME))
            .or_else(|| self.pc.time(tag::CREATION_TIME))
    }

    pub fn subject(&self) -> Option<String> {
        self.pc.str(tag::SUBJECT).map(clean_subject)
    }

    pub fn from(&self) -> Address {
        let p = &self.pc;
        let name = p.str(tag::SENT_REPRESENTING_NAME).or_else(|| p.str(tag::SENDER_NAME));
        let email = p
            .str(tag::SENT_REPRESENTING_SMTP)
            .or_else(|| smtp_or_none(p.str(tag::SENT_REPRESENTING_ADDRTYPE), p.str(tag::SENT_REPRESENTING_EMAIL)))
            .or_else(|| p.str(tag::SENDER_SMTP))
            .or_else(|| smtp_or_none(p.str(tag::SENDER_ADDRTYPE), p.str(tag::SENDER_EMAIL)))
            .or_else(|| p.str(tag::SENT_REPRESENTING_EMAIL).or_else(|| p.str(tag::SENDER_EMAIL)));
        Address { name, email }
    }

    /// Text that `--from` filters match against: names and all known addresses.
    pub fn from_haystack(&self) -> String {
        let a = self.from();
        let mut s = format!("{} {}", a.name.unwrap_or_default(), a.email.unwrap_or_default());
        for t in [tag::SENDER_NAME, tag::SENDER_SMTP, tag::SENDER_EMAIL, tag::SENT_REPRESENTING_EMAIL] {
            if let Some(v) = self.pc.str(t) {
                s.push(' ');
                if v.starts_with('/') {
                    s.push_str(&ex_alias(&v).unwrap_or(v));
                } else {
                    s.push_str(&v);
                }
            }
        }
        s.to_lowercase()
    }

    pub fn flags(&self) -> i64 {
        self.pc.int(tag::MESSAGE_FLAGS).unwrap_or(0)
    }

    pub fn recipients(&self) -> Result<Vec<Recipient>> {
        let mut out = Vec::new();
        let Some(s) = self.pc.ctx.sub(NID_RECIPIENT_TABLE) else {
            return Ok(out);
        };
        let tc = Tc::load(self.pc.ctx.ndb, s.data, s.sub)?;
        let c = [
            tc.col_id(tag::RECIPIENT_TYPE),
            tc.col_id(tag::DISPLAY_NAME),
            tc.col_id(tag::SMTP_ADDRESS),
            tc.col_id(tag::ADDRTYPE),
            tc.col_id(tag::EMAIL_ADDRESS),
        ];
        tc.for_each_row(&mut |r| {
            let kind = match r.int(c[0]).unwrap_or(1) & 0xF {
                2 => "cc",
                3 => "bcc",
                _ => "to",
            };
            let email = r.str(c[2]).or_else(|| smtp_or_none(r.str(c[3]), r.str(c[4]))).or_else(|| r.str(c[4]));
            out.push(Recipient { kind, addr: Address { name: r.str(c[1]), email } });
            Ok(true)
        })?;
        Ok(out)
    }

    pub fn attachments(&self) -> Result<Vec<Attachment>> {
        let mut out = Vec::new();
        let Some(s) = self.pc.ctx.sub(NID_ATTACHMENT_TABLE) else {
            return Ok(out);
        };
        let tc = Tc::load(self.pc.ctx.ndb, s.data, s.sub)?;
        let mut nids = Vec::new();
        tc.for_each_row(&mut |r| {
            nids.push(r.id());
            Ok(true)
        })?;
        for (index, nid) in nids.into_iter().enumerate() {
            let Some(sub) = self.pc.ctx.sub(nid) else { continue };
            let pc = Pc::load(self.pc.ctx.ndb, sub.data, sub.sub)?;
            let method = match pc.int(tag::ATTACH_METHOD).unwrap_or(1) {
                0 => "none",
                2..=4 => "by_reference",
                5 => "embedded_message",
                6 => "ole",
                7 => "web_reference",
                _ => "by_value",
            };
            out.push(Attachment {
                index,
                name: pc
                    .str(tag::ATTACH_LONG_FILENAME)
                    .or_else(|| pc.str(tag::ATTACH_FILENAME))
                    .or_else(|| pc.str(tag::DISPLAY_NAME)),
                size: pc.int(tag::ATTACH_SIZE),
                mime_type: pc.str(tag::ATTACH_MIME_TAG),
                content_id: pc.str(tag::ATTACH_CONTENT_ID),
                method,
                inline: pc.get(tag::ATTACHMENT_HIDDEN).and_then(|v| v.as_bool()).unwrap_or(false),
                nid,
            });
        }
        Ok(out)
    }

    /// Raw content of a by-value attachment.
    pub fn attachment_data(&self, att: &Attachment) -> Result<Vec<u8>> {
        let sub = self.pc.ctx.sub(att.nid).ok_or_else(|| Error::NotFound("attachment".into()))?;
        let pc = Pc::load(self.pc.ctx.ndb, sub.data, sub.sub)?;
        match pc.get(tag::ATTACH_DATA) {
            Some(Value::Bin(b)) => Ok(b.into_owned()),
            _ => Err(Error::NotFound(format!("attachment {} has no binary content ({})", att.index, att.method))),
        }
    }
}

pub fn is_mail_folder(f: &Folder) -> bool {
    f.class.as_deref().is_none_or(|c| {
        (c.starts_with("IPF.Note") && !c.starts_with("IPF.Note.OutlookHomepage")) || c.starts_with("IPF.Imap")
    })
}

pub fn resolve_folders(store: &Store, specs: &[String]) -> Result<Vec<Folder>> {
    let folders = store.folders(false)?;
    let inbox = |folders: &[Folder]| -> Result<Folder> {
        let nid = store.inbox(folders).ok_or_else(|| Error::NotFound("Inbox folder".into()))?;
        Ok(folders.iter().find(|f| f.nid == nid).unwrap().clone())
    };
    if specs.is_empty() {
        return Ok(vec![inbox(&folders)?]);
    }
    let mut out: Vec<Folder> = Vec::new();
    for spec in specs {
        let s = spec.trim().trim_matches('/');
        let lower = s.to_lowercase();
        let found: Vec<Folder> = if lower == "all" || s == "*" {
            for f in folders.iter().filter(|f| is_mail_folder(f)) {
                if !out.iter().any(|o| o.nid == f.nid) {
                    out.push(f.clone());
                }
            }
            continue;
        } else if lower == "inbox" {
            vec![inbox(&folders)?]
        } else if let Some(f) = parse_id(s).ok().and_then(|id| folders.iter().find(|f| f.nid == id)) {
            vec![f.clone()]
        } else {
            let by_path: Vec<_> = folders.iter().filter(|f| f.path.to_lowercase() == lower).cloned().collect();
            if !by_path.is_empty() {
                by_path
            } else {
                let by_name: Vec<_> = folders.iter().filter(|f| f.name.to_lowercase() == lower).cloned().collect();
                if by_name.len() > 1 {
                    let paths: Vec<_> = by_name.iter().map(|f| f.path.as_str()).collect();
                    return Err(Error::Usage(format!("folder '{s}' is ambiguous: {}", paths.join(", "))));
                }
                by_name
            }
        };
        if found.is_empty() {
            return Err(Error::NotFound(format!("folder '{s}' (see `outlook-cli folders`)")));
        }
        for f in found {
            if !out.iter().any(|o| o.nid == f.nid) {
                out.push(f);
            }
        }
    }
    Ok(out)
}

pub fn parse_id(s: &str) -> Result<Nid> {
    let s = s.trim();
    let r = match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|_| Error::Usage(format!("invalid message id '{s}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(nid: Nid, name: &str, depth: usize) -> Folder {
        Folder {
            nid,
            parent: None,
            name: name.into(),
            path: name.into(),
            class: Some("IPF.Note".into()),
            count: 0,
            unread: 0,
            depth,
        }
    }

    fn pst_id(nid: Nid) -> Vec<u8> {
        let mut e = vec![0u8; 20];
        e.extend(nid.to_le_bytes());
        e
    }

    /// Exchange folder entry id whose source key is `key` (22 bytes).
    fn exchange_id(key: &[u8]) -> Vec<u8> {
        let mut e = vec![0u8; 20];
        e.extend([1, 0]);
        e.extend(key);
        e.extend([0, 0]);
        e
    }

    const KEY: [u8; 22] = [7; 22];

    #[test]
    fn entry_id_shapes() {
        assert_eq!(entry_id_nid(&pst_id(0x8062)), Some(0x8062));
        assert_eq!(entry_id_nid(&pst_id(0x8064)), None, "a message NID is not a folder");
        assert_eq!(entry_id_nid(&pst_id(0x8062)[..23]), None);
        let e = exchange_id(&KEY);
        assert_eq!(e.len(), 46);
        assert_eq!(entry_id_source_key(&e), Some(&KEY[..]));
        assert_eq!(entry_id_nid(&e), None);
        assert_eq!(entry_id_source_key(&pst_id(0x8062)), None);
        assert_eq!(entry_id_source_key(&e[..45]), None);
    }

    #[test]
    fn locate_order() {
        let fs = [
            folder(0x22, "Inbox", 0),
            folder(0x42, "Éléments supprimés", 0),
            folder(0x62, "Archive", 0),
            folder(0x82, "Deleted Items", 1),
            folder(0xA2, "Courrier indésirable", 0),
        ];
        let mut keys = |n: Nid| Ok((n == 0x62).then(|| KEY.to_vec()));
        let l = |id: Option<Vec<u8>>, names: &[&str], keys: &mut dyn FnMut(Nid) -> Result<Option<Vec<u8>>>| {
            locate(&fs, id.as_deref(), names, keys).unwrap()
        };
        let f = |n: Nid, method| Found { nid: Some(n), method };
        // NID first, even when the name points elsewhere.
        assert_eq!(l(Some(pst_id(0x62)), DELETED_ITEMS_NAMES, &mut keys), f(0x62, "entry_id_nid"));
        // Source key next.
        assert_eq!(l(Some(exchange_id(&KEY)), DELETED_ITEMS_NAMES, &mut keys), f(0x62, "entry_id_source_key"));
        // An id matching no folder falls back to the name (FR); depth 0 only.
        assert_eq!(l(Some(pst_id(0x1002)), DELETED_ITEMS_NAMES, &mut keys), f(0x42, "name"));
        assert_eq!(l(Some(exchange_id(&[9; 22])), DELETED_ITEMS_NAMES, &mut keys), f(0x42, "name"));
        assert_eq!(l(None, JUNK_NAMES, &mut keys), f(0xA2, "name"));
        assert_eq!(l(None, &["nothing"], &mut keys), Found::default());
        assert_eq!(Found::default().method, "not_found");
        // EN, case-insensitive; the nested "Deleted Items" is ignored.
        let en = [folder(0x22, "DELETED ITEMS", 0), folder(0x42, "Junk E-mail", 0), folder(0x62, "Deleted Items", 2)];
        let mut none = |_| Ok(None);
        assert_eq!(locate(&en, None, DELETED_ITEMS_NAMES, &mut none).unwrap(), f(0x22, "name"));
        assert_eq!(locate(&en, None, JUNK_NAMES, &mut none).unwrap(), f(0x42, "name"));
        // A source-key read error is returned (retried by the caller), not hidden.
        let mut bad = |_| Err(Error::Corrupt("x".into()));
        assert!(locate(&fs, Some(&exchange_id(&KEY)), DELETED_ITEMS_NAMES, &mut bad).is_err());
    }
}
