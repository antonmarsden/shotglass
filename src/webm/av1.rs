//! Just enough AV1 bitstream reading for muxing: OBU framing, the sequence header
//! fields that go in Matroska's `av1C` (`CodecPrivate`), and temporal-delimiter
//! stripping (Matroska blocks hold temporal units without them).

use anyhow::{Result, bail};

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;

/// One OBU: its type and its whole bytes (header, size field and payload).
#[derive(Debug, Clone, Copy)]
pub struct Obu<'a> {
    pub kind: u8,
    pub bytes: &'a [u8],
    /// The payload, after the header and size field.
    pub payload: &'a [u8],
}

fn leb128(data: &[u8]) -> Result<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let Some(&b) = data.get(i) else {
            bail!("truncated leb128");
        };
        v |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    bail!("leb128 longer than 8 bytes")
}

/// Split a low-overhead bitstream (every OBU has `obu_has_size_field`) into OBUs.
pub fn obus(mut data: &[u8]) -> Result<Vec<Obu<'_>>> {
    let mut out = vec![];
    while !data.is_empty() {
        let h = data[0];
        if h & 0x80 != 0 {
            bail!("OBU forbidden bit set");
        }
        let kind = (h >> 3) & 0xf;
        let ext = h & 0x04 != 0;
        if h & 0x02 == 0 {
            bail!("OBU without a size field");
        }
        let head = 1 + ext as usize;
        if data.len() < head {
            bail!("truncated OBU header");
        }
        let (size, n) = leb128(&data[head..])?;
        let start = head + n;
        let end = start
            .checked_add(usize::try_from(size)?)
            .filter(|&e| e <= data.len());
        let Some(end) = end else {
            bail!("OBU runs past the end of the data");
        };
        out.push(Obu {
            kind,
            bytes: &data[..end],
            payload: &data[start..end],
        });
        data = &data[end..];
    }
    Ok(out)
}

/// A temporal unit as Matroska stores it: the same OBUs without temporal delimiters.
pub fn strip_temporal_delimiters(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    for o in obus(data)? {
        if o.kind != OBU_TEMPORAL_DELIMITER {
            out.extend_from_slice(o.bytes);
        }
    }
    Ok(out)
}

/// Split a whole `.obu` stream into temporal units (each starts at a temporal delimiter).
pub fn temporal_units(data: &[u8]) -> Result<Vec<&[u8]>> {
    let all = obus(data)?;
    let base = data.as_ptr() as usize;
    let mut starts: Vec<usize> = all
        .iter()
        .filter(|o| o.kind == OBU_TEMPORAL_DELIMITER)
        .map(|o| o.bytes.as_ptr() as usize - base)
        .collect();
    if starts.first() != Some(&0) {
        starts.insert(0, 0);
    }
    starts.push(data.len());
    Ok(starts.windows(2).map(|w| &data[w[0]..w[1]]).collect())
}

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32> {
        let Some(&b) = self.data.get(self.pos / 8) else {
            bail!("sequence header ends early");
        };
        let v = (b >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(v as u32)
    }
    fn f(&mut self, n: u32) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Ok(v)
    }
    fn uvlc(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Ok(u32::MAX);
            }
        }
        Ok(self.f(zeros)? + ((1u32 << zeros) - 1))
    }
}

/// The sequence header fields `av1C` records, plus the colour description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceHeader {
    pub profile: u8,
    pub level: u8,
    pub tier: u8,
    pub bit_depth: u8,
    pub monochrome: bool,
    pub subsampling: (u8, u8),
    pub chroma_sample_position: u8,
    /// (primaries, transfer, matrix) when the header has a colour description.
    pub color: Option<(u8, u8, u8)>,
    /// Full range (`color_range` = 1).
    pub full_range: bool,
    pub max_size: (u32, u32),
}

/// Parse a sequence header OBU's payload (AV1 spec 5.5).
pub fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeader> {
    let mut b = Bits {
        data: payload,
        pos: 0,
    };
    let profile = b.f(3)? as u8;
    let _still = b.f(1)?;
    let reduced = b.f(1)? == 1;
    let (level, tier);
    if reduced {
        level = b.f(5)? as u8;
        tier = 0;
    } else {
        let timing = b.f(1)? == 1;
        let mut decoder_model = false;
        let mut buffer_delay_len = 0;
        if timing {
            b.f(32)?;
            b.f(32)?;
            if b.f(1)? == 1 {
                b.uvlc()?;
            }
            decoder_model = b.f(1)? == 1;
            if decoder_model {
                buffer_delay_len = b.f(5)? + 1;
                b.f(32)?;
                b.f(5)?;
                b.f(5)?;
            }
        }
        let initial_display_delay = b.f(1)? == 1;
        let ops = b.f(5)? + 1;
        let (mut level0, mut tier0) = (0, 0);
        for i in 0..ops {
            b.f(12)?;
            let l = b.f(5)? as u8;
            let t = if l > 7 { b.f(1)? as u8 } else { 0 };
            if i == 0 {
                (level0, tier0) = (l, t);
            }
            if decoder_model && b.f(1)? == 1 {
                b.f(buffer_delay_len)?;
                b.f(buffer_delay_len)?;
                b.f(1)?;
            }
            if initial_display_delay && b.f(1)? == 1 {
                b.f(4)?;
            }
        }
        (level, tier) = (level0, tier0);
    }
    let wbits = b.f(4)? + 1;
    let hbits = b.f(4)? + 1;
    let max_w = b.f(wbits)? + 1;
    let max_h = b.f(hbits)? + 1;
    let frame_ids = !reduced && b.f(1)? == 1;
    if frame_ids {
        b.f(4)?;
        b.f(3)?;
    }
    b.f(3)?; // 128x128 superblocks, filter intra, intra edge filter
    if !reduced {
        b.f(4)?; // interintra, masked compound, warped motion, dual filter
        let order_hint = b.f(1)? == 1;
        if order_hint {
            b.f(2)?; // jnt comp, ref frame mvs
        }
        let force_sct = if b.f(1)? == 1 { 2 } else { b.f(1)? };
        if force_sct > 0 && b.f(1)? == 0 {
            b.f(1)?; // seq_force_integer_mv
        }
        if order_hint {
            b.f(3)?;
        }
    }
    b.f(3)?; // superres, cdef, restoration

    // color_config()
    let high = b.f(1)? == 1;
    let bit_depth = if profile == 2 && high {
        if b.f(1)? == 1 { 12 } else { 10 }
    } else if high {
        10
    } else {
        8
    };
    let monochrome = profile != 1 && b.f(1)? == 1;
    let color = if b.f(1)? == 1 {
        Some((b.f(8)? as u8, b.f(8)? as u8, b.f(8)? as u8))
    } else {
        None
    };
    let (full_range, subsampling, csp);
    if monochrome {
        full_range = b.f(1)? == 1;
        subsampling = (1, 1);
        csp = 0;
    } else if color == Some((1, 13, 0)) {
        full_range = true;
        subsampling = (0, 0);
        csp = 0;
    } else {
        full_range = b.f(1)? == 1;
        subsampling = match profile {
            0 => (1, 1),
            1 => (0, 0),
            _ if bit_depth == 12 => {
                let x = b.f(1)? as u8;
                (x, if x == 1 { b.f(1)? as u8 } else { 0 })
            }
            _ => (1, 0),
        };
        csp = if subsampling == (1, 1) {
            b.f(2)? as u8
        } else {
            0
        };
    }
    Ok(SequenceHeader {
        profile,
        level,
        tier,
        bit_depth,
        monochrome,
        subsampling,
        chroma_sample_position: csp,
        color,
        full_range,
        max_size: (max_w, max_h),
    })
}

/// The first sequence header in `data` (OBUs), parsed, with its whole OBU bytes.
pub fn find_sequence_header(data: &[u8]) -> Result<(SequenceHeader, Vec<u8>)> {
    for o in obus(data)? {
        if o.kind == OBU_SEQUENCE_HEADER {
            return Ok((parse_sequence_header(o.payload)?, o.bytes.to_vec()));
        }
    }
    bail!("no AV1 sequence header")
}

/// The `av1C` record (AV1-ISOBMFF 2.3): four bytes of sequence header fields, then
/// the sequence header OBU itself as the only config OBU.
pub fn av1c(seq: &SequenceHeader, seq_obu: &[u8]) -> Vec<u8> {
    let mut v = vec![
        0x81, // marker, version 1
        (seq.profile << 5) | (seq.level & 0x1f),
        (seq.tier << 7)
            | (u8::from(seq.bit_depth > 8) << 6)
            | (u8::from(seq.bit_depth == 12) << 5)
            | (u8::from(seq.monochrome) << 4)
            | (seq.subsampling.0 << 3)
            | (seq.subsampling.1 << 2)
            | (seq.chroma_sample_position & 3),
        0, // no initial presentation delay
    ];
    v.extend_from_slice(seq_obu);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/av1/testsrc-64x48-10bit.obu");

    #[test]
    fn splits_the_fixture_into_temporal_units() {
        let tus = temporal_units(FIXTURE).unwrap();
        assert_eq!(tus.len(), 6, "0.25 s at 24 fps");
        for tu in &tus {
            assert_eq!(obus(tu).unwrap()[0].kind, OBU_TEMPORAL_DELIMITER);
            let s = strip_temporal_delimiters(tu).unwrap();
            assert!(obus(&s).unwrap().iter().all(|o| o.kind != 2));
            assert_eq!(s.len(), tu.len() - 2, "a temporal delimiter is 2 bytes");
        }
        assert_eq!(tus.iter().map(|t| t.len()).sum::<usize>(), FIXTURE.len());
    }

    #[test]
    fn reads_the_sequence_header() {
        let (seq, obu) = find_sequence_header(FIXTURE).unwrap();
        assert_eq!(seq.profile, 0);
        assert_eq!(seq.bit_depth, 10);
        assert_eq!(seq.subsampling, (1, 1));
        assert_eq!(seq.max_size, (64, 48));
        assert!(!seq.monochrome && !seq.full_range);
        // What ffmpeg's Matroska muxer writes for the same stream.
        let c = av1c(&seq, &obu);
        assert_eq!(&c[..6], &[0x81, 0x00, 0x4c, 0x00, 0x0a, 0x0b]);
        assert_eq!(c.len(), 17);
    }

    #[test]
    fn rejects_garbage() {
        assert!(obus(&[0x80]).is_err());
        assert!(obus(&[0x12, 0x05, 0x00]).is_err());
        assert!(find_sequence_header(&[0x12, 0x00]).is_err());
        assert!(parse_sequence_header(&[0x00]).is_err());
    }
}
