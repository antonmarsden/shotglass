//! SRT and WebVTT subtitle files (#71) → timed entries in film seconds.
//!
//! What's kept: each cue's start, end and text (its lines). What's dropped,
//! since looks are the shader's: styling tags (`<i>`, `<b>`, `<c.x>`, `<v Name>`,
//! `<ruby>`, karaoke timestamps `<00:01.000>`), SRT/ASS position overrides
//! (`{\an8}`), WebVTT cue settings, cue identifiers, `NOTE`, `STYLE` and
//! `REGION` blocks. Entities `&amp; &lt; &gt; &quot; &apos; &nbsp; &lrm; &rlm;`
//! are decoded. Times are exact milliseconds.

use anyhow::{Result, bail};
use std::collections::BTreeSet;

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

/// What parsing left out that a reader might expect to see: styling a glyph run
/// can't carry (it draws plain text; looks are the shader's). `shotglass check`
/// reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dropped {
    /// Cues that lost a tag, an override or cue settings.
    pub cues: usize,
    /// Tag names as written (`i`, `v`, `c`, `font`; `timestamp` for karaoke times).
    pub tags: BTreeSet<String>,
    /// `{\an8}`-style overrides.
    pub overrides: bool,
    /// WebVTT cue settings (`align:start line:0`).
    pub settings: bool,
    /// WebVTT `STYLE` / `REGION` blocks (`NOTE` is a comment, not counted).
    pub blocks: BTreeSet<&'static str>,
}

impl Dropped {
    pub fn is_empty(&self) -> bool {
        *self == Dropped::default()
    }

    /// One line for `check`, None when nothing was dropped.
    pub fn summary(&self, total: usize) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut what = vec![];
        if !self.tags.is_empty() {
            let tags: Vec<String> = self
                .tags
                .iter()
                .map(|t| {
                    if t == "timestamp" {
                        "karaoke timestamps".to_string()
                    } else {
                        format!("<{t}>")
                    }
                })
                .collect();
            what.push(format!("tags {}", tags.join(" ")));
        }
        if self.overrides {
            what.push("{\\…} overrides".to_string());
        }
        if self.settings {
            what.push("cue settings".to_string());
        }
        let mut s = String::new();
        if !what.is_empty() {
            s = format!(
                "{} dropped in {} of {total} cues",
                what.join(", "),
                self.cues
            );
        }
        if !self.blocks.is_empty() {
            let b: Vec<&str> = self.blocks.iter().copied().collect();
            if !s.is_empty() {
                s += "; ";
            }
            s += &format!("{} blocks skipped", b.join("/"));
        }
        Some(format!(
            "{s}: a glyph run draws plain text (style it in the shader)"
        ))
    }
}

/// Parses a subtitle file. `label` names it in errors. Entries come back in
/// file order; they must have start < end and must not overlap (a subtitle
/// layer shows one entry at a time).
#[cfg(test)]
pub fn parse(src: &str, format: Format, label: &str) -> Result<Vec<Entry>> {
    parse_reporting(src, format, label).map(|(e, _)| e)
}

/// `parse`, also saying what styling it dropped.
pub fn parse_reporting(src: &str, format: Format, label: &str) -> Result<(Vec<Entry>, Dropped)> {
    let mut dropped = Dropped::default();
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
            && let Some(k) = block.first().and_then(|l| {
                ["NOTE", "STYLE", "REGION"].into_iter().find(|k| {
                    *l == *k || l.starts_with(&format!("{k} ")) || l.starts_with(&format!("{k}\t"))
                })
            })
        {
            if k != "NOTE" {
                dropped.blocks.insert(k);
            }
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
        let mut after = rest.split_whitespace();
        let b = after.next().unwrap_or("");
        let settings = format == Format::Vtt && after.next().is_some();
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
        let mut cue = Dropped {
            settings,
            ..Dropped::default()
        };
        let text: Vec<String> = block[ti + 1..]
            .iter()
            .map(|l| clean(l, &mut cue))
            .filter(|l| !l.trim().is_empty())
            .collect();
        if !cue.is_empty() {
            dropped.cues += 1;
            dropped.tags.append(&mut cue.tags);
            dropped.overrides |= cue.overrides;
            dropped.settings |= cue.settings;
        }
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
    Ok((out, dropped))
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

/// A cue line without tags, overrides and entities; what it dropped goes in `dropped`.
fn clean(line: &str, dropped: &mut Dropped) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(c) = rest.chars().next() {
        // A tag starts with a letter, `/` or a digit (a karaoke timestamp); a bare
        // '<' in plain SRT text ("x < 5") is text.
        let tag_start = rest[1..]
            .chars()
            .next()
            .is_some_and(|n| n.is_ascii_alphanumeric() || n == '/');
        if c == '<' && tag_start {
            if let Some(end) = rest.find('>') {
                let tag = rest[1..end].trim_start_matches('/');
                let name: String = tag
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                dropped
                    .tags
                    .insert(if name.starts_with(|c: char| c.is_ascii_digit()) {
                        "timestamp".to_string()
                    } else {
                        name.to_ascii_lowercase()
                    });
                rest = &rest[end + 1..];
                continue;
            }
        } else if c == '{' && rest.starts_with("{\\") {
            if let Some(end) = rest.find('}') {
                dropped.overrides = true;
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
    fn a_bare_less_than_is_text() {
        let e = srt("1\n00:00:01,000 --> 00:00:02,000\nx < 5 and y > 3, <i>z</i>\n").unwrap();
        assert_eq!(e[0].text, "x < 5 and y > 3, z");
    }

    #[test]
    fn escaped_markup_is_text() {
        // Entities decode in the same pass that strips tags: a decoded '<' is never
        // read as the start of one.
        let e = srt("1\n00:00:01,000 --> 00:00:02,000\n&lt;i&gt;not italic&lt;/i&gt; &amp;lt;\n")
            .unwrap();
        assert_eq!(e[0].text, "<i>not italic</i> &lt;");
    }

    #[test]
    fn dropped_styling_is_reported() {
        let (_, d) = parse_reporting(
            "WEBVTT\n\nSTYLE\n::cue { color: red }\n\nNOTE hi\n\n\
             00:01.000 --> 00:02.000 align:start\n<v Anna>Hi <00:01.500>there</v>\n\n\
             00:03.000 --> 00:04.000\nplain\n\n00:05.000 --> 00:06.000\n<I>loud</I>\n",
            Format::Vtt,
            "subs.vtt",
        )
        .unwrap();
        assert_eq!(d.cues, 2);
        assert_eq!(
            d.tags.iter().map(String::as_str).collect::<Vec<_>>(),
            ["i", "timestamp", "v"]
        );
        assert!(d.settings && !d.overrides);
        assert_eq!(d.blocks.iter().copied().collect::<Vec<_>>(), ["STYLE"]);
        assert_eq!(
            d.summary(3).unwrap(),
            "tags <i> karaoke timestamps <v>, cue settings dropped in 2 of 3 cues; STYLE blocks \
             skipped: a glyph run draws plain text (style it in the shader)"
        );
        let (_, d) = parse_reporting(
            "1\n00:00:01,000 --> 00:00:02,000\n{\\an8}top & x < 5\n",
            Format::Srt,
            "subs.srt",
        )
        .unwrap();
        assert_eq!(
            d.summary(1).unwrap(),
            "{\\…} overrides dropped in 1 of 1 cues: a glyph run draws plain text (style it in the shader)"
        );
        // Plain text, entities and comments drop nothing.
        let (_, d) = parse_reporting(
            "WEBVTT\n\nNOTE x\n\n00:01.000 --> 00:02.000\n&lt;b&gt; a < b\n",
            Format::Vtt,
            "subs.vtt",
        )
        .unwrap();
        assert!(d.is_empty() && d.summary(1).is_none(), "{d:?}");
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
