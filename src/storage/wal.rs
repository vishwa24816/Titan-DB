use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::error::{Result, TitanError};

const WAL_MAGIC: u32 = 0x54495457; // "TITW"
const WAL_SIZE_CAP: u64 = 256 * 1024 * 1024; // 256 MiB — error, never OOM

/// Portable CRC32-IEEE (bitwise, std-only — no new deps per T-01-SC).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogRecord {
    Put {
        table: String,
        key: Vec<u8>,
        after: Vec<u8>,
        tx: u64,
    },
    Delete {
        table: String,
        key: Vec<u8>,
        tx: u64,
    },
    Commit {
        tx: u64,
    },
    Abort {
        tx: u64,
    },
    CheckpointBegin {
        active: Vec<u64>,
        dirty_lsn: u64,
    },
    CheckpointEnd {
        active: Vec<u64>,
        dirty_lsn: u64,
    },
}

impl LogRecord {
    pub fn tx(&self) -> Option<u64> {
        match self {
            LogRecord::Put { tx, .. } => Some(*tx),
            LogRecord::Delete { tx, .. } => Some(*tx),
            LogRecord::Commit { tx } => Some(*tx),
            LogRecord::Abort { tx } => Some(*tx),
            _ => None,
        }
    }
}

/// Append-only WAL: frame = magic u32 + LSN u64 + len u32 + crc32 u32 + bincode(LogRecord).
///
/// Group commit: `commit()` batches fsyncs — commits arriving within a
/// 10 ms window (or up to 32 pending) share one `sync_all`. Choice: 10 ms
/// bounds commit latency for interactive use while coalescing bursts;
/// 32 caps the batch so a flood can't delay the sync indefinitely.
pub struct Wal {
    file: Mutex<File>,
    path: PathBuf,
    next_lsn: Mutex<u64>,
    /// Group-commit state: (pending count, batch generation).
    pending: Mutex<(u64, u64)>,
    pending_cv: parking_lot::Condvar,
}

pub const GROUP_COMMIT_WINDOW: std::time::Duration = std::time::Duration::from_millis(10);
pub const GROUP_COMMIT_MAX_BATCH: u64 = 32;

impl Wal {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        let next_lsn = scan_next_lsn(&path)?;
        Ok(Wal {
            file: Mutex::new(file),
            path,
            next_lsn: Mutex::new(next_lsn),
            pending: Mutex::new((0, 0)),
            pending_cv: parking_lot::Condvar::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append a record, return its LSN. Does NOT sync (group-commit friendly).
    pub fn append(&self, record: &LogRecord) -> Result<u64> {
        let payload = bincode::serialize(record)?;
        let lsn = {
            let mut n = self.next_lsn.lock();
            let lsn = *n;
            *n += 1;
            lsn
        };
        let mut frame = Vec::with_capacity(16 + payload.len());
        frame.extend_from_slice(&WAL_MAGIC.to_le_bytes());
        frame.extend_from_slice(&lsn.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(&crc32(&payload).to_le_bytes());
        frame.extend_from_slice(&payload);
        {
            let mut file = self.file.lock();
            let len = file.metadata()?.len();
            if len + frame.len() as u64 > WAL_SIZE_CAP {
                return Err(TitanError::WalCorrupt(format!(
                    "WAL size cap ({} bytes) exceeded",
                    WAL_SIZE_CAP
                )));
            }
            file.seek(SeekFrom::End(0))?;
            file.write_all(&frame)?;
            file.flush()?;
        }
        Ok(lsn)
    }

    /// Commit: append Commit record + group-commit sync (durability ack).
    /// Leader-based batching: first committer in a window becomes leader,
    /// waits up to 10 ms (or 32 batched commits), issues ONE `sync_all`
    /// covering the batch; followers wait on the condvar for the batch
    /// generation to advance. Solo commits degrade to commit+sync.
    pub fn commit(&self, tx: u64) -> Result<u64> {
        let lsn = self.append(&LogRecord::Commit { tx })?;
        let mut p = self.pending.lock();
        if p.0 == 0 {
            let gen = p.1;
            p.0 = 1;
            drop(p);
            let deadline = std::time::Instant::now() + GROUP_COMMIT_WINDOW;
            loop {
                let mut p = self.pending.lock();
                if p.0 >= GROUP_COMMIT_MAX_BATCH || p.1 != gen {
                    break;
                }
                let now = std::time::Instant::now();
                if now >= deadline {
                    break;
                }
                let timeout = deadline - now;
                self.pending_cv.wait_for(&mut p, timeout);
            }
            self.sync_all()?;
            {
                let mut p = self.pending.lock();
                p.0 = 0;
                p.1 = p.1.wrapping_add(1);
            }
            self.pending_cv.notify_all();
        } else {
            let gen = p.1;
            p.0 += 1;
            if p.0 >= GROUP_COMMIT_MAX_BATCH {
                self.pending_cv.notify_one();
            }
            while p.1 == gen {
                self.pending_cv.wait(&mut p);
            }
        }
        Ok(lsn)
    }

    pub fn abort(&self, tx: u64) -> Result<u64> {
        let lsn = self.append(&LogRecord::Abort { tx })?;
        self.sync_all()?;
        Ok(lsn)
    }

    pub fn sync_all(&self) -> Result<()> {
        self.file.lock().sync_all()?;
        Ok(())
    }

    /// Fuzzy checkpoint: Begin/End markers + truncate prefix (compact file).
    /// Keeps frames at/after `keep_from_lsn`; rewrites file when safe.
    pub fn checkpoint(&self, active: Vec<u64>, dirty_lsn: u64, keep_from_lsn: u64) -> Result<()> {
        self.append(&LogRecord::CheckpointBegin {
            active: active.clone(),
            dirty_lsn,
        })?;
        self.append(&LogRecord::CheckpointEnd { active, dirty_lsn })?;
        self.sync_all()?;
        // Truncate prefix: keep only frames with LSN >= keep_from_lsn.
        let all = Self::read_all_frames(&self.path)?;
        let kept: Vec<(u64, LogRecord)> = all
            .into_iter()
            .filter(|(lsn, _)| *lsn >= keep_from_lsn)
            .collect();
        let mut file = self.file.lock();
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        for (lsn, rec) in &kept {
            let payload = bincode::serialize(rec)?;
            let mut frame = Vec::with_capacity(16 + payload.len());
            frame.extend_from_slice(&WAL_MAGIC.to_le_bytes());
            frame.extend_from_slice(&lsn.to_le_bytes());
            frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            frame.extend_from_slice(&crc32(&payload).to_le_bytes());
            frame.extend_from_slice(&payload);
            file.write_all(&frame)?;
        }
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    fn read_all_frames(path: &Path) -> Result<Vec<(u64, LogRecord)>> {
        let mut bytes = Vec::new();
        File::open(path)?.read_to_end(&mut bytes)?;
        parse_frames(&bytes)
    }

    /// Full replay: validate CRCs then Analysis -> Redo -> Undo.
    /// Returns committed data records (Put/Delete from committed txns only).
    pub fn replay(path: &Path) -> Result<Vec<LogRecord>> {
        let mut bytes = Vec::new();
        if !path.exists() {
            return Ok(Vec::new());
        }
        File::open(path)?.read_to_end(&mut bytes)?;
        // Size cap — never buffer unbounded input blindly (T-01-05).
        if bytes.len() as u64 > WAL_SIZE_CAP + 1024 * 1024 {
            return Err(TitanError::WalCorrupt(
                "WAL file exceeds size cap".to_string(),
            ));
        }
        let frames = parse_frames(&bytes)?;
        // Analysis: committed set + loser set from Commit/Abort markers.
        let mut committed = std::collections::HashSet::new();
        let mut aborted = std::collections::HashSet::new();
        for (_, rec) in &frames {
            match rec {
                LogRecord::Commit { tx } => {
                    committed.insert(*tx);
                }
                LogRecord::Abort { tx } => {
                    aborted.insert(*tx);
                }
                _ => {}
            }
        }
        // Redo: re-apply data records from committed txns (in LSN order).
        // Undo: drop loser-txn records (never applied).
        let mut redo = Vec::new();
        for (_, rec) in frames {
            match &rec {
                LogRecord::Put { tx, .. } | LogRecord::Delete { tx, .. } => {
                    if committed.contains(tx) && !aborted.contains(tx) {
                        redo.push(rec);
                    }
                }
                _ => {}
            }
        }
        Ok(redo)
    }
}

fn scan_next_lsn(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Ok(1);
    }
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    let frames = parse_frames(&bytes)?;
    Ok(frames
        .into_iter()
        .map(|(lsn, _)| lsn + 1)
        .max()
        .unwrap_or(1))
}

fn parse_frames(bytes: &[u8]) -> Result<Vec<(u64, LogRecord)>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < bytes.len() {
        // Header: magic u32 (4) + LSN u64 (8) + len u32 (4) + crc32 u32 (4) = 20 bytes.
        if bytes.len() - off < 20 {
            return Err(TitanError::WalCorrupt(format!(
                "torn frame header at offset {}",
                off
            )));
        }
        let magic = u32::from_le_bytes(
            bytes[off..off + 4]
                .try_into()
                .map_err(|_| TitanError::WalCorrupt("bad magic bytes".to_string()))?,
        );
        if magic != WAL_MAGIC {
            return Err(TitanError::WalCorrupt(format!(
                "bad frame magic {:#x} at offset {}",
                magic, off
            )));
        }
        let lsn = u64::from_le_bytes(
            bytes[off + 4..off + 12]
                .try_into()
                .map_err(|_| TitanError::WalCorrupt("bad lsn bytes".to_string()))?,
        );
        let len = u32::from_le_bytes(
            bytes[off + 12..off + 16]
                .try_into()
                .map_err(|_| TitanError::WalCorrupt("bad len bytes".to_string()))?,
        ) as usize;
        let crc = u32::from_le_bytes(
            bytes[off + 16..off + 20]
                .try_into()
                .map_err(|_| TitanError::WalCorrupt("bad crc bytes".to_string()))?,
        );
        // NOTE: header is 20 bytes (magic 4 + lsn 8 + len 4 + crc 4).
        let start = off + 20;
        let end = start
            .checked_add(len)
            .ok_or_else(|| TitanError::WalCorrupt("frame length overflow".to_string()))?;
        if end > bytes.len() {
            return Err(TitanError::WalCorrupt(format!(
                "torn frame payload at offset {} (need {}, have {})",
                off,
                len,
                bytes.len() - start
            )));
        }
        out.push((lsn, placeholder_parse(&bytes[start..end], crc, off)?));
        off = end;
    }
    Ok(out)
}

fn placeholder_parse(payload: &[u8], expect_crc: u32, off: usize) -> Result<LogRecord> {
    if crc32(payload) != expect_crc {
        return Err(TitanError::WalCorrupt(format!(
            "CRC mismatch at offset {}",
            off
        )));
    }
    let rec: LogRecord = bincode::deserialize(payload)
        .map_err(|e| TitanError::WalCorrupt(format!("frame decode failed at {}: {}", off, e)))?;
    Ok(rec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wal_commit_durable_unit() {
        let dir = std::env::temp_dir().join(format!("titan_walunit_{}.wal", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let wal = Wal::open(&dir).unwrap();
        let tx = 42;
        wal.append(&LogRecord::Put {
            table: "t".into(),
            key: b"1".to_vec(),
            after: b"v".to_vec(),
            tx,
        })
        .unwrap();
        wal.commit(tx).unwrap();
        let redo = Wal::replay(&dir).unwrap();
        assert_eq!(redo.len(), 1);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn wal_corrupt_frame_unit() {
        let dir = std::env::temp_dir().join(format!("titan_walcorr_{}.wal", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        {
            let wal = Wal::open(&dir).unwrap();
            wal.commit(7).unwrap();
        }
        let mut bytes = std::fs::read(&dir).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&dir, &bytes).unwrap();
        let err = Wal::replay(&dir).unwrap_err();
        assert!(matches!(err, TitanError::WalCorrupt(_)));
        let _ = std::fs::remove_file(&dir);
    }
}
