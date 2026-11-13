//! Test client: speaks the monitor protocol and sends a moving colour gradient (default 1280x720, 30 fps).
//! Usage: fake_layer [width height] (socket: WIVRN_MONITOR_SOCK or the default path).
use std::os::fd::AsRawFd;
use std::time::Duration;
use wivrn_monitor_protocol as proto;
use wivrn_monitor_protocol::{Mapping, Message, PixelFormat, RingInfo};

fn main() {
    let args: Vec<u32> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (w, h) = (args.first().copied().unwrap_or(1280), args.get(1).copied().unwrap_or(720));
    let fd = proto::connect(&proto::socket_path()).expect("connect (is the viewer running?)");
    let info = RingInfo::new(1, w, h, PixelFormat::Rgba8Srgb, 3);
    let ring = proto::create_memfd("fake-ring", info.total_size()).expect("memfd");
    let map = Mapping::new(ring.as_raw_fd(), info.total_size() as usize, true).expect("mmap");
    proto::send_msg(fd.as_raw_fd(), &Message::Hello { version: proto::VERSION, pid: std::process::id() }, None, false).unwrap();
    proto::send_msg(fd.as_raw_fd(), &Message::Setup(info), Some(ring.as_raw_fd()), false).unwrap();
    for n in 0u64.. {
        let slot = (n % 3) as u32;
        let range = info.slot_range(slot).unwrap();
        // SAFETY: only this process writes the memfd.
        let buf = unsafe { map.slice_mut(range) };
        let shift = (n * 4) as u32;
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                buf[i..i + 4].copy_from_slice(&[((x + shift) * 255 / w) as u8, (y * 255 / h) as u8, 128, 0]);
            }
        }
        if proto::send_msg(fd.as_raw_fd(), &Message::Frame { gen: 1, slot, frame_nr: n, timestamp_ns: 0 }, None, false).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(33));
    }
}
