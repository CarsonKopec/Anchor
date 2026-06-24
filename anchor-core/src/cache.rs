//! Caches that sit between WinFsp callbacks and the network (spec §3.2): a TTL'd
//! directory-listing cache ([`DirCache`]), a TTL'd per-path metadata cache ([`StatCache`],
//! prefilled from listings), and a per-file adaptive read-ahead buffer ([`ReadAheadBuffer`]).
//! All are deliberately simple for v1 — none is a generalized LRU.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::remote_fs::{DirEntry, RemoteMetadata};

/// Read-ahead chunk size: 256 KiB (spec §3.2). Large enough to amortize round-trip
/// latency, small enough that one seek into a large file doesn't fetch megabytes that
/// get thrown away.
pub const READAHEAD_CHUNK: usize = 256 * 1024;
/// Maximum adaptive read-ahead window for sequential file reads.
pub const MAX_READAHEAD_CHUNK: usize = 4 * 1024 * 1024;

/// TTL-keyed directory listing cache.
///
/// Explorer re-lists directories constantly (icon overlays, thumbnails, preview panes);
/// a short TTL absorbs that without meaningfully delaying visibility of remote changes.
/// Any local mutation calls [`DirCache::invalidate_parent`] immediately, so the user's
/// own actions are never delayed by the TTL — only *other* clients' changes can lag.
pub struct DirCache {
    ttl: Duration,
    entries: Mutex<HashMap<PathBuf, (Instant, Vec<DirEntry>)>>,
}

impl DirCache {
    /// Create a cache whose entries live for `ttl`.
    pub fn new(ttl: Duration) -> Self {
        DirCache {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Return a cached listing for `dir` if present and not yet expired.
    pub fn get(&self, dir: &Path) -> Option<Vec<DirEntry>> {
        let mut map = self.entries.lock().unwrap();
        match map.get(dir) {
            Some((inserted, entries)) if inserted.elapsed() < self.ttl => Some(entries.clone()),
            Some(_) => {
                // Expired — drop it so the map doesn't accumulate stale keys.
                map.remove(dir);
                None
            }
            None => None,
        }
    }

    /// Cache a fresh listing for `dir`.
    pub fn insert(&self, dir: &Path, entries: Vec<DirEntry>) {
        self.entries
            .lock()
            .unwrap()
            .insert(dir.to_path_buf(), (Instant::now(), entries));
    }

    /// Drop the cached listing for exactly `dir`.
    pub fn invalidate(&self, dir: &Path) {
        self.entries.lock().unwrap().remove(dir);
    }

    /// Drop the cached listing for the *parent* of `path`. Called on every create/
    /// remove/rename/write so a directory the user just changed re-lists immediately.
    pub fn invalidate_parent(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            self.invalidate(parent);
        }
    }

    /// Drop everything (e.g. after a reconnect).
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

/// TTL-keyed metadata cache for individual paths.
///
/// Explorer frequently asks for the same file attributes immediately after a directory
/// listing. Keeping a short-lived stat cache avoids turning those repeated checks into
/// separate network round trips.
pub struct StatCache {
    ttl: Duration,
    entries: Mutex<HashMap<PathBuf, (Instant, RemoteMetadata)>>,
}

impl StatCache {
    /// Create a cache whose entries live for `ttl`.
    pub fn new(ttl: Duration) -> Self {
        StatCache {
            ttl,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Return cached metadata for `path` if present and not yet expired.
    pub fn get(&self, path: &Path) -> Option<RemoteMetadata> {
        let mut map = self.entries.lock().unwrap();
        match map.get(path) {
            Some((inserted, meta)) if inserted.elapsed() < self.ttl => Some(meta.clone()),
            Some(_) => {
                map.remove(path);
                None
            }
            None => None,
        }
    }

    /// Cache fresh metadata for `path`.
    pub fn insert(&self, path: &Path, meta: RemoteMetadata) {
        self.entries
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), (Instant::now(), meta));
    }

    /// Cache every child entry returned by a directory listing.
    pub fn insert_dir_entries(&self, dir: &Path, entries: &[DirEntry]) {
        let now = Instant::now();
        let mut map = self.entries.lock().unwrap();
        for entry in entries {
            map.insert(dir.join(&entry.name), (now, entry.metadata.clone()));
        }
    }

    /// Drop cached metadata for exactly `path`.
    pub fn invalidate(&self, path: &Path) {
        self.entries.lock().unwrap().remove(path);
    }

    /// Drop cached metadata for a path and its parent listing's children.
    pub fn invalidate_parent_children(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let mut map = self.entries.lock().unwrap();
            map.retain(|cached_path, _| cached_path.parent() != Some(parent));
        }
    }

    /// Drop everything (e.g. after a reconnect).
    pub fn clear(&self) {
        self.entries.lock().unwrap().clear();
    }
}

/// One cached chunk for one path.
struct CachedChunk {
    start: u64,
    data: Vec<u8>,
    requested: usize,
}

/// Per-file sequential chunk cache: a single [`READAHEAD_CHUNK`]-sized window per path.
///
/// Most real read patterns from Explorer/apps are sequential (preview, copy, media
/// start), so a single cached window turns N small sequential `read`s into one network
/// fetch. Invalidated on any write to that path.
pub struct ReadAheadBuffer {
    chunks: Mutex<HashMap<PathBuf, CachedChunk>>,
}

impl Default for ReadAheadBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadAheadBuffer {
    /// Create an empty buffer.
    pub fn new() -> Self {
        ReadAheadBuffer {
            chunks: Mutex::new(HashMap::new()),
        }
    }

    /// Return `[offset, offset+len)` for `path` iff it is fully contained in the
    /// currently-cached chunk for that path. A request that straddles the chunk edge
    /// misses and the caller fetches a fresh window via [`ReadAheadBuffer::fill`].
    pub fn get(&self, path: &Path, offset: u64, len: u32) -> Option<Vec<u8>> {
        let map = self.chunks.lock().unwrap();
        let chunk = map.get(path)?;
        let end = offset.checked_add(len as u64)?;
        let chunk_end = chunk.start + chunk.data.len() as u64;
        if offset >= chunk.start && end <= chunk_end {
            let lo = (offset - chunk.start) as usize;
            let hi = (end - chunk.start) as usize;
            Some(chunk.data[lo..hi].to_vec())
        } else {
            None
        }
    }

    /// Replace the cached window for `path` with `data` beginning at byte `start`.
    pub fn fill(&self, path: &Path, start: u64, data: Vec<u8>) {
        self.fill_with_request(path, start, data, READAHEAD_CHUNK);
    }

    /// Replace the cached window and remember how much the caller attempted to fetch.
    pub fn fill_with_request(&self, path: &Path, start: u64, data: Vec<u8>, requested: usize) {
        self.chunks.lock().unwrap().insert(
            path.to_path_buf(),
            CachedChunk {
                start,
                data,
                requested,
            },
        );
    }

    /// Pick a fetch size for a cache miss.
    ///
    /// Sequential reads grow the next fetch window, while random seeks fall back to the
    /// initial size so scrubbing a large file does not pull in too much unused data.
    pub fn next_fetch_len(&self, path: &Path, offset: u64, requested: u32) -> usize {
        let base = (requested as usize).clamp(READAHEAD_CHUNK, MAX_READAHEAD_CHUNK);
        let map = self.chunks.lock().unwrap();
        let Some(chunk) = map.get(path) else {
            return base;
        };
        let chunk_end = chunk.start + chunk.data.len() as u64;
        let full_window = chunk.data.len() >= chunk.requested;
        let sequential = offset >= chunk.start && offset <= chunk_end;
        if sequential && full_window {
            (chunk.requested.max(base) * 2).min(MAX_READAHEAD_CHUNK)
        } else {
            base
        }
    }

    /// Drop the cached window for `path` (called on any write to it).
    pub fn invalidate(&self, path: &Path) {
        self.chunks.lock().unwrap().remove(path);
    }

    /// Drop everything (e.g. after a reconnect).
    pub fn clear(&self) {
        self.chunks.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_fs::RemoteMetadata;

    fn entry(name: &str) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            metadata: RemoteMetadata::dir(),
        }
    }

    #[test]
    fn dir_cache_hit_then_expire() {
        let cache = DirCache::new(Duration::from_millis(40));
        let dir = Path::new("/a");
        cache.insert(dir, vec![entry("x")]);
        assert_eq!(cache.get(dir).map(|v| v.len()), Some(1));
        std::thread::sleep(Duration::from_millis(60));
        assert!(cache.get(dir).is_none(), "entry should have expired");
    }

    #[test]
    fn dir_cache_invalidate_parent() {
        let cache = DirCache::new(Duration::from_secs(60));
        let dir = Path::new("/a/b");
        cache.insert(dir, vec![entry("x")]);
        // A write to /a/b/file.txt must drop the listing of its parent /a/b.
        cache.invalidate_parent(Path::new("/a/b/file.txt"));
        assert!(cache.get(dir).is_none());
    }

    #[test]
    fn stat_cache_hit_then_expire() {
        let cache = StatCache::new(Duration::from_millis(40));
        let path = Path::new("/a/file.txt");
        cache.insert(path, RemoteMetadata::dir());
        assert!(cache.get(path).is_some());
        std::thread::sleep(Duration::from_millis(60));
        assert!(cache.get(path).is_none(), "entry should have expired");
    }

    #[test]
    fn stat_cache_prefills_from_directory_listing() {
        let cache = StatCache::new(Duration::from_secs(60));
        cache.insert_dir_entries(Path::new("/a"), &[entry("file.txt")]);
        assert!(cache.get(Path::new("/a/file.txt")).is_some());
        assert!(cache.get(Path::new("/a/missing.txt")).is_none());
    }

    #[test]
    fn stat_cache_invalidates_parent_children() {
        let cache = StatCache::new(Duration::from_secs(60));
        cache.insert(Path::new("/a/file.txt"), RemoteMetadata::dir());
        cache.insert(Path::new("/other/file.txt"), RemoteMetadata::dir());
        cache.invalidate_parent_children(Path::new("/a/new.txt"));
        assert!(cache.get(Path::new("/a/file.txt")).is_none());
        assert!(cache.get(Path::new("/other/file.txt")).is_some());
    }

    #[test]
    fn readahead_contained_hit_and_straddle_miss() {
        let buf = ReadAheadBuffer::new();
        let p = Path::new("/movie.mp4");
        buf.fill(p, 0, (0u8..100).collect());
        // Fully contained.
        assert_eq!(buf.get(p, 10, 20).unwrap(), (10u8..30).collect::<Vec<_>>());
        // Straddles the end of the cached window -> miss.
        assert!(buf.get(p, 90, 20).is_none());
        // Sequential continuation still within the window -> hit.
        assert_eq!(buf.get(p, 30, 10).unwrap(), (30u8..40).collect::<Vec<_>>());
    }

    #[test]
    fn readahead_invalidate_on_write() {
        let buf = ReadAheadBuffer::new();
        let p = Path::new("/f");
        buf.fill(p, 0, vec![1, 2, 3, 4]);
        assert!(buf.get(p, 0, 4).is_some());
        buf.invalidate(p);
        assert!(buf.get(p, 0, 4).is_none());
    }

    #[test]
    fn readahead_grows_for_sequential_misses() {
        let buf = ReadAheadBuffer::new();
        let p = Path::new("/large.bin");
        assert_eq!(buf.next_fetch_len(p, 0, 4096), READAHEAD_CHUNK);

        buf.fill_with_request(p, 0, vec![0; READAHEAD_CHUNK], READAHEAD_CHUNK);
        assert_eq!(
            buf.next_fetch_len(p, READAHEAD_CHUNK as u64, 4096),
            READAHEAD_CHUNK * 2
        );

        buf.fill_with_request(
            p,
            READAHEAD_CHUNK as u64,
            vec![0; READAHEAD_CHUNK * 2],
            READAHEAD_CHUNK * 2,
        );
        assert_eq!(
            buf.next_fetch_len(p, (READAHEAD_CHUNK * 3) as u64, 4096),
            READAHEAD_CHUNK * 4
        );
    }

    #[test]
    fn readahead_stays_small_for_random_or_short_reads() {
        let buf = ReadAheadBuffer::new();
        let p = Path::new("/large.bin");
        buf.fill_with_request(p, 0, vec![0; READAHEAD_CHUNK], READAHEAD_CHUNK);
        assert_eq!(buf.next_fetch_len(p, 10_000_000, 4096), READAHEAD_CHUNK);

        buf.fill_with_request(p, 0, vec![0; 1024], READAHEAD_CHUNK);
        assert_eq!(buf.next_fetch_len(p, 1024, 4096), READAHEAD_CHUNK);
    }
}
