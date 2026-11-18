//! Bounded, best-effort persistent storage for privacy-minimised QR timing events.
//! A single background OS thread writes ten fixed-size segments. The QR path
//! never waits for disk I/O; overload or storage failures may drop events.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use once_cell::sync::OnceCell;

const SEGMENTS: usize = 10;
const SEGMENT_BYTES: u64 = 5 * 1024 * 1024;
const QUEUE_CAPACITY: usize = 512;
const MAX_RECORD_BYTES: usize = 2048;
static SENDER: OnceCell<SyncSender<Vec<u8>>> = OnceCell::new();
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn segment(dir: &Path, index: usize) -> PathBuf {
    dir.join(format!("qr-timing.{index}.jsonl"))
}

/// Start once, using a private directory beside the configured accounts DB.
/// A failed spawn is reported to the caller; attendance continues without it.
/// Assumes a single server process owns this directory.
pub fn init(accounts_path: &Path) -> io::Result<()> {
    if SENDER.get().is_some() { return Ok(()); }
    let base = accounts_path.parent().filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let dir = base.join("qr-timing-logs");
    let (tx, rx) = sync_channel::<Vec<u8>>(QUEUE_CAPACITY);
    std::thread::Builder::new().name("qr-timing-writer".to_string()).spawn(move || {
        let mut writer = match SegmentWriter::open(&dir) {
            Ok(w) => Some(w),
            Err(e) => {
                eprintln!("QR timing storage unavailable: {e}");
                None
            }
        };
        let mut reported_failure = writer.is_none();
        while let Ok(mut line) = rx.recv() {
            if writer.is_none() {
                writer = SegmentWriter::open(&dir).ok();
            }
            let dropped = DROPPED.swap(0, Ordering::Relaxed);
            if dropped > 0 {
                // Preserve a bounded count of known losses without blocking producers.
                let marker = format!("{{\"event\":\"qr_timing_dropped\",\"count\":{dropped}}}\n");
                let marker_ok = writer.as_mut().map(|w| w.append(marker.as_bytes()).is_ok()).unwrap_or(false);
                if !marker_ok { DROPPED.fetch_add(dropped, Ordering::Relaxed); writer = None; }
            }
            line.push(b'\n');
            let ok = writer.as_mut().map(|w| w.append(&line).is_ok()).unwrap_or(false);
            if !ok {
                DROPPED.fetch_add(1, Ordering::Relaxed);
                writer = None;
                if !reported_failure {
                    eprintln!("QR timing storage write failed; timing events may be dropped");
                    reported_failure = true;
                }
            } else {
                reported_failure = false;
            }
        }
    })?;
    SENDER.set(tx).map_err(|_| io::Error::new(io::ErrorKind::AlreadyExists, "QR timing writer already started"))?;
    Ok(())
}

/// Serialize a small, caller-supplied allowlisted object, then enqueue without
/// waiting for the background writer. No account ID, QR payload or secret.
pub fn record(mut event: serde_json::Value) {
    let Some(map) = event.as_object_mut() else { return; };
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64).unwrap_or(0);
    map.insert("record_epoch_ms".to_string(), serde_json::json!(now_ms));
    let Ok(bytes) = serde_json::to_vec(&event) else { DROPPED.fetch_add(1, Ordering::Relaxed); return; };
    if bytes.len() + 1 > MAX_RECORD_BYTES {
        DROPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    match SENDER.get().map(|tx| tx.try_send(bytes)) {
        Some(Ok(())) => {}
        Some(Err(TrySendError::Full(_))) | Some(Err(TrySendError::Disconnected(_))) | None => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct SegmentWriter { dir: PathBuf, file: File, size: u64 }

impl SegmentWriter {
    fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        #[cfg(unix)] {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        }
        // Repair oversized managed files left by external interference before
        // opening the active segment. The ten managed files remain bounded.
        for index in 0..SEGMENTS {
            let path = segment(dir, index);
            if path.metadata().map(|m| m.len() > SEGMENT_BYTES).unwrap_or(false) {
                fs::remove_file(path)?;
            }
        }
        let file = open_segment(&segment(dir, 0))?;
        let size = file.metadata()?.len();
        Ok(Self { dir: dir.to_path_buf(), file, size })
    }

    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        if line.len() as u64 > SEGMENT_BYTES { return Err(io::Error::new(io::ErrorKind::InvalidData, "QR timing record too large")); }
        if self.size + line.len() as u64 > SEGMENT_BYTES { self.rotate()?; }
        // Interrupted prior writes can leave a partial JSON line. Separate it
        // from the next record rather than merging two JSON values.
        if self.size > 0 {
            self.file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            self.file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                if self.size + 1 + line.len() as u64 > SEGMENT_BYTES { self.rotate()?; }
                else { self.file.write_all(b"\n")?; self.size += 1; }
            }
        }
        self.file.write_all(line)?;
        self.size += line.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let oldest = segment(&self.dir, SEGMENTS - 1);
        match fs::remove_file(oldest) {
            Ok(()) => {}, Err(e) if e.kind() == io::ErrorKind::NotFound => {}, Err(e) => return Err(e),
        }
        for index in (0..SEGMENTS - 1).rev() {
            match fs::rename(segment(&self.dir, index), segment(&self.dir, index + 1)) {
                Ok(()) => {}, Err(e) if e.kind() == io::ErrorKind::NotFound => {}, Err(e) => return Err(e),
            }
        }
        self.file = open_segment(&segment(&self.dir, 0))?;
        self.size = self.file.metadata()?.len();
        Ok(())
    }
}

fn open_segment(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)] {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_rotation_stays_within_bound() {
        let dir = std::env::temp_dir().join(format!("qr-timing-test-{}", uuid::Uuid::new_v4()));
        let mut writer = SegmentWriter::open(&dir).unwrap();
        let mut full = vec![b'x'; SEGMENT_BYTES as usize];
        *full.last_mut().unwrap() = b'\n';
        writer.append(&full).unwrap();
        writer.append(b"{}\n").unwrap();
        assert_eq!(fs::metadata(segment(&dir, 0)).unwrap().len(), 3);
        assert_eq!(fs::metadata(segment(&dir, 1)).unwrap().len(), SEGMENT_BYTES);
        drop(writer);
        let writer = SegmentWriter::open(&dir).unwrap();
        assert_eq!(writer.size, 3);
        drop(writer);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn partial_line_is_separated_after_restart() {
        let dir = std::env::temp_dir().join(format!("qr-timing-test-{}", uuid::Uuid::new_v4()));
        let mut writer = SegmentWriter::open(&dir).unwrap();
        writer.append(b"partial").unwrap();
        drop(writer);
        let mut writer = SegmentWriter::open(&dir).unwrap();
        writer.append(b"{}\n").unwrap();
        drop(writer);
        assert_eq!(fs::read(segment(&dir, 0)).unwrap(), b"partial\n{}\n");
        fs::remove_dir_all(dir).unwrap();
    }
}
