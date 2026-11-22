//! Per-VM packet capture (`ARCHITECTURE.md`, packet capture methods).
//!
//! The daemon reads every frame on a VM's TAP with its own `AF_PACKET` socket and
//! writes a classic pcap file (Ethernet link type). No external capture binary runs.

use crate::rpc::{Capture, CaptureState};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Maximum pcap file size, including headers (256 MiB).
pub const CAPTURE_LIMIT_BYTES: u64 = 256 * 1_048_576;

/// Largest frame recorded; longer frames are truncated (pcap `snaplen`).
const SNAPLEN: usize = 65_535;
const PCAP_GLOBAL_HEADER_LEN: u64 = 24;
const PCAP_RECORD_HEADER_LEN: u64 = 16;
const LINKTYPE_ETHERNET: u32 = 1;
const RECV_TIMEOUT: Duration = Duration::from_millis(200);

/// A source of raw Ethernet frames.
pub trait PacketSource: Send {
    /// Receive one frame into `buf`. Returns `(captured_len, original_len)`, or
    /// `None` when no frame arrived within the source's poll interval.
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, usize)>>;
}

/// `AF_PACKET` raw socket bound to one interface, seeing both directions.
pub struct AfPacketSource {
    fd: OwnedFd,
}

impl AfPacketSource {
    pub fn open(ifname: &str) -> io::Result<Self> {
        let c_name =
            CString::new(ifname).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let ifindex = unsafe { libc::if_nametoindex(c_name.as_ptr()) };
        if ifindex == 0 {
            return Err(io::Error::last_os_error());
        }

        let proto = (libc::ETH_P_ALL as u16).to_be();
        let raw = unsafe {
            libc::socket(
                libc::AF_PACKET,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                i32::from(proto),
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };

        let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = proto;
        addr.sll_ifindex = ifindex as i32;
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const libc::sockaddr_ll as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }

        let tv = libc::timeval {
            tv_sec: 0,
            tv_usec: RECV_TIMEOUT.as_micros() as libc::suseconds_t,
        };
        let rc = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                &tv as *const libc::timeval as *const libc::c_void,
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd })
    }
}

impl PacketSource for AfPacketSource {
    fn recv(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, usize)>> {
        // MSG_TRUNC makes the return value the frame's real length even when truncated.
        let n = unsafe {
            libc::recv(
                self.fd.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_TRUNC,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
                _ => Err(err),
            };
        }
        let orig = n as usize;
        Ok(Some((orig.min(buf.len()), orig)))
    }
}

/// A running capture's writer thread and live state.
pub struct CaptureHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    state: Arc<Mutex<Capture>>,
}

impl CaptureHandle {
    /// Start writing frames from `source` into `file` on a dedicated thread.
    /// `persist` is called with the final Capture object when writing ends.
    pub fn spawn(
        source: Box<dyn PacketSource>,
        file: File,
        capture: Capture,
        limit_bytes: u64,
        persist: Box<dyn Fn(&Capture) + Send>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(capture));
        let thread = {
            let stop = Arc::clone(&stop);
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                let end_state = write_pcap(source, file, limit_bytes, &stop, &state);
                let final_capture = {
                    let mut c = state.lock().unwrap();
                    c.state = end_state;
                    c.clone()
                };
                persist(&final_capture);
            })
        };
        Self {
            stop,
            thread: Some(thread),
            state,
        }
    }

    /// Current Capture object (live byte count while running).
    pub fn snapshot(&self) -> Capture {
        self.state.lock().unwrap().clone()
    }

    /// Whether the writer is still recording.
    pub fn is_running(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Stop recording, wait for the final flush and persist, and return the result.
    pub fn stop(mut self) -> Capture {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        self.snapshot()
    }
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Write pcap until stopped, the source fails, or the size limit is reached.
fn write_pcap(
    mut source: Box<dyn PacketSource>,
    file: File,
    limit_bytes: u64,
    stop: &AtomicBool,
    state: &Mutex<Capture>,
) -> CaptureState {
    let mut out = BufWriter::new(file);
    let mut header = Vec::with_capacity(PCAP_GLOBAL_HEADER_LEN as usize);
    header.extend_from_slice(&0xa1b2_c3d4u32.to_le_bytes());
    header.extend_from_slice(&2u16.to_le_bytes());
    header.extend_from_slice(&4u16.to_le_bytes());
    header.extend_from_slice(&0i32.to_le_bytes());
    header.extend_from_slice(&0u32.to_le_bytes());
    header.extend_from_slice(&(SNAPLEN as u32).to_le_bytes());
    header.extend_from_slice(&LINKTYPE_ETHERNET.to_le_bytes());
    if out.write_all(&header).is_err() {
        return CaptureState::Stopped;
    }
    let mut written = PCAP_GLOBAL_HEADER_LEN;
    state.lock().unwrap().bytes = written;

    let mut buf = vec![0u8; SNAPLEN];
    let mut end_state = CaptureState::Stopped;
    while !stop.load(Ordering::SeqCst) {
        let (captured, orig) = match source.recv(&mut buf) {
            Ok(Some(lens)) => lens,
            Ok(None) => continue,
            Err(_) => break,
        };
        let record_len = PCAP_RECORD_HEADER_LEN + captured as u64;
        if written + record_len > limit_bytes {
            end_state = CaptureState::LimitReached;
            break;
        }
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let mut rec = Vec::with_capacity(record_len as usize);
        rec.extend_from_slice(&(ts.as_secs() as u32).to_le_bytes());
        rec.extend_from_slice(&ts.subsec_micros().to_le_bytes());
        rec.extend_from_slice(&(captured as u32).to_le_bytes());
        rec.extend_from_slice(&(orig as u32).to_le_bytes());
        rec.extend_from_slice(&buf[..captured]);
        if out.write_all(&rec).is_err() {
            break;
        }
        written += record_len;
        state.lock().unwrap().bytes = written;
    }

    if out.flush().is_ok() {
        if let Ok(file) = out.into_inner() {
            let _ = file.sync_all();
        }
    }
    end_state
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct ScriptedSource(VecDeque<Vec<u8>>);

    impl PacketSource for ScriptedSource {
        fn recv(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, usize)>> {
            match self.0.pop_front() {
                Some(frame) => {
                    let n = frame.len().min(buf.len());
                    buf[..n].copy_from_slice(&frame[..n]);
                    Ok(Some((n, frame.len())))
                }
                None => {
                    std::thread::sleep(Duration::from_millis(5));
                    Ok(None)
                }
            }
        }
    }

    fn capture() -> Capture {
        Capture {
            capture_id: "0c2f6e9a-7b1d-4c3e-9f80-2d5a6b7c8e91".to_string(),
            vm_id: "b6f51eed-8dc7-47f6-a8e7-81b6ae783a21".to_string(),
            started_at: "2026-11-22T00:00:00Z".to_string(),
            state: CaptureState::Running,
            bytes: 0,
        }
    }

    fn temp_path() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("hyperos-pcap-{}.pcap", uuid::Uuid::new_v4()))
    }

    #[test]
    fn writes_valid_pcap_and_persists_on_stop() {
        let path = temp_path();
        let frames = VecDeque::from(vec![vec![0xaa; 60], vec![0xbb; 100]]);
        let persisted = Arc::new(Mutex::new(None));
        let sink = Arc::clone(&persisted);
        let handle = CaptureHandle::spawn(
            Box::new(ScriptedSource(frames)),
            File::create(&path).unwrap(),
            capture(),
            CAPTURE_LIMIT_BYTES,
            Box::new(move |c| *sink.lock().unwrap() = Some(c.clone())),
        );
        let expected = 24 + (16 + 60) + (16 + 100);
        for _ in 0..200 {
            if handle.snapshot().bytes == expected {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(handle.is_running());
        let done = handle.stop();
        assert_eq!(done.state, CaptureState::Stopped);
        assert_eq!(done.bytes, expected);
        assert_eq!(persisted.lock().unwrap().as_ref(), Some(&done));

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, expected);
        assert_eq!(&bytes[0..4], &0xa1b2_c3d4u32.to_le_bytes());
        assert_eq!(&bytes[20..24], &1u32.to_le_bytes()); // Ethernet
        assert_eq!(&bytes[24 + 8..24 + 12], &60u32.to_le_bytes()); // incl_len
        assert_eq!(&bytes[24 + 12..24 + 16], &60u32.to_le_bytes()); // orig_len
        assert_eq!(bytes[24 + 16], 0xaa);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn stops_with_limit_reached_before_exceeding_cap() {
        let path = temp_path();
        let frames = VecDeque::from(vec![vec![1u8; 100]; 10]);
        let limit = 24 + 2 * (16 + 100) + 50; // room for two frames only
        let handle = CaptureHandle::spawn(
            Box::new(ScriptedSource(frames)),
            File::create(&path).unwrap(),
            capture(),
            limit,
            Box::new(|_| {}),
        );
        for _ in 0..200 {
            if !handle.is_running() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!handle.is_running());
        let done = handle.stop();
        assert_eq!(done.state, CaptureState::LimitReached);
        assert_eq!(done.bytes, 24 + 2 * (16 + 100));
        assert!(std::fs::metadata(&path).unwrap().len() <= limit);
        let _ = std::fs::remove_file(path);
    }
}
