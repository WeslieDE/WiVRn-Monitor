//! Minimal fragmented MP4 (ISO BMFF) writer for one H.264 video track at a constant frame rate.
//!
//! Layout: `ftyp`, `moov` (empty sample tables + `mvex`), then one `moof`+`mdat` pair per fragment (~1 s).
//! Every fragment is flushed to the OS as soon as it is complete, so after a crash, kill or power loss everything
//! up to the last full fragment stays playable. `finish` rewrites the `moov` in place with the real duration
//! (same byte length), which makes the file report its length in players that do not scan all fragments.
//!
//! Samples are AVCC (4-byte length prefixed NAL units). There are no B-frames, so decode time = presentation time.

use std::io::{self, Seek, SeekFrom, Write};

/// Constant output frame rate.
pub const FPS: u32 = 30;
/// Media timescale: 1000 ticks per frame.
const TIMESCALE: u32 = FPS * 1000;
const TICKS_PER_FRAME: u32 = 1000;
/// Frames per fragment (1 s). Keyframes (every 60 frames) therefore always start a fragment.
pub const FRAGMENT_FRAMES: usize = FPS as usize;

fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&(len as u32).to_be_bytes());
    v.extend_from_slice(kind);
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

fn u32b(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}
fn u16b(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

fn matrix() -> Vec<u8> {
    MATRIX.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// Splits an Annex B byte stream into NAL units (without start codes).
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new(); // (start code position, payload position)
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (n, &(_, payload)) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |s| s.0);
        // The zero before `00 00 01` of a 4-byte start code (and any padding) is not part of the NAL unit.
        while end > payload && data[end - 1] == 0 {
            end -= 1;
        }
        if end > payload {
            out.push(&data[payload..end]);
        }
    }
    out
}

/// One encoded picture, converted from Annex B to an AVCC sample.
#[derive(Default, Debug, PartialEq)]
pub struct AccessUnit {
    pub sample: Vec<u8>,
    pub key: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

/// SPS/PPS go into the `avcC` box instead of the samples; all other NAL units are length-prefixed.
pub fn annexb_to_avcc(data: &[u8]) -> AccessUnit {
    let mut au = AccessUnit::default();
    for nal in split_annexb(data) {
        match nal[0] & 0x1F {
            7 => au.sps = Some(nal.to_vec()),
            8 => au.pps = Some(nal.to_vec()),
            t => {
                if t == 5 {
                    au.key = true;
                }
                au.sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                au.sample.extend_from_slice(nal);
            }
        }
    }
    au
}

fn avc1(width: u16, height: u16, sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let avcc = bx(
        b"avcC",
        &[
            &[1, sps[1], sps[2], sps[3], 0xFF, 0xE1],
            &u16b(sps.len() as u16),
            sps,
            &[1],
            &u16b(pps.len() as u16),
            pps,
        ],
    );
    // colr/nclx: BT.709 primaries, transfer and matrix; limited range.
    let colr = bx(b"colr", &[b"nclx", &u16b(1), &u16b(1), &u16b(1), &[0]]);
    let mut name = [0u8; 32];
    name[0] = 4;
    name[1..5].copy_from_slice(b"H264");
    bx(
        b"avc1",
        &[
            &[0; 6],
            &u16b(1),
            &[0; 16],
            &u16b(width),
            &u16b(height),
            &u32b(0x0048_0000),
            &u32b(0x0048_0000),
            &[0; 4],
            &u16b(1),
            &name,
            &u16b(0x0018),
            &u16b(0xFFFF),
            &avcc,
            &colr,
        ],
    )
}

/// `ftyp` + `moov` for the header. `duration_ms` is 0 while recording and patched in `finish`.
fn init_segment(width: u16, height: u16, sps: &[u8], pps: &[u8], duration_ms: u32) -> (Vec<u8>, Vec<u8>) {
    let ftyp = bx(b"ftyp", &[b"isom", &u32b(0x200), b"isom", b"iso6", b"avc1", b"mp41"]);
    let mvhd = bx(
        b"mvhd",
        &[&[0; 4], &u32b(0), &u32b(0), &u32b(1000), &u32b(duration_ms), &u32b(0x0001_0000), &u16b(0x0100), &[0; 10], &matrix(), &[0; 24], &u32b(2)],
    );
    let tkhd = bx(
        b"tkhd",
        &[
            &u32b(3),
            &u32b(0),
            &u32b(0),
            &u32b(1),
            &[0; 4],
            &u32b(duration_ms),
            &[0; 8],
            &u16b(0),
            &u16b(0),
            &u16b(0),
            &[0; 2],
            &matrix(),
            &u32b((width as u32) << 16),
            &u32b((height as u32) << 16),
        ],
    );
    let media_duration = (duration_ms as u64 * TIMESCALE as u64 / 1000) as u32;
    let mdhd = bx(b"mdhd", &[&[0; 4], &u32b(0), &u32b(0), &u32b(TIMESCALE), &u32b(media_duration), &u16b(0x55C4), &u16b(0)]);
    let hdlr = bx(b"hdlr", &[&[0; 4], &[0; 4], b"vide", &[0; 12], b"VideoHandler\0"]);
    let vmhd = bx(b"vmhd", &[&u32b(1), &[0; 8]]);
    let dinf = bx(b"dinf", &[&bx(b"dref", &[&[0; 4], &u32b(1), &bx(b"url ", &[&u32b(1)])])]);
    let empty = |k: &[u8; 4], extra: &[u8]| bx(k, &[&[0; 4], extra, &u32b(0)]);
    let stsd = bx(b"stsd", &[&[0; 4], &u32b(1), &avc1(width, height, sps, pps)]);
    let stbl = bx(b"stbl", &[&stsd, &empty(b"stts", &[]), &empty(b"stsc", &[]), &empty(b"stsz", &u32b(0)), &empty(b"stco", &[])]);
    let minf = bx(b"minf", &[&vmhd, &dinf, &stbl]);
    let mdia = bx(b"mdia", &[&mdhd, &hdlr, &minf]);
    let trak = bx(b"trak", &[&tkhd, &mdia]);
    let mvex = bx(b"mvex", &[&bx(b"mehd", &[&[0; 4], &u32b(duration_ms)]), &bx(b"trex", &[&[0; 4], &u32b(1), &u32b(1), &u32b(0), &u32b(0), &u32b(0)])]);
    (ftyp, bx(b"moov", &[&mvhd, &trak, &mvex]))
}

struct Sample {
    data: Vec<u8>,
    key: bool,
    /// Duration in frames (more than 1 if the encoder skipped the following pictures).
    frames: u32,
}

pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    moov_pos: u64,
    dims: (u16, u16),
    sps: Vec<u8>,
    pps: Vec<u8>,
    seq: u32,
    pending: Vec<Sample>,
    /// Frame slot of the first pending sample.
    pending_first: u64,
    /// Number of frame slots written or pending (= last slot + 1).
    frames: u64,
    bytes: u64,
}

impl<W: Write + Seek> Mp4Writer<W> {
    pub fn new(mut out: W, width: u32, height: u32, sps: &[u8], pps: &[u8]) -> io::Result<Self> {
        if sps.len() < 4 || pps.is_empty() || width > u16::MAX as u32 || height > u16::MAX as u32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid H.264 parameter sets or size"));
        }
        let (ftyp, moov) = init_segment(width as u16, height as u16, sps, pps, 0);
        out.write_all(&ftyp)?;
        let moov_pos = ftyp.len() as u64;
        out.write_all(&moov)?;
        out.flush()?;
        Ok(Mp4Writer {
            out,
            moov_pos,
            dims: (width as u16, height as u16),
            sps: sps.to_vec(),
            pps: pps.to_vec(),
            seq: 0,
            pending: Vec::new(),
            pending_first: 0,
            frames: 0,
            bytes: (ftyp.len() + moov.len()) as u64,
        })
    }

    /// Appends the sample for frame slot `slot` (slots increase). A gap before it (pictures the encoder skipped)
    /// extends the duration of the previous sample.
    pub fn push(&mut self, slot: u64, data: Vec<u8>, key: bool) -> io::Result<()> {
        if let (Some(last), true) = (self.pending.last_mut(), slot > self.frames) {
            last.frames += (slot - self.frames) as u32;
        }
        if self.pending.len() >= FRAGMENT_FRAMES {
            self.flush_fragment()?;
        }
        if self.pending.is_empty() {
            self.pending_first = slot;
        }
        self.pending.push(Sample { data, key, frames: 1 });
        self.frames = slot + 1;
        Ok(())
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    fn flush_fragment(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        self.seq += 1;
        let moof = self.moof(0);
        let moof = self.moof(moof.len() as u32 + 8);
        let payload: usize = self.pending.iter().map(|s| s.data.len()).sum();
        self.out.write_all(&moof)?;
        self.out.write_all(&u32b(8 + payload as u32))?;
        self.out.write_all(b"mdat")?;
        for s in &self.pending {
            self.out.write_all(&s.data)?;
        }
        self.out.flush()?;
        self.bytes += (moof.len() + 8 + payload) as u64;
        self.pending.clear();
        Ok(())
    }

    fn moof(&self, data_offset: u32) -> Vec<u8> {
        let mfhd = bx(b"mfhd", &[&[0; 4], &u32b(self.seq)]);
        let tfhd = bx(b"tfhd", &[&u32b(0x0002_0000), &u32b(1)]); // default-base-is-moof
        let tfdt = bx(b"tfdt", &[&u32b(0x0100_0000), &(self.pending_first * TICKS_PER_FRAME as u64).to_be_bytes()]);
        let mut entries = Vec::with_capacity(self.pending.len() * 12);
        for s in &self.pending {
            entries.extend_from_slice(&u32b(TICKS_PER_FRAME * s.frames));
            entries.extend_from_slice(&u32b(s.data.len() as u32));
            // sample_depends_on = 2 (I picture) / depends on others + non-sync flag
            entries.extend_from_slice(&u32b(if s.key { 0x0200_0000 } else { 0x0101_0000 }));
        }
        let trun = bx(b"trun", &[&u32b(0x0000_0701), &u32b(self.pending.len() as u32), &data_offset.to_be_bytes(), &entries]);
        bx(b"moof", &[&mfhd, &bx(b"traf", &[&tfhd, &tfdt, &trun])])
    }

    /// Writes the last fragment, stores the real duration in the header and returns the writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_fragment()?;
        let ms = (self.frames * 1000 / FPS as u64).min(u32::MAX as u64) as u32;
        let (_, moov) = init_segment(self.dims.0, self.dims.1, &self.sps, &self.pps, ms);
        self.out.seek(SeekFrom::Start(self.moov_pos))?;
        self.out.write_all(&moov)?;
        self.out.seek(SeekFrom::End(0))?;
        self.out.flush()?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Walks the top-level boxes; panics if sizes do not add up.
    fn top_level(data: &[u8]) -> Vec<(String, usize, usize)> {
        let (mut pos, mut out) = (0, vec![]);
        while pos < data.len() {
            let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            assert!(size >= 8 && pos + size <= data.len(), "bad box size at {pos}");
            out.push((String::from_utf8_lossy(&data[pos + 4..pos + 8]).into_owned(), pos, size));
            pos += size;
        }
        out
    }

    #[test]
    fn annexb_split_and_convert() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 0, 1, 0x68, 9, 0, 0, 1, 0x65, 0xAA, 0xBB, 0, 0, 0, 1, 0x41, 0xCC];
        let nals = split_annexb(&stream);
        assert_eq!(nals, vec![&[0x67, 1, 2, 3][..], &[0x68, 9], &[0x65, 0xAA, 0xBB], &[0x41, 0xCC]]);
        let au = annexb_to_avcc(&stream);
        assert!(au.key);
        assert_eq!(au.sps.as_deref(), Some(&[0x67, 1, 2, 3][..]));
        assert_eq!(au.pps.as_deref(), Some(&[0x68, 9][..]));
        assert_eq!(au.sample, [0, 0, 0, 3, 0x65, 0xAA, 0xBB, 0, 0, 0, 2, 0x41, 0xCC]);
        assert!(!annexb_to_avcc(&[0, 0, 0, 1, 0x41, 1]).key);
    }

    #[test]
    fn structure_and_duration_patch() {
        let sps = [0x67, 0x42, 0xC0, 0x1F, 0xAA];
        let pps = [0x68, 0xCE];
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), 1280, 720, &sps, &pps).unwrap();
        for n in 0..70u64 {
            w.push(n, vec![0, 0, 0, 2, 0x41, n as u8], n % 60 == 0).unwrap();
        }
        let data = w.finish().unwrap().into_inner();
        let boxes = top_level(&data);
        let kinds: Vec<&str> = boxes.iter().map(|b| b.0.as_str()).collect();
        // 70 frames = fragments of 30, 30, 10.
        assert_eq!(kinds, ["ftyp", "moov", "moof", "mdat", "moof", "mdat", "moof", "mdat"]);
        let moov = &data[boxes[1].1..boxes[1].1 + boxes[1].2];
        let pos = moov.windows(4).position(|w| w == b"mvhd").unwrap();
        let dur = u32::from_be_bytes(moov[pos + 20..pos + 24].try_into().unwrap());
        assert_eq!(dur, 70 * 1000 / 30); // ms
        // First fragment: data_offset in trun points at the mdat payload.
        let (moof_pos, moof_len) = (boxes[2].1, boxes[2].2);
        let trun = data[moof_pos..moof_pos + moof_len].windows(4).position(|w| w == b"trun").unwrap() + moof_pos;
        let off = u32::from_be_bytes(data[trun + 12..trun + 16].try_into().unwrap()) as usize;
        assert_eq!(off, moof_len + 8);
        assert_eq!(&data[moof_pos + off..moof_pos + off + 6], &[0, 0, 0, 2, 0x41, 0]);
    }

    #[test]
    fn skipped_pictures_extend_the_previous_sample() {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), 16, 16, &[0x67, 0x42, 0xC0, 0x1F], &[0x68, 0xCE]).unwrap();
        w.push(0, vec![0, 0, 0, 1, 0x65], true).unwrap();
        w.push(3, vec![0, 0, 0, 1, 0x41], false).unwrap(); // slots 1 and 2 skipped
        let data = w.finish().unwrap().into_inner();
        let trun = data.windows(4).position(|w| w == b"trun").unwrap();
        let first_dur = u32::from_be_bytes(data[trun + 16..trun + 20].try_into().unwrap());
        assert_eq!(first_dur, 3 * TICKS_PER_FRAME);
    }

    #[test]
    fn rejects_bad_parameter_sets() {
        assert!(Mp4Writer::new(Cursor::new(Vec::new()), 16, 16, &[0x67], &[0x68], ).is_err());
    }
}
