//! Minimal fragmented MP4 (ISO BMFF) writer for one H.264 video track at a constant frame rate and an optional
//! AAC-LC audio track (track 2, 48 kHz timescale, 1024 ticks per access unit).
//!
//! Layout: `ftyp`, `moov` (empty sample tables + `mvex`), then one `moof`+`mdat` pair per fragment (~1 s; the `moof`
//! holds one `traf` per track that has samples in the fragment, the `mdat` holds the video bytes and then the audio bytes).
//! Every fragment is flushed to the OS as soon as it is complete, so after a crash, kill or power loss everything
//! up to the last full fragment stays playable. `finish` rewrites the `moov` in place with the real duration
//! (same byte length), which makes the file report its length in players that do not scan all fragments.
//!
//! Samples are AVCC (4-byte length prefixed NAL units). There are no B-frames, so decode time = presentation time.
//! Audio: the first audio access unit starts at time 0 like the first video frame; an edit list (`elst`, media time =
//! encoder priming) makes players skip the encoder delay so both tracks start in sync.

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


/// Audio sample rate (also the audio media timescale).
pub const AUDIO_RATE: u32 = 48_000;
/// PCM frames per AAC access unit.
pub const AU_FRAMES: u32 = 1024;
/// Access units per fragment (about 1 s) at which audio alone triggers a fragment.
pub const AU_PER_FRAGMENT: usize = 47;

/// Parameters of the audio track (AAC-LC, stereo, 48 kHz).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    /// AudioSpecificConfig bytes for the `esds` box.
    pub asc: Vec<u8>,
    /// Encoder delay in samples (media time of the edit list).
    pub priming: u32,
    /// Nominal bitrate in bits per second.
    pub bitrate: u32,
}

/// Presentation time in milliseconds of access unit `k` (counter -> time; the priming is removed by the edit list).
pub fn au_pts_ms(k: u64) -> f64 {
    k as f64 * AU_FRAMES as f64 * 1000.0 / AUDIO_RATE as f64
}

fn desc(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    debug_assert!(len < 128);
    let mut v = vec![tag, len as u8];
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

fn mp4a(a: &AudioTrack) -> Vec<u8> {
    let dec_specific = desc(0x05, &[&a.asc]);
    let mut dcd = vec![0x40, 0x15, 0, 0, 0]; // AAC, audio stream, buffer size
    dcd.extend_from_slice(&u32b(a.bitrate * 3 / 2));
    dcd.extend_from_slice(&u32b(a.bitrate));
    let dcd = desc(0x04, &[&dcd, &dec_specific]);
    let es = desc(0x03, &[&u16b(2), &[0], &dcd, &desc(0x06, &[&[2]])]);
    let esds = bx(b"esds", &[&[0; 4], &es]);
    bx(
        b"mp4a",
        &[&[0; 6], &u16b(1), &[0; 8], &u16b(2), &u16b(16), &[0; 4], &u32b(AUDIO_RATE << 16), &esds],
    )
}

/// Durations for the header: `video_ms` and `audio_ms` are 0 while recording and patched in `finish`.
#[derive(Clone, Copy, Default)]
struct Durations {
    video_ms: u32,
    audio_ms: u32,
}

fn tkhd(track_id: u32, duration_ms: u32, volume: u16, width: u32, height: u32) -> Vec<u8> {
    bx(
        b"tkhd",
        &[
            &u32b(3),
            &u32b(0),
            &u32b(0),
            &u32b(track_id),
            &[0; 4],
            &u32b(duration_ms),
            &[0; 8],
            &u16b(0),
            &u16b(0),
            &u16b(volume),
            &[0; 2],
            &matrix(),
            &u32b(width << 16),
            &u32b(height << 16),
        ],
    )
}

fn empty_tables(sample_entry: &[u8]) -> Vec<u8> {
    let empty = |k: &[u8; 4], extra: &[u8]| bx(k, &[&[0; 4], extra, &u32b(0)]);
    let stsd = bx(b"stsd", &[&[0; 4], &u32b(1), sample_entry]);
    bx(b"stbl", &[&stsd, &empty(b"stts", &[]), &empty(b"stsc", &[]), &empty(b"stsz", &u32b(0)), &empty(b"stco", &[])])
}

fn dinf() -> Vec<u8> {
    bx(b"dinf", &[&bx(b"dref", &[&[0; 4], &u32b(1), &bx(b"url ", &[&u32b(1)])])])
}

/// `ftyp` + `moov` for the header.
fn init_segment(width: u16, height: u16, sps: &[u8], pps: &[u8], audio: Option<&AudioTrack>, d: Durations) -> (Vec<u8>, Vec<u8>) {
    let ftyp = bx(b"ftyp", &[b"isom", &u32b(0x200), b"isom", b"iso6", b"avc1", b"mp41"]);
    let movie_ms = d.video_ms.max(d.audio_ms);
    let next_track = if audio.is_some() { 3 } else { 2 };
    let mvhd = bx(
        b"mvhd",
        &[&[0; 4], &u32b(0), &u32b(0), &u32b(1000), &u32b(movie_ms), &u32b(0x0001_0000), &u16b(0x0100), &[0; 10], &matrix(), &[0; 24], &u32b(next_track)],
    );
    let media_duration = (d.video_ms as u64 * TIMESCALE as u64 / 1000) as u32;
    let mdhd = bx(b"mdhd", &[&[0; 4], &u32b(0), &u32b(0), &u32b(TIMESCALE), &u32b(media_duration), &u16b(0x55C4), &u16b(0)]);
    let hdlr = bx(b"hdlr", &[&[0; 4], &[0; 4], b"vide", &[0; 12], b"VideoHandler\0"]);
    let vmhd = bx(b"vmhd", &[&u32b(1), &[0; 8]]);
    let minf = bx(b"minf", &[&vmhd, &dinf(), &empty_tables(&avc1(width, height, sps, pps))]);
    let mdia = bx(b"mdia", &[&mdhd, &hdlr, &minf]);
    let trak = bx(b"trak", &[&tkhd(1, d.video_ms, 0, width as u32, height as u32), &mdia]);
    let trex = |id: u32, dur: u32| bx(b"trex", &[&[0; 4], &u32b(id), &u32b(1), &u32b(dur), &u32b(0), &u32b(0)]);
    let mut mvex_parts = vec![bx(b"mehd", &[&[0; 4], &u32b(movie_ms)]), trex(1, 0)];
    let mut traks = vec![trak];
    if let Some(a) = audio {
        let a_dur = (d.audio_ms as u64 * AUDIO_RATE as u64 / 1000) as u32;
        let mdhd = bx(b"mdhd", &[&[0; 4], &u32b(0), &u32b(0), &u32b(AUDIO_RATE), &u32b(a_dur + a.priming), &u16b(0x55C4), &u16b(0)]);
        let hdlr = bx(b"hdlr", &[&[0; 4], &[0; 4], b"soun", &[0; 12], b"SoundHandler\0"]);
        let smhd = bx(b"smhd", &[&[0; 4], &[0; 4]]);
        let minf = bx(b"minf", &[&smhd, &dinf(), &empty_tables(&mp4a(a))]);
        let mdia = bx(b"mdia", &[&mdhd, &hdlr, &minf]);
        // Edit list: skip the encoder delay (media_time = priming) so that audio sample 0 plays at time 0.
        let elst = bx(b"elst", &[&[0; 4], &u32b(1), &u32b(d.audio_ms), &a.priming.to_be_bytes(), &u32b(0x0001_0000)]);
        traks.push(bx(b"trak", &[&tkhd(2, d.audio_ms, 0x0100, 0, 0), &bx(b"edts", &[&elst]), &mdia]));
        mvex_parts.push(trex(2, AU_FRAMES));
    }
    let mvex = bx(b"mvex", &mvex_parts.iter().map(|v| v.as_slice()).collect::<Vec<_>>());
    let mut parts: Vec<&[u8]> = vec![&mvhd];
    parts.extend(traks.iter().map(|v| v.as_slice()));
    parts.push(&mvex);
    (ftyp, bx(b"moov", &parts))
}

struct Sample {
    data: Vec<u8>,
    key: bool,
    /// Duration in frames (more than 1 if the encoder skipped the following pictures).
    frames: u32,
}

struct AudioState {
    track: AudioTrack,
    pending: Vec<Vec<u8>>,
    /// Index of the first pending access unit.
    pending_first: u64,
    /// Access units written or pending.
    count: u64,
    /// Real number of PCM frames (without encoder padding), set before `finish`; 0 = unknown.
    pcm_frames: u64,
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
    audio: Option<AudioState>,
}

impl<W: Write + Seek> Mp4Writer<W> {
    pub fn new(out: W, width: u32, height: u32, sps: &[u8], pps: &[u8]) -> io::Result<Self> {
        Self::with_audio(out, width, height, sps, pps, None)
    }

    pub fn with_audio(mut out: W, width: u32, height: u32, sps: &[u8], pps: &[u8], audio: Option<AudioTrack>) -> io::Result<Self> {
        if sps.len() < 4 || pps.is_empty() || width > u16::MAX as u32 || height > u16::MAX as u32 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid H.264 parameter sets or size"));
        }
        let (ftyp, moov) = init_segment(width as u16, height as u16, sps, pps, audio.as_ref(), Durations::default());
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
            audio: audio.map(|track| AudioState { track, pending: Vec::new(), pending_first: 0, count: 0, pcm_frames: 0 }),
        })
    }

    /// Appends the sample for frame slot `slot` (slots increase). A gap before it (pictures the encoder skipped)
    /// extends the duration of the previous sample.
    pub fn push(&mut self, slot: u64, data: Vec<u8>, key: bool) -> io::Result<()> {
        if let (Some(last), true) = (self.pending.last_mut(), slot > self.frames) {
            last.frames += (slot - self.frames) as u32;
        }
        if self.pending.len() >= FRAGMENT_FRAMES || (!self.pending.is_empty() && slot - self.pending_first >= FRAGMENT_FRAMES as u64) {
            self.flush_fragment()?;
        }
        if self.pending.is_empty() {
            self.pending_first = slot;
        }
        self.pending.push(Sample { data, key, frames: 1 });
        self.frames = slot + 1;
        Ok(())
    }

    /// Appends the next AAC access unit (no-op without an audio track). About every second a fragment is written.
    pub fn push_audio(&mut self, au: Vec<u8>) -> io::Result<()> {
        let Some(a) = self.audio.as_mut() else { return Ok(()) };
        if a.pending.is_empty() {
            a.pending_first = a.count;
        }
        a.pending.push(au);
        a.count += 1;
        if a.pending.len() >= AU_PER_FRAGMENT {
            self.flush_fragment()?;
        }
        Ok(())
    }

    /// Real length of the audio in PCM frames (cuts the encoder padding off the end via the edit list).
    pub fn set_audio_pcm_frames(&mut self, frames: u64) {
        if let Some(a) = self.audio.as_mut() {
            a.pcm_frames = frames;
        }
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    fn flush_fragment(&mut self) -> io::Result<()> {
        let audio_pending = self.audio.as_ref().is_some_and(|a| !a.pending.is_empty());
        if self.pending.is_empty() && !audio_pending {
            return Ok(());
        }
        self.seq += 1;
        let v_payload: usize = self.pending.iter().map(|s| s.data.len()).sum();
        let a_payload: usize = self.audio.as_ref().map_or(0, |a| a.pending.iter().map(|s| s.len()).sum());
        let moof = self.moof(0, 0);
        let base = moof.len() as u32 + 8;
        let moof = self.moof(base, base + v_payload as u32);
        self.out.write_all(&moof)?;
        self.out.write_all(&u32b(8 + (v_payload + a_payload) as u32))?;
        self.out.write_all(b"mdat")?;
        for s in &self.pending {
            self.out.write_all(&s.data)?;
        }
        if let Some(a) = self.audio.as_mut() {
            for s in &a.pending {
                self.out.write_all(s)?;
            }
            a.pending.clear();
        }
        self.out.flush()?;
        self.bytes += (moof.len() + 8 + v_payload + a_payload) as u64;
        self.pending.clear();
        Ok(())
    }

    fn moof(&self, video_offset: u32, audio_offset: u32) -> Vec<u8> {
        let mfhd = bx(b"mfhd", &[&[0; 4], &u32b(self.seq)]);
        let mut trafs = Vec::new();
        if !self.pending.is_empty() {
            let tfhd = bx(b"tfhd", &[&u32b(0x0002_0000), &u32b(1)]); // default-base-is-moof
            let tfdt = bx(b"tfdt", &[&u32b(0x0100_0000), &(self.pending_first * TICKS_PER_FRAME as u64).to_be_bytes()]);
            let mut entries = Vec::with_capacity(self.pending.len() * 12);
            for s in &self.pending {
                entries.extend_from_slice(&u32b(TICKS_PER_FRAME * s.frames));
                entries.extend_from_slice(&u32b(s.data.len() as u32));
                // sample_depends_on = 2 (I picture) / depends on others + non-sync flag
                entries.extend_from_slice(&u32b(if s.key { 0x0200_0000 } else { 0x0101_0000 }));
            }
            let trun = bx(b"trun", &[&u32b(0x0000_0701), &u32b(self.pending.len() as u32), &video_offset.to_be_bytes(), &entries]);
            trafs.push(bx(b"traf", &[&tfhd, &tfdt, &trun]));
        }
        if let Some(a) = self.audio.as_ref().filter(|a| !a.pending.is_empty()) {
            // default-base-is-moof + default-sample-duration
            let tfhd = bx(b"tfhd", &[&u32b(0x0002_0008), &u32b(2), &u32b(AU_FRAMES)]);
            let tfdt = bx(b"tfdt", &[&u32b(0x0100_0000), &(a.pending_first * AU_FRAMES as u64).to_be_bytes()]);
            let mut entries = Vec::with_capacity(a.pending.len() * 4);
            for s in &a.pending {
                entries.extend_from_slice(&u32b(s.len() as u32));
            }
            // data offset + sample size present
            let trun = bx(b"trun", &[&u32b(0x0000_0201), &u32b(a.pending.len() as u32), &audio_offset.to_be_bytes(), &entries]);
            trafs.push(bx(b"traf", &[&tfhd, &tfdt, &trun]));
        }
        let mut parts: Vec<&[u8]> = vec![&mfhd];
        parts.extend(trafs.iter().map(|t| t.as_slice()));
        bx(b"moof", &parts)
    }

    /// Writes the last fragment, stores the real duration in the header and returns the writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_fragment()?;
        let video_ms = (self.frames * 1000 / FPS as u64).min(u32::MAX as u64) as u32;
        let audio_ms = self.audio.as_ref().map_or(0, |a| {
            let pcm = if a.pcm_frames > 0 { a.pcm_frames } else { (a.count * AU_FRAMES as u64).saturating_sub(a.track.priming as u64) };
            (pcm * 1000 / AUDIO_RATE as u64).min(u32::MAX as u64) as u32
        });
        let track = self.audio.as_ref().map(|a| &a.track);
        let (_, moov) = init_segment(self.dims.0, self.dims.1, &self.sps, &self.pps, track, Durations { video_ms, audio_ms });
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

    fn find(data: &[u8], kind: &[u8; 4]) -> Vec<usize> {
        data.windows(4).enumerate().filter(|(_, w)| w == kind).map(|(i, _)| i).collect()
    }

    #[test]
    fn audio_track_boxes_fragments_and_timestamps() {
        let asc = vec![0x11, 0x90];
        let track = AudioTrack { asc: asc.clone(), priming: 1024, bitrate: 160_000 };
        let mut w = Mp4Writer::with_audio(Cursor::new(Vec::new()), 64, 48, &[0x67, 0x42, 0xC0, 0x1F], &[0x68, 0xCE], Some(track)).unwrap();
        for n in 0..65u64 {
            w.push(n, vec![0, 0, 0, 2, 0x41, n as u8], n % 60 == 0).unwrap();
            // about 1.5 audio access units per frame (48000 / 1024 / 30)
            for _ in 0..(if n % 2 == 0 { 2 } else { 1 }) {
                w.push_audio(vec![0xAA; 20]).unwrap();
            }
        }
        w.set_audio_pcm_frames(98 * 1024 - 1024);
        let data = w.finish().unwrap().into_inner();
        assert_eq!(find(&data, b"trak").len(), 2);
        // esds carries the AudioSpecificConfig; the edit list skips the priming.
        let e = find(&data, b"esds")[0];
        assert!(data[e..e + 80].windows(2).any(|p| p == asc.as_slice()));
        assert_eq!(data[e + 4..e + 8], [0, 0, 0, 0]); // version + flags
        assert_eq!(data[e + 8], 0x03); // ES_Descriptor
        let el = find(&data, b"elst")[0];
        let (dur, media_time) = (u32::from_be_bytes(data[el + 12..el + 16].try_into().unwrap()), u32::from_be_bytes(data[el + 16..el + 20].try_into().unwrap()));
        assert_eq!(media_time, 1024);
        assert_eq!(dur, (97 * 1024 * 1000 / 48_000) as u32); // patched in finish
        // Every fragment (about 1 s) has an audio traf next to the video traf; audio time stamps run on.
        let moofs = find(&data, b"moof");
        assert!(moofs.len() >= 2, "{} fragments", moofs.len());
        let tfdts: Vec<u64> = find(&data, b"tfdt").iter().map(|&i| u64::from_be_bytes(data[i + 8..i + 16].try_into().unwrap())).collect();
        let audio_tfdt: Vec<u64> = tfdts.iter().skip(1).step_by(2).copied().collect();
        assert_eq!(audio_tfdt.len(), moofs.len());
        assert!(audio_tfdt.windows(2).all(|p| p[1] > p[0] && p[0] % 1024 == 0));
        assert_eq!(audio_tfdt[0], 0);
    }

    #[test]
    fn au_counter_to_time() {
        assert_eq!(au_pts_ms(0), 0.0);
        assert!((au_pts_ms(47) - 1002.666).abs() < 0.01);
        assert!((au_pts_ms(46_875) - 1_000_000.0).abs() < 1e-6); // 46875 * 1024 = 48 000 000 frames = 1000 s
    }

    #[test]
    fn audio_alone_triggers_fragments_and_video_only_files_have_one_track() {
        let sps = [0x67, 0x42, 0xC0, 0x1F];
        let mut w = Mp4Writer::with_audio(Cursor::new(Vec::new()), 16, 16, &sps, &[0x68, 0xCE], Some(AudioTrack { asc: vec![0x11, 0x90], priming: 1024, bitrate: 128_000 })).unwrap();
        for _ in 0..(AU_PER_FRAGMENT * 2) {
            w.push_audio(vec![1; 10]).unwrap(); // no video at all (a pause)
        }
        let data = w.finish().unwrap().into_inner();
        assert_eq!(find(&data, b"moof").len(), 2);
        assert_eq!(find(&data, b"tfdt").len(), 2, "audio-only fragments have no video traf");
        let mut v = Mp4Writer::new(Cursor::new(Vec::new()), 16, 16, &sps, &[0x68, 0xCE]).unwrap();
        v.push_audio(vec![1]).unwrap(); // ignored without a track
        assert!(!v.has_audio());
        let data = v.finish().unwrap().into_inner();
        assert_eq!((find(&data, b"trak").len(), find(&data, b"esds").len()), (1, 0));
    }

    #[test]
    fn a_long_gap_flushes_the_fragment_and_keeps_the_timeline() {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), 16, 16, &[0x67, 0x42, 0xC0, 0x1F], &[0x68, 0xCE]).unwrap();
        w.push(0, vec![0, 0, 0, 1, 0x65], true).unwrap();
        w.push(1, vec![0, 0, 0, 1, 0x41], false).unwrap();
        w.push(100, vec![0, 0, 0, 1, 0x41], false).unwrap(); // 3.3 s later (no cut)
        let data = w.finish().unwrap().into_inner();
        assert_eq!(find(&data, b"moof").len(), 2);
        let tfdt = find(&data, b"tfdt");
        assert_eq!(u64::from_be_bytes(data[tfdt[1] + 8..tfdt[1] + 16].try_into().unwrap()), 100 * TICKS_PER_FRAME as u64);
    }
}

