use std::{
    fs::{File, OpenOptions},
    mem::{align_of, size_of},
    path::{Path, PathBuf},
    slice,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use memmap2::{Mmap, MmapMut};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 1;
pub const RING_SECONDS: usize = 10;
pub const RING_SAMPLES: usize = SAMPLE_RATE as usize * RING_SECONDS;
pub const LATENCY_FRAMES: u64 = SAMPLE_RATE as u64 / 20;
pub const DEFAULT_RING_PATH: &str = "/tmp/m5mic-virtual-mic-ring-v1";

const MAGIC: u32 = u32::from_le_bytes(*b"M5VR");
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 4096;
const TOTAL_BYTES: usize = HEADER_BYTES + RING_SAMPLES * size_of::<f32>();

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FeedState {
    Idle = 0,
    Receiving = 1,
}

#[repr(C, align(64))]
struct SharedHeader {
    magic: AtomicU32,
    version: AtomicU32,
    header_bytes: AtomicU32,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    ring_samples: AtomicU32,
    state: AtomicU32,
    _reserved: AtomicU32,
    write_index: AtomicU64,
    generation: AtomicU64,
    last_write_unix_ms: AtomicU64,
}

pub struct VirtualMicWriter {
    map: MmapMut,
}

pub struct VirtualMicReader {
    map: Mmap,
    read_index: AtomicU64,
    latency_frames: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeedStatus {
    pub state: FeedState,
    pub write_index: u64,
    pub generation: u64,
    pub last_write_unix_ms: u64,
}

impl VirtualMicWriter {
    pub fn open_default() -> Result<Self> {
        Self::open(DEFAULT_RING_PATH)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = open_ring_file(path, true)?;
        file.set_len(TOTAL_BYTES as u64)
            .with_context(|| format!("size virtual mic ring {}", path.display()))?;
        let mut map = unsafe { MmapMut::map_mut(&file) }
            .with_context(|| format!("map virtual mic ring {}", path.display()))?;
        initialize_header(&mut map);
        Ok(Self { map })
    }

    pub fn write_f32(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }

        let write_index = header(&self.map).write_index.load(Ordering::Relaxed);
        let ring_samples = RING_SAMPLES;
        let samples_to_write = samples.len().min(ring_samples);
        let source = &samples[samples.len() - samples_to_write..];
        let start = write_index as usize % ring_samples;

        {
            let ring = samples_mut(&mut self.map);
            let first = source.len().min(ring_samples - start);
            ring[start..start + first].copy_from_slice(&source[..first]);
            if first < source.len() {
                ring[..source.len() - first].copy_from_slice(&source[first..]);
            }
        }

        let header = header(&self.map);
        header.write_index.store(
            write_index.saturating_add(source.len() as u64),
            Ordering::Release,
        );
        header
            .last_write_unix_ms
            .store(now_unix_ms(), Ordering::Release);
        header
            .state
            .store(FeedState::Receiving as u32, Ordering::Release);
    }

    pub fn set_idle(&mut self) {
        let header = header(&self.map);
        header
            .state
            .store(FeedState::Idle as u32, Ordering::Release);
        header.generation.fetch_add(1, Ordering::AcqRel);
    }

    pub fn status(&self) -> FeedStatus {
        status(header(&self.map))
    }
}

impl VirtualMicReader {
    pub fn open_default() -> Result<Self> {
        Self::open(DEFAULT_RING_PATH, LATENCY_FRAMES)
    }

    pub fn open(path: impl AsRef<Path>, latency_frames: u64) -> Result<Self> {
        let file = open_ring_file(path.as_ref(), false)?;
        let map = unsafe { Mmap::map(&file) }
            .with_context(|| format!("map virtual mic ring {}", path.as_ref().display()))?;
        validate_header(&map)?;
        let write_index = header(&map).write_index.load(Ordering::Acquire);
        Ok(Self {
            map,
            read_index: AtomicU64::new(write_index.saturating_sub(latency_frames)),
            latency_frames,
        })
    }

    pub fn read_f32(&self, out: &mut [f32]) -> usize {
        if out.is_empty() {
            return 0;
        }

        let header = header(&self.map);
        if header.magic.load(Ordering::Acquire) != MAGIC
            || header.state.load(Ordering::Acquire) != FeedState::Receiving as u32
        {
            out.fill(0.0);
            return 0;
        }

        let write_index = header.write_index.load(Ordering::Acquire);
        let ring_samples = RING_SAMPLES as u64;
        let mut read_index = self.read_index.load(Ordering::Relaxed);
        let mut available = write_index.saturating_sub(read_index);
        if available > ring_samples {
            read_index = write_index.saturating_sub(ring_samples);
            available = write_index.saturating_sub(read_index);
        }
        if self.latency_frames > 0 && available > self.latency_frames.saturating_mul(2) {
            read_index = write_index.saturating_sub(self.latency_frames);
            available = write_index.saturating_sub(read_index);
        }

        if available == 0 {
            out.fill(0.0);
            return 0;
        }

        let to_read = out.len().min(available as usize);
        let start = read_index as usize % RING_SAMPLES;
        let ring = samples(&self.map);
        let first = to_read.min(RING_SAMPLES - start);
        out[..first].copy_from_slice(&ring[start..start + first]);
        if first < to_read {
            out[first..to_read].copy_from_slice(&ring[..to_read - first]);
        }
        if to_read < out.len() {
            out[to_read..].fill(0.0);
        }
        self.read_index
            .store(read_index.saturating_add(to_read as u64), Ordering::Relaxed);
        to_read
    }

    pub fn status(&self) -> FeedStatus {
        status(header(&self.map))
    }
}

pub fn default_ring_path() -> PathBuf {
    PathBuf::from(DEFAULT_RING_PATH)
}

fn open_ring_file(path: &Path, create: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    if create {
        options.create(true).write(true);
    }
    options
        .open(path)
        .with_context(|| format!("open virtual mic ring {}", path.display()))
}

fn initialize_header(map: &mut MmapMut) {
    assert!(HEADER_BYTES >= size_of::<SharedHeader>());
    assert_eq!(HEADER_BYTES % align_of::<SharedHeader>(), 0);

    let initialized = {
        let header = header(map);
        header.magic.load(Ordering::Acquire) == MAGIC
            && header.version.load(Ordering::Acquire) == VERSION
            && header.sample_rate.load(Ordering::Acquire) == SAMPLE_RATE
            && header.channels.load(Ordering::Acquire) == CHANNELS
            && header.ring_samples.load(Ordering::Acquire) == RING_SAMPLES as u32
    };
    if initialized {
        header(map)
            .state
            .store(FeedState::Idle as u32, Ordering::Release);
        return;
    }

    let header_ref = header(map);
    header_ref.magic.store(0, Ordering::Release);
    header_ref.version.store(VERSION, Ordering::Release);
    header_ref
        .header_bytes
        .store(HEADER_BYTES as u32, Ordering::Release);
    header_ref.sample_rate.store(SAMPLE_RATE, Ordering::Release);
    header_ref.channels.store(CHANNELS, Ordering::Release);
    header_ref
        .ring_samples
        .store(RING_SAMPLES as u32, Ordering::Release);
    header_ref
        .state
        .store(FeedState::Idle as u32, Ordering::Release);
    header_ref.write_index.store(0, Ordering::Release);
    header_ref.generation.store(1, Ordering::Release);
    header_ref.last_write_unix_ms.store(0, Ordering::Release);
    samples_mut(map).fill(0.0);
    header(map).magic.store(MAGIC, Ordering::Release);
}

fn validate_header(map: &Mmap) -> Result<()> {
    let header = header(map);
    if header.magic.load(Ordering::Acquire) != MAGIC {
        return Err(anyhow!("virtual mic ring is not initialized"));
    }
    if header.version.load(Ordering::Acquire) != VERSION
        || header.sample_rate.load(Ordering::Acquire) != SAMPLE_RATE
        || header.channels.load(Ordering::Acquire) != CHANNELS
        || header.ring_samples.load(Ordering::Acquire) != RING_SAMPLES as u32
    {
        return Err(anyhow!("virtual mic ring format mismatch"));
    }
    Ok(())
}

fn status(header: &SharedHeader) -> FeedStatus {
    let state = match header.state.load(Ordering::Acquire) {
        value if value == FeedState::Receiving as u32 => FeedState::Receiving,
        _ => FeedState::Idle,
    };
    FeedStatus {
        state,
        write_index: header.write_index.load(Ordering::Acquire),
        generation: header.generation.load(Ordering::Acquire),
        last_write_unix_ms: header.last_write_unix_ms.load(Ordering::Acquire),
    }
}

fn header(map: &[u8]) -> &SharedHeader {
    unsafe { &*(map.as_ptr() as *const SharedHeader) }
}

fn samples(map: &[u8]) -> &[f32] {
    unsafe { slice::from_raw_parts(map.as_ptr().add(HEADER_BYTES) as *const f32, RING_SAMPLES) }
}

fn samples_mut(map: &mut [u8]) -> &mut [f32] {
    unsafe {
        slice::from_raw_parts_mut(map.as_mut_ptr().add(HEADER_BYTES) as *mut f32, RING_SAMPLES)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_gets_written_samples() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = VirtualMicWriter::open(temp.path()).unwrap();
        let reader = VirtualMicReader::open(temp.path(), 0).unwrap();
        writer.write_f32(&[0.25, -0.5, 0.75]);
        let mut out = [0.0; 4];
        let read = reader.read_f32(&mut out);

        assert_eq!(read, 3);
        assert_eq!(out, [0.25, -0.5, 0.75, 0.0]);
    }

    #[test]
    fn idle_feed_reads_silence() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let mut writer = VirtualMicWriter::open(temp.path()).unwrap();
        writer.write_f32(&[1.0, 1.0]);
        writer.set_idle();

        let reader = VirtualMicReader::open(temp.path(), 0).unwrap();
        let mut out = [9.0; 2];
        let read = reader.read_f32(&mut out);

        assert_eq!(read, 0);
        assert_eq!(out, [0.0, 0.0]);
    }
}
