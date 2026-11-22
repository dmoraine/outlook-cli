//! Message bodies: plain text, HTML (converted to text) and compressed RTF
//! ([MS-OXRTFCP] LZFu, [MS-OXRTFEX] HTML de-encapsulation).

use crate::ltp::ansi;
use crate::store::{Message, tag};

/// Decode bytes of an HTML body according to PR_INTERNET_CPID.
pub fn decode_text(b: &[u8], cpid: Option<i64>) -> String {
    match cpid {
        Some(1200) => crate::ltp::utf16(b),
        Some(65001) => String::from_utf8_lossy(b).into_owned(),
        _ => match std::str::from_utf8(b) {
            Ok(s) => s.trim_end_matches('\0').to_string(),
            Err(_) => ansi(b),
        },
    }
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let v = if let Some(h) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(h, 16).ok()?
        } else {
            num.parse().ok()?
        };
        return char::from_u32(v).map(|c| if c == '\u{a0}' { ' ' } else { c });
    }
    Some(match name {
        "nbsp" | "ensp" | "emsp" | "thinsp" => ' ',
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "laquo" => '«',
        "raquo" => '»',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "bull" => '•',
        "middot" => '·',
        "euro" => '€',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "eacute" => 'é',
        "egrave" => 'è',
        "ecirc" => 'ê',
        "euml" => 'ë',
        "agrave" => 'à',
        "aacute" => 'á',
        "acirc" => 'â',
        "auml" => 'ä',
        "ccedil" => 'ç',
        "icirc" => 'î',
        "iuml" => 'ï',
        "ocirc" => 'ô',
        "ouml" => 'ö',
        "ugrave" => 'ù',
        "ucirc" => 'û',
        "uuml" => 'ü',
        "Eacute" => 'É',
        "Egrave" => 'È',
        "Agrave" => 'À',
        "Ccedil" => 'Ç',
        "szlig" => 'ß',
        "times" => '×',
        "zwnj" | "zwj" | "shy" => '\u{200b}',
        _ => return None,
    })
}

/// Convert HTML to readable plain text: drops markup, scripts and styles,
/// turns block elements into line breaks and decodes entities.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 3);
    let mut skip_until: Option<&'static str> = None;
    let mut pending_space = false;
    let mut i = 0;
    let b = html.as_bytes();

    // Block boundaries give one line break: Outlook wraps every line in a
    // <p class=MsoNormal>, so blank lines would double the text height.
    let newline = |out: &mut String| {
        while out.ends_with(' ') {
            out.pop();
        }
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    };

    while i < b.len() {
        if b[i] == b'<' {
            if html[i..].starts_with("<!--") {
                i = html[i..].find("-->").map(|p| i + p + 3).unwrap_or(b.len());
                continue;
            }
            let end = match html[i..].find('>') {
                Some(p) => i + p,
                None => break,
            };
            let tag = &html[i + 1..end];
            i = end + 1;
            let closing = tag.starts_with('/');
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == ':')
                .collect::<String>()
                .to_ascii_lowercase();
            if let Some(until) = skip_until {
                if closing && name == until {
                    skip_until = None;
                }
                continue;
            }
            match name.as_str() {
                "style" | "script" | "head" | "title" | "xml" if !closing && !tag.ends_with('/') => {
                    skip_until = Some(match name.as_str() {
                        "style" => "style",
                        "script" => "script",
                        "head" => "head",
                        "title" => "title",
                        _ => "xml",
                    });
                }
                "br" => {
                    while out.ends_with(' ') {
                        out.pop();
                    }
                    out.push('\n');
                    pending_space = false;
                }
                "p" | "div" | "tr" | "table" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote" | "ul" | "ol"
                | "pre" | "hr" | "o:p" => {
                    if name != "o:p" {
                        newline(&mut out);
                    }
                    pending_space = false;
                }
                "li" if !closing => {
                    newline(&mut out);
                    out.push_str("- ");
                    pending_space = false;
                }
                "td" | "th" if !closing => pending_space = true,
                _ => {}
            }
            continue;
        }
        if skip_until.is_some() {
            i += 1;
            continue;
        }
        // Text run up to the next tag.
        let next = html[i..].find('<').map(|p| i + p).unwrap_or(b.len());
        let text = &html[i..next];
        i = next;
        let mut chars = text.char_indices().peekable();
        while let Some((pos, c)) = chars.next() {
            let c = if c == '&' {
                let rest = &text[pos + 1..];
                match rest.find(';').filter(|&p| p <= 10).and_then(|p| entity(&rest[..p]).map(|e| (p, e))) {
                    Some((p, e)) => {
                        for _ in 0..=p {
                            chars.next();
                        }
                        e
                    }
                    None => '&',
                }
            } else {
                c
            };
            if c == '\u{200b}' || c == '\u{feff}' {
                continue;
            }
            if c.is_whitespace() {
                pending_space = true;
                continue;
            }
            if pending_space && !out.is_empty() && !out.ends_with('\n') && !out.ends_with(' ') {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        }
    }
    tidy(&out)
}

/// Trim line ends and collapse runs of blank lines into one.
pub fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
            out.push('\n');
        } else {
            blank = 0;
            out.push_str(line);
            out.push('\n');
        }
    }
    out.trim().to_string()
}

const LZFU_PREBUF: &[u8] = b"{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";

/// Decompress PR_RTF_COMPRESSED.
pub fn decompress_rtf(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 16 {
        return None;
    }
    let raw_size = u32::from_le_bytes(data[4..8].try_into().ok()?) as usize;
    let magic = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let body = &data[16..];
    match magic {
        0x414C_454D => return Some(body[..raw_size.min(body.len())].to_vec()),
        0x7546_5A4C => {}
        _ => return None,
    }
    let mut dict = [0u8; 4096];
    dict[..LZFU_PREBUF.len()].copy_from_slice(LZFU_PREBUF);
    let mut wpos = LZFU_PREBUF.len();
    let mut out = Vec::with_capacity(raw_size);
    let mut i = 0;
    'outer: while i < body.len() {
        let ctrl = body[i];
        i += 1;
        for bit in 0..8 {
            if i >= body.len() {
                break 'outer;
            }
            if ctrl & (1 << bit) != 0 {
                if i + 1 >= body.len() {
                    break 'outer;
                }
                let v = u16::from_be_bytes([body[i], body[i + 1]]) as usize;
                i += 2;
                let (off, len) = (v >> 4, (v & 0xF) + 2);
                if off == wpos {
                    break 'outer;
                }
                for k in 0..len {
                    let c = dict[(off + k) % 4096];
                    out.push(c);
                    dict[wpos] = c;
                    wpos = (wpos + 1) % 4096;
                }
            } else {
                let c = body[i];
                i += 1;
                out.push(c);
                dict[wpos] = c;
                wpos = (wpos + 1) % 4096;
            }
        }
    }
    out.truncate(raw_size);
    Some(out)
}

pub enum Rtf {
    /// HTML recovered from an RTF body that encapsulates it (\fromhtml1).
    Html(String),
    Text(String),
}

/// Extract text (or encapsulated HTML) from RTF.
pub fn rtf_to_text(rtf: &[u8]) -> Rtf {
    let from_html = rtf.windows(10).take(2000).any(|w| w == b"\\fromhtml1");
    #[derive(Clone, Copy)]
    struct G {
        skip: bool,
        htmlrtf: bool,
        htmltag: bool,
        uc: usize,
    }
    let mut stack = vec![G { skip: false, htmlrtf: false, htmltag: false, uc: 1 }];
    let mut out = String::new();
    let mut bytes: Vec<u8> = Vec::new(); // pending \'hh bytes (cp1252)
    let mut skip_chars = 0usize;
    let mut i = 0;
    let mut group_start = false;

    let flush = |bytes: &mut Vec<u8>, out: &mut String| {
        if !bytes.is_empty() {
            out.push_str(&ansi(bytes));
            bytes.clear();
        }
    };

    while i < rtf.len() {
        let c = rtf[i];
        let g = *stack.last().unwrap();
        let emit = !g.skip && (!from_html || g.htmltag || !g.htmlrtf);
        match c {
            b'{' => {
                flush(&mut bytes, &mut out);
                stack.push(G { htmltag: false, ..g });
                group_start = true;
                i += 1;
                continue;
            }
            b'}' => {
                flush(&mut bytes, &mut out);
                if stack.len() > 1 {
                    stack.pop();
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= rtf.len() {
                    break;
                }
                let n = rtf[i];
                if !n.is_ascii_alphabetic() {
                    i += 1;
                    match n {
                        b'\'' => {
                            let hex = rtf.get(i..i + 2).and_then(|h| std::str::from_utf8(h).ok());
                            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                                if skip_chars > 0 {
                                    skip_chars -= 1;
                                } else if emit {
                                    bytes.push(v);
                                }
                            }
                            i += 2;
                        }
                        b'*' => {
                            if group_start {
                                // Optional destination: skipped unless it carries HTML.
                                let rest = &rtf[i..rtf.len().min(i + 12)];
                                let is_tag = rest.starts_with(b"\\htmltag");
                                let top = stack.last_mut().unwrap();
                                if from_html && is_tag {
                                    top.htmltag = true;
                                } else {
                                    top.skip = true;
                                }
                            }
                        }
                        b'~' if emit => out.push(' '),
                        b'_' if emit => out.push('-'),
                        b'\\' | b'{' | b'}' if emit => {
                            flush(&mut bytes, &mut out);
                            out.push(n as char);
                        }
                        b'\r' | b'\n' if emit && !from_html => out.push('\n'),
                        _ => {}
                    }
                    group_start = false;
                    continue;
                }
                let start = i;
                while i < rtf.len() && rtf[i].is_ascii_alphabetic() {
                    i += 1;
                }
                let word = std::str::from_utf8(&rtf[start..i]).unwrap_or("");
                let nstart = i;
                if i < rtf.len() && rtf[i] == b'-' {
                    i += 1;
                }
                while i < rtf.len() && rtf[i].is_ascii_digit() {
                    i += 1;
                }
                let param: Option<i64> = std::str::from_utf8(&rtf[nstart..i]).ok().and_then(|s| s.parse().ok());
                if i < rtf.len() && rtf[i] == b' ' {
                    i += 1;
                }
                flush(&mut bytes, &mut out);
                let top = stack.last_mut().unwrap();
                match word {
                    "fonttbl" | "colortbl" | "stylesheet" | "info" | "pict" | "header" | "footer" | "headerl"
                    | "headerr" | "footerl" | "footerr" | "xmlnstbl" | "listtable" | "listoverridetable"
                    | "rsidtbl" | "generator" | "themedata" | "colorschememapping" | "latentstyles" | "datastore"
                    | "object"
                        if group_start =>
                    {
                        top.skip = true
                    }
                    "htmlrtf" => top.htmlrtf = param != Some(0),
                    "uc" => top.uc = param.unwrap_or(1).max(0) as usize,
                    "u" => {
                        if emit {
                            let v = param.unwrap_or(0);
                            let v = if v < 0 { v + 65536 } else { v } as u32;
                            if let Some(ch) = char::from_u32(v) {
                                out.push(ch);
                            }
                        }
                        skip_chars = top.uc;
                    }
                    "par" | "line" if emit && !from_html => out.push('\n'),
                    "par" | "line" if emit && from_html && !g.htmltag => out.push('\n'),
                    "tab" if emit => out.push('\t'),
                    "emdash" if emit => out.push('—'),
                    "endash" if emit => out.push('–'),
                    "lquote" if emit => out.push('‘'),
                    "rquote" if emit => out.push('’'),
                    "ldblquote" if emit => out.push('“'),
                    "rdblquote" if emit => out.push('”'),
                    "bullet" if emit => out.push('•'),
                    _ => {}
                }
                group_start = false;
                continue;
            }
            b'\r' | b'\n' => {
                i += 1;
            }
            _ => {
                if skip_chars > 0 {
                    skip_chars -= 1;
                } else if emit {
                    flush(&mut bytes, &mut out);
                    out.push(c as char);
                }
                i += 1;
            }
        }
        group_start = false;
    }
    flush(&mut bytes, &mut out);
    if from_html { Rtf::Html(out) } else { Rtf::Text(tidy(&out)) }
}

pub fn html_of(m: &Message) -> Option<String> {
    let cpid = m.pc.int(tag::INTERNET_CPID);
    if let Some(b) = m.pc.bytes(tag::HTML).filter(|b| !b.is_empty()) {
        return Some(decode_text(&b, cpid));
    }
    let rtf = decompress_rtf(&m.pc.bytes(tag::RTF_COMPRESSED)?)?;
    match rtf_to_text(&rtf) {
        Rtf::Html(h) => Some(h),
        Rtf::Text(_) => None,
    }
}

/// Plain-text body: PR_BODY, else the HTML body converted, else the RTF body.
pub fn text_of(m: &Message) -> Option<String> {
    if let Some(t) = m.pc.str(tag::BODY) {
        return Some(tidy(&t));
    }
    let cpid = m.pc.int(tag::INTERNET_CPID);
    if let Some(b) = m.pc.bytes(tag::HTML).filter(|b| !b.is_empty()) {
        return Some(html_to_text(&decode_text(&b, cpid)));
    }
    let rtf = decompress_rtf(&m.pc.bytes(tag::RTF_COMPRESSED)?)?;
    Some(match rtf_to_text(&rtf) {
        Rtf::Html(h) => html_to_text(&h),
        Rtf::Text(t) => t,
    })
}

pub fn truncate(s: String, max: usize) -> (String, bool) {
    if max == 0 {
        return (s, false);
    }
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prebuf_length() {
        assert_eq!(LZFU_PREBUF.len(), 207);
    }

    #[test]
    fn lzfu_spec_example() {
        // [MS-OXRTFCP] 3.1.1 compressed example for "{\rtf1\ansi\ansicpg1252\pard hello world}\r\n".
        let data: [u8; 61] = [
            0x2d, 0x00, 0x00, 0x00, 0x2b, 0x00, 0x00, 0x00, 0x4c, 0x5a, 0x46, 0x75, 0xf1, 0xc5, 0xc7, 0xa7, 0x03, 0x00,
            0x0a, 0x00, 0x72, 0x63, 0x70, 0x67, 0x31, 0x32, 0x35, 0x42, 0x32, 0x0a, 0xf3, 0x20, 0x68, 0x65, 0x6c, 0x09,
            0x00, 0x20, 0x62, 0x77, 0x05, 0xb0, 0x6c, 0x64, 0x7d, 0x0a, 0x80, 0x0f, 0xa0, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        let out = decompress_rtf(&data).unwrap();
        assert_eq!(out, b"{\\rtf1\\ansi\\ansicpg1252\\pard hello world}\r\n");
    }

    #[test]
    fn html_conversion() {
        let html = "<html><head><style>p{}</style></head><body><p>Bonjour&nbsp;Marie,</p>\
                    <div>Ligne&#233; 2<br>Ligne 3</div><ul><li>a</li><li>b</li></ul></body></html>";
        assert_eq!(html_to_text(html), "Bonjour Marie,\nLigneé 2\nLigne 3\n- a\n- b");
    }

    #[test]
    fn rtf_plain() {
        let rtf = br"{\rtf1\ansi{\fonttbl{\f0 Arial;}}\f0 Caf\'e9 cr\u232?me\par fin}";
        match rtf_to_text(rtf) {
            Rtf::Text(t) => assert_eq!(t, "Café crème\nfin"),
            Rtf::Html(_) => panic!(),
        }
    }

    #[test]
    fn rtf_fromhtml() {
        let rtf = br"{\rtf1\ansi\fromhtml1 {\*\htmltag19 <html>}{\*\htmltag34 <p>}\htmlrtf {\htmlrtf0 Salut \'e0 toi}\htmlrtf0 {\*\htmltag244 </p>}}";
        match rtf_to_text(rtf) {
            Rtf::Html(h) => assert_eq!(html_to_text(&h), "Salut à toi"),
            Rtf::Text(_) => panic!(),
        }
    }
}
