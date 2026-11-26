//! SRT and WebVTT subtitle files (#71) → timed entries in film seconds.
//!
//! What's kept: each cue's start, end and text (its lines). What's dropped,
//! since looks are the shader's: styling tags (`<i>`, `<b>`, `<c.x>`, `<v Name>`,
//! `<ruby>`, karaoke timestamps `<00:01.000>`), SRT/ASS position overrides
//! (`{\an8}`), WebVTT cue settings, cue identifiers, `NOTE`, `STYLE` and
//! `REGION` blocks. Entities `&amp; &lt; &gt; &quot; &apos; &nbsp; &lrm; &rlm;`
//! are decoded. Times are exact milliseconds.

#![allow(dead_code)] // wired into `[texts]` in the next #71 step

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Milliseconds from the start of the film.
    pub start: u32,
    pub end: u32,
    /// Lines joined by `\n`, tags stripped.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Srt,
    Vtt,
}

impl Format {
    /// By file extension (`.srt`, `.vtt`, any case).
    pub fn of(path: &std::path::Path) -> Option<Format> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "srt" => Some(Format::Srt),
            "vtt" => Some(Format::Vtt),
            _ => None,
        }
    }
}

/// Parses a subtitle file. `label` names it in errors. Entries come back in
/// file order; they must have start < end and must not overlap (a subtitle
/// layer shows one entry at a time).
pub fn parse(src: &str, format: Format, label: &str) -> Result<Vec<Entry>> {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src);
    let src = src.replace("\r\n", "\n").replace('\r', "\n");
    let mut blocks = blocks(&src);
    if format == Format::Vtt {
        let Some((line, first)) = blocks.first() else {
            bail!("{label}: empty file; a WebVTT file starts with \"WEBVTT\"");
        };
        let head = first.first().copied().unwrap_or("");
        if !(head == "WEBVTT" || head.starts_with("WEBVTT ") || head.starts_with("WEBVTT\t")) {
            bail!("{label}:{line}: a WebVTT file starts with \"WEBVTT\"");
        }
        blocks.remove(0);
    }
    let mut out = vec![];
    for (line, block) in blocks {
        if format == Format::Vtt
            && block.first().is_some_and(|l| {
                ["NOTE", "STYLE", "REGION"].iter().any(|k| {
                    *l == *k || l.starts_with(&format!("{k} ")) || l.starts_with(&format!("{k}\t"))
                })
            })
        {
            continue;
        }
        // The timing line: the first line with "-->" (an identifier may precede it).
        let Some(ti) = block.iter().position(|l| l.contains("-->")) else {
            bail!("{label}:{line}: no timing line (\"00:00:01,000 --> 00:00:02,000\") in this cue");
        };
        if ti > 1 || (format == Format::Srt && ti == 1 && block[0].trim().parse::<u64>().is_err()) {
            bail!("{label}:{}: expected a timing line here", line + 1);
        }
        let tline = line + ti;
        let (a, rest) = block[ti].split_once("-->").unwrap();
        // WebVTT cue settings (and anything else) follow the end time.
        let b = rest.split_whitespace().next().unwrap_or("");
        let start = time(a.trim(), format).ok_or_else(|| {
            anyhow::anyhow!(
                "{label}:{tline}: can't read the start time \"{}\"",
                a.trim()
            )
        })?;
        let end = time(b, format)
            .ok_or_else(|| anyhow::anyhow!("{label}:{tline}: can't read the end time \"{b}\""))?;
        if end <= start {
            bail!("{label}:{tline}: the cue ends before it starts");
        }
        let text: Vec<String> = block[ti + 1..]
            .iter()
            .map(|l| clean(l))
            .filter(|l| !l.trim().is_empty())
            .collect();
        if text.is_empty() {
            bail!("{label}:{tline}: the cue has no text");
        }
        out.push(Entry {
            start,
            end,
            text: text.join("\n"),
        });
    }
    if out.is_empty() {
        bail!("{label}: no cues");
    }
    let mut by_time: Vec<&Entry> = out.iter().collect();
    by_time.sort_by_key(|e| (e.start, e.end));
    for w in by_time.windows(2) {
        if w[1].start < w[0].end {
            bail!(
                "{label}: cues overlap: \"{}\" ({}) and \"{}\" ({}); a subtitle layer shows one at a time",
                first_line(&w[0].text),
                stamp(w[0].start),
                first_line(&w[1].text),
                stamp(w[1].start)
            );
        }
    }
    Ok(out)
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// `h:mm:ss.mmm`, for messages.
pub fn stamp(ms: u32) -> String {
    let (h, m, s, f) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    format!("{h}:{m:02}:{s:02}.{f:03}")
}

/// Blank-line separated blocks, each with the 1-based line number it starts on.
fn blocks(src: &str) -> Vec<(usize, Vec<&str>)> {
    let mut out = vec![];
    let mut cur: Vec<&str> = vec![];
    let mut start = 0;
    for (i, l) in src.split('\n').enumerate() {
        if l.trim().is_empty() {
            if !cur.is_empty() {
                out.push((start, std::mem::take(&mut cur)));
            }
        } else {
            if cur.is_empty() {
                start = i + 1;
            }
            cur.push(l);
        }
    }
    if !cur.is_empty() {
        out.push((start, cur));
    }
    out
}

/// `hh:mm:ss,mmm` (SRT, also with `.`) or `[hh:]mm:ss.mmm` (WebVTT) → ms.
fn time(s: &str, format: Format) -> Option<u32> {
    let (hms, frac) = match format {
        Format::Srt => s.split_once([',', '.'])?,
        Format::Vtt => s.split_once('.')?,
    };
    if frac.len() != 3 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parts: Vec<&str> = hms.split(':').collect();
    let num = |p: &str, max: Option<u64>| -> Option<u64> {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u64 = p.parse().ok()?;
        max.is_none_or(|m| v < m).then_some(v)
    };
    let (h, m, sec) = match (format, parts.as_slice()) {
        (_, [h, m, s]) => (num(h, None)?, num(m, Some(60))?, num(s, Some(60))?),
        (Format::Vtt, [m, s]) => (0, num(m, Some(60))?, num(s, Some(60))?),
        _ => return None,
    };
    if parts.iter().skip(parts.len() - 2).any(|p| p.len() != 2) {
        return None;
    }
    let ms = ((h * 60 + m) * 60 + sec) * 1000 + frac.parse::<u64>().ok()?;
    u32::try_from(ms).ok()
}

/// A cue line without tags, overrides and entities.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            if let Some(end) = rest.find('>') {
                rest = &rest[end + 1..];
                continue;
            }
        } else if c == '{' && rest.starts_with("{\\") {
            if let Some(end) = rest.find('}') {
                rest = &rest[end + 1..];
                continue;
            }
        } else if c == '&' {
            let ents = [
                ("&amp;", "&"),
                ("&lt;", "<"),
                ("&gt;", ">"),
                ("&quot;", "\""),
                ("&apos;", "'"),
                ("&nbsp;", "\u{a0}"),
                ("&lrm;", "\u{200e}"),
                ("&rlm;", "\u{200f}"),
            ];
            if let Some((e, r)) = ents.iter().find(|(e, _)| rest.starts_with(e)) {
                out.push_str(r);
                rest = &rest[e.len()..];
                continue;
            }
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn srt(s: &str) -> Result<Vec<Entry>> {
        parse(s, Format::Srt, "subs.srt")
    }
    fn vtt(s: &str) -> Result<Vec<Entry>> {
        parse(s, Format::Vtt, "subs.vtt")
    }

    #[test]
    fn srt_cues_with_crlf_bom_and_tags() {
        let e = srt(
            "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>Hello</i>, {\\an8}world\r\n\r\n\
             2\r\n00:01:02,003 --> 01:00:00,000\r\nTom &amp; Jerry\r\n<b>two</b> lines\r\n",
        )
        .unwrap();
        assert_eq!(
            e,
            [
                Entry {
                    start: 1000,
                    end: 2500,
                    text: "Hello, world".into()
                },
                Entry {
                    start: 62_003,
                    end: 3_600_000,
                    text: "Tom & Jerry\ntwo lines".into()
                },
            ]
        );
    }

    #[test]
    fn vtt_header_identifiers_settings_and_blocks() {
        let e = vtt("WEBVTT - a title\n\nNOTE this is a comment\nover two lines\n\n\
                     STYLE\n::cue { color: red }\n\nintro\n00:01.000 --> 00:02.000 align:start line:0\n\
                     <v Anna>Hi <00:01.500>there</v>\n\n01:00:00.000 --> 01:00:01.250\n&lt;3 &rlm;\n")
            .unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(
            (e[0].start, e[0].end, e[0].text.as_str()),
            (1000, 2000, "Hi there")
        );
        assert_eq!(
            (e[1].start, e[1].end, e[1].text.as_str()),
            (3_600_000, 3_601_250, "<3 \u{200f}")
        );
    }

    #[test]
    fn bad_files_are_errors_with_a_line() {
        let err = |r: Result<Vec<Entry>>| r.err().unwrap().to_string();
        assert!(err(vtt("1\n00:01.000 --> 00:02.000\nx\n")).contains("WEBVTT"));
        assert!(
            err(srt("1\n00:00:02,000 --> 00:00:01,000\nx\n"))
                .contains("subs.srt:2: the cue ends before")
        );
        assert!(err(srt("1\n00:00:01,000 --> 00:00:02,000\n")).contains("no text"));
        assert!(err(srt("1\n00:00:01 --> 00:00:02,000\nx\n")).contains("start time"));
        assert!(err(srt("1\n00:61:01,000 --> 00:62:02,000\nx\n")).contains("start time"));
        assert!(err(srt("1\nhello\n")).contains("no timing line"));
        assert!(err(srt("")).contains("no cues"));
        // SRT needs hours; WebVTT may leave them out.
        assert!(srt("1\n00:01,000 --> 00:02,000\nx\n").is_err());
        assert!(vtt("WEBVTT\n\n00:01.000 --> 00:02.000\nx\n").is_ok());
    }

    #[test]
    fn overlapping_cues_are_refused() {
        let err = srt(
            "1\n00:00:01,000 --> 00:00:03,000\nfirst\n\n2\n00:00:02,000 --> 00:00:04,000\nsecond\n",
        )
        .err()
        .unwrap()
        .to_string();
        assert!(
            err.contains("overlap") && err.contains("first") && err.contains("second"),
            "{err}"
        );
        // Touching is fine.
        assert!(
            srt("1\n00:00:01,000 --> 00:00:02,000\na\n\n2\n00:00:02,000 --> 00:00:03,000\nb\n")
                .is_ok()
        );
    }

    #[test]
    fn format_by_extension() {
        assert_eq!(Format::of("a/b.SRT".as_ref()), Some(Format::Srt));
        assert_eq!(Format::of("b.vtt".as_ref()), Some(Format::Vtt));
        assert_eq!(Format::of("b.txt".as_ref()), None);
    }

    #[test]
    fn stamps() {
        assert_eq!(stamp(3_723_004), "1:02:03.004");
    }
}
