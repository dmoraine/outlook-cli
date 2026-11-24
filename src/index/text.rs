//! Pure text preparation for the index: quoted-reply / signature stripping
//! and paragraph chunking.

use std::ops::Range;

pub use super::doctor::{CHUNK_CHARS, CHUNK_OVERLAP};

/// The text written by the sender: quoted replies, forwarded headers and
/// signature / disclaimer tails removed. Falls back to the whole text when
/// nothing would be left (a bare forward).
pub fn strip_quotes(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let next = |i: usize| lines.get(i + 1).map_or("", |l| l.trim());
    let cut = (0..lines.len()).find(|&i| quote_start(lines[i], next(i))).unwrap_or(lines.len());
    let kept: Vec<&str> = lines[..cut].iter().copied().filter(|l| !l.trim_start().starts_with('>')).collect();
    let s = kept.join("\n").trim().to_string();
    if s.is_empty() { text.to_string() } else { s }
}

const SEPARATORS: &[&str] = &["original message", "message d'origine", "forwarded message", "message transféré"];
const HEADER_FROM: &[&str] = &["from:", "de :", "de:"];
const HEADER_NEXT: &[&str] =
    &["sent:", "envoyé :", "envoyé:", "date:", "date :", "to:", "à :", "à:", "subject:", "objet :", "objet:"];
const TAILS: &[&str] = &[
    "sent from my ",
    "envoyé de mon ",
    "envoyé depuis mon ",
    "get outlook for ",
    "obtenir outlook pour ",
    "this e-mail and any attachment",
    "this email and any attachment",
    "this message and any attachment",
    "confidentiality notice",
    "ce message et toutes les pièces jointes",
    "ce message et les pièces jointes",
    "ce courriel et ",
    "avis de confidentialité",
];

/// Whether `line` (followed by `next`) starts a quoted reply, a forward
/// header, a signature or a disclaimer.
fn quote_start(line: &str, next: &str) -> bool {
    let t = line.trim();
    let l = t.to_lowercase();
    let starts = |set: &[&str], s: &str| set.iter().any(|p| s.starts_with(p));
    if (t.starts_with("---") && SEPARATORS.contains(&l.trim_matches(|c| c == '-' || c == ' ')))
        || (t.len() >= 10 && t.chars().all(|c| c == '_'))
        || line == "-- "
        || t == "--"
        || starts(TAILS, &l)
        || (starts(HEADER_FROM, &l) && starts(HEADER_NEXT, &next.to_lowercase()))
    {
        return true;
    }
    // "On ... wrote:" / "Le ... a écrit :", possibly wrapped on two lines.
    let joined = format!("{l} {}", next.to_lowercase());
    t.len() < 300
        && ((l.starts_with("on ") && (l.ends_with("wrote:") || joined.ends_with("wrote:")))
            || (l.starts_with("le ") && [&l, &joined].iter().any(|s| s.ends_with("écrit :") || s.ends_with("écrit:"))))
}

/// Paragraph-based chunks of at most CHUNK_CHARS characters, consecutive
/// chunks sharing about CHUNK_OVERLAP characters. Each chunk is a slice of
/// `text`, returned with its char range in `text`.
pub fn chunk(text: &str) -> Vec<(String, Range<usize>)> {
    let off = |s: &str| s.as_ptr() as usize - text.as_ptr() as usize;
    // Byte spans of `text`, start and end both increasing.
    let (mut spans, mut cur) = (Vec::new(), None);
    for para in text.split("\n\n").map(str::trim).filter(|p| !p.is_empty()) {
        for piece in split_long(para, CHUNK_CHARS - CHUNK_OVERLAP - 2) {
            let (ps, pe) = (off(piece), off(piece) + piece.len());
            cur = Some(match cur {
                Some((s, e)) if len(&text[s..pe]) > CHUNK_CHARS => {
                    spans.push((s, e));
                    let t = tail(&text[s..e], CHUNK_OVERLAP);
                    let ts = if t.is_empty() { ps } else { off(t) };
                    // No overlap when a long blank gap would overflow the chunk.
                    if len(&text[ts..pe]) > CHUNK_CHARS { (ps, pe) } else { (ts, pe) }
                }
                Some((s, _)) => (s, pe),
                None => (ps, pe),
            });
        }
    }
    spans.extend(cur);
    let (mut b, mut c) = (0, 0);
    spans
        .into_iter()
        .map(|(s, e)| {
            c += len(&text[b..s]);
            b = s;
            let t = &text[s..e];
            (t.to_string(), c..c + len(t))
        })
        .collect()
}

fn len(s: &str) -> usize {
    s.chars().count()
}

/// Byte offset of char `n` (or the end).
fn char_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

/// The last `n` characters at most, starting at a word.
fn tail(s: &str, n: usize) -> &str {
    let t = &s[char_at(s, len(s).saturating_sub(n))..];
    t.split_once(char::is_whitespace).map_or(t, |(_, rest)| rest).trim_start()
}

/// Pieces of at most `max` characters, split at whitespace when possible.
fn split_long(mut p: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    while len(p) > max {
        let end = char_at(p, max);
        let at = p[..end].rfind(char::is_whitespace).filter(|&i| i > 0).unwrap_or(end);
        out.push(p[..at].trim_end());
        p = p[at..].trim_start();
    }
    if !p.is_empty() {
        out.push(p);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_english_reply() {
        let t = "Thanks, see the plan below.\n\nBest regards\n\n-----Original Message-----\nFrom: Zed Fict\nSent: Monday\nSubject: Plan\n\nOld text";
        assert_eq!(strip_quotes(t), "Thanks, see the plan below.\n\nBest regards");
        let t = "Fine by me.\n\nOn Mon, 3 Mar 2026 at 10:00, Zed Fict <zed@fict.invalid> wrote:\n> old\n> older";
        assert_eq!(strip_quotes(t), "Fine by me.");
        let t = "Yes.\n\nFrom: Zed Fict <zed@fict.invalid>\nSent: Monday, March 3, 2026 10:00\nTo: Ann\n\nold";
        assert_eq!(strip_quotes(t), "Yes.");
        let t = "Inline answer\n> quoted line\nmore answer\n\n-- \nZed Fict\nFict Corp";
        assert_eq!(strip_quotes(t), "Inline answer\nmore answer");
        let t = "Short note\n\nSent from my phone";
        assert_eq!(strip_quotes(t), "Short note");
    }

    #[test]
    fn strips_french_reply() {
        let t = "Bonjour,\nC'est noté.\n\nDe : Zed Fict\nEnvoyé : lundi 3 mars 2026 10:00\nÀ : Ann\nObjet : Plan\n\nAncien texte";
        assert_eq!(strip_quotes(t), "Bonjour,\nC'est noté.");
        let t = "D'accord.\n\nLe lun. 3 mars 2026 à 10:00, Zed Fict <zed@fict.invalid> a\nécrit :\n> ancien";
        assert_eq!(strip_quotes(t), "D'accord.");
        let t = "Merci.\n\n----- Message d'origine -----\nDe : Zed\nancien";
        assert_eq!(strip_quotes(t), "Merci.");
        let t = "Voir ci-joint.\n\nEnvoyé de mon iPhone";
        assert_eq!(strip_quotes(t), "Voir ci-joint.");
        let t = "Texte.\n\nCe message et toutes les pièces jointes sont confidentiels.";
        assert_eq!(strip_quotes(t), "Texte.");
    }

    #[test]
    fn bare_forward_keeps_text() {
        let t = "-----Original Message-----\nFrom: Zed\nSent: Monday\n\nForwarded body";
        assert_eq!(strip_quotes(t), t);
        assert_eq!(strip_quotes("From: here to there, a plain sentence."), "From: here to there, a plain sentence.");
    }

    #[test]
    fn chunk_boundaries() {
        assert!(chunk("").is_empty());
        assert_eq!(chunk("one paragraph"), [("one paragraph".to_string(), 0..13)]);
        let para = |i: usize| format!("Paragraph {i} {}", "lorem ipsum dolor sit amet ".repeat(20));
        let text: Vec<String> = (0..20).map(para).collect();
        let text = text.join("\n\n");
        let c: Vec<String> = chunk(&text).into_iter().map(|c| c.0).collect();
        assert!(c.len() > 4, "{}", c.len());
        for (i, ch) in c.iter().enumerate() {
            let n = ch.chars().count();
            assert!(n <= CHUNK_CHARS, "chunk {i} has {n} chars");
            assert!(i + 1 == c.len() || n > CHUNK_CHARS / 2, "chunk {i} too short: {n}");
        }
        for w in c.windows(2) {
            let tail: String = w[0].chars().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect();
            assert!(w[1].contains(tail.trim()), "consecutive chunks overlap");
            let shared = w[1].find("Paragraph").unwrap_or(0);
            assert!(shared <= CHUNK_OVERLAP + 2, "overlap bounded: {shared}");
        }
        for i in 0..20 {
            assert!(c.iter().any(|ch| ch.contains(&format!("Paragraph {i} "))), "paragraph {i} kept");
        }
        // One huge paragraph without blank lines, multibyte characters.
        let long = "é".repeat(5000);
        let c: Vec<String> = chunk(&long).into_iter().map(|c| c.0).collect();
        assert!(c.iter().all(|ch| ch.chars().count() <= CHUNK_CHARS));
        assert!(c.iter().map(|ch| ch.chars().count()).sum::<usize>() >= 5000);
    }

    #[test]
    fn chunk_ranges_match_body() {
        let para = |i: usize| format!("Paragraphe {i} café {}", "lorem ipsum dolor sit amet ".repeat(15 + i % 7));
        let mut body = (0..25).map(para).collect::<Vec<_>>().join("\n\n");
        // Extra blank line, then one huge multibyte paragraph.
        body.push_str(&format!("\n\n\n{}", "é".repeat(4000)));
        let chars: Vec<char> = body.chars().collect();
        let c = chunk(&body);
        assert!(c.len() > 5, "{}", c.len());
        assert_eq!(c[0].1.start, 0, "covers the start");
        assert_eq!(c.last().unwrap().1.end, chars.len(), "covers the end");
        for (t, r) in &c {
            assert_eq!(chars[r.clone()].iter().collect::<String>(), *t, "text is body[start..end] in chars");
            assert!(r.len() <= CHUNK_CHARS, "{r:?}");
        }
        for w in c.windows(2) {
            let (a, b) = (&w[0].1, &w[1].1);
            assert!(a.start < b.start && a.end < b.end, "monotonic: {a:?} {b:?}");
            assert!(b.start < a.end, "overlap, no gap: {a:?} {b:?}");
        }
        assert_eq!(chunk("  x  "), [("x".to_string(), 2..3)]);
    }
}
