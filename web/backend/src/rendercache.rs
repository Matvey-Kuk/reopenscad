//! Persistent cache of design render responses.
//!
//! Opening a design used to recompile it every time: the browser posts
//! `/api/designs/{id}/render` right after loading, and the engine's in-process
//! memo (`engine::compile_parts_shared`, two entries) is cold after a restart,
//! on another Cloud Run instance, or after anyone else's render. This stores
//! the finished response body next to the design instead, keyed by a SHA-256
//! of everything the response depends on, so an unchanged revision is served
//! from storage without taking an evaluator slot.
//!
//! The engine memo stays as the first level; this is the second. Entries are
//! scoped to a design, so evicting the design evicts its renders, and bounded
//! three ways: per entry ([`MAX_ENTRY_BYTES`]), per design
//! ([`ENTRIES_PER_DESIGN`], least recently used out) and in total
//! ([`MAX_TOTAL_BYTES`], least recently used out across all designs).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::SystemTime;

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use serde_json::Value;

/// Part of every key, so changing the engine invalidates every stored render.
/// Bump the suffix on any change that alters geometry or the response shape;
/// the package version covers releases.
pub const ENGINE_VERSION: &str = concat!("reopenscad-engine/", env!("CARGO_PKG_VERSION"), "/r4");

/// Largest response body stored. Bigger renders are served, not kept: a few
/// huge models must not push every other design's render out.
pub const MAX_ENTRY_BYTES: usize = 16 * 1024 * 1024;
/// Renders kept per design, most recently used first. Covers the current
/// revision in both modes plus a step or two of undo.
pub const ENTRIES_PER_DESIGN: usize = 4;
/// Ceiling on all stored renders together.
pub const MAX_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// Cache hits read, splice and send up to [`MAX_ENTRY_BYTES`] without an
/// evaluator slot, so they get a budget of their own: at most this many are
/// held in memory at once. A hit that finds none free waits in the ordinary
/// render queue instead and is served under that permit.
pub const MAX_HITS_IN_FLIGHT: usize = 4;

static HITS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// One slot of [`MAX_HITS_IN_FLIGHT`], released on drop.
pub struct HitPermit(());

impl HitPermit {
    pub fn try_acquire() -> Option<Self> {
        HITS_IN_FLIGHT
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |held| {
                (held < MAX_HITS_IN_FLIGHT).then_some(held + 1)
            })
            .ok()
            .map(|_| Self(()))
    }
}

impl Drop for HitPermit {
    fn drop(&mut self) {
        HITS_IN_FLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A stored entry: `u32` big-endian length of the object-summary JSON, that
/// JSON, then the response JSON object. Laid out so a hit parses only the
/// small summaries and splices the per-request fields onto the response
/// bytes rather than re-parsing and re-serialising megabytes of meshes.
pub fn encode_entry(summaries: &Value, response: &Value) -> Option<Vec<u8>> {
    let summaries = serde_json::to_vec(summaries).ok()?;
    let length = u32::try_from(summaries.len()).ok()?;
    let mut entry = Vec::new();
    entry.extend_from_slice(&length.to_be_bytes());
    entry.extend_from_slice(&summaries);
    serde_json::to_writer(&mut entry, response).ok()?;
    (entry.last() == Some(&b'}')).then_some(entry)
}

/// Splits an entry into its summaries and the response JSON object, reusing
/// the entry's buffer for the latter. The response is returned without its
/// closing brace, ready for [`close_response`]. `None` for a malformed entry.
pub fn decode_entry(mut entry: Vec<u8>) -> Option<(Value, Vec<u8>)> {
    let length = u32::from_be_bytes(entry.get(..4)?.try_into().ok()?) as usize;
    let summaries = serde_json::from_slice(entry.get(4..4usize.checked_add(length)?)?).ok()?;
    entry.drain(..4 + length);
    if entry.first() != Some(&b'{') || entry.pop() != Some(b'}') {
        return None;
    }
    Some((summaries, entry))
}

/// Appends `fields` (keys absent from the stored object) and the closing
/// brace to an open response from [`decode_entry`].
pub fn close_response(mut open: Vec<u8>, fields: &serde_json::Map<String, Value>) -> Vec<u8> {
    for (key, value) in fields {
        if open.len() > 1 {
            open.push(b',');
        }
        let _ = serde_json::to_writer(&mut open, key);
        open.push(b':');
        let _ = serde_json::to_writer(&mut open, value);
    }
    open.push(b'}');
    open
}

/// The key for one render: a hex SHA-256 over the engine version and every
/// input the response is a function of. Fields are length-prefixed, so no two
/// distinct inputs can serialise to the same byte string.
///
/// The mesh assets `import()` resolves against are one of those inputs, and
/// they are read from [`crate::assets::current_digest`]: call this inside the
/// same `assets::with_assets` scope the compile runs in. Outside one (or for
/// a design without assets) the digest is empty and the key is exactly what it
/// was before assets existed. [`render_key_with_assets`] takes the digest
/// explicitly instead.
pub fn render_key(
    mode: &str,
    code: &str,
    effective_source: &str,
    overrides: &Value,
    object_ids: Option<&[String]>,
) -> String {
    render_key_with_assets(
        mode,
        code,
        effective_source,
        overrides,
        object_ids,
        &crate::assets::current_digest(),
    )
}

/// [`render_key`] with the asset digest (`AssetSet::digest`) given rather
/// than read from the current scope. An empty digest adds no field, so keys of
/// asset-free designs are unchanged and survive the upgrade.
pub fn render_key_with_assets(
    mode: &str,
    code: &str,
    effective_source: &str,
    overrides: &Value,
    object_ids: Option<&[String]>,
    asset_digest: &str,
) -> String {
    let object_ids = object_ids.map(|ids| serde_json::to_string(ids).unwrap_or_default());
    let overrides = overrides.to_string();
    let mut fields: Vec<&[u8]> = vec![
        ENGINE_VERSION.as_bytes(),
        mode.as_bytes(),
        code.as_bytes(),
        effective_source.as_bytes(),
        overrides.as_bytes(),
        object_ids.as_deref().unwrap_or("\u{0}all").as_bytes(),
    ];
    if !asset_digest.is_empty() {
        fields.push(asset_digest.as_bytes());
    }
    let mut message = Vec::with_capacity(fields.iter().map(|field| field.len() + 8).sum());
    for field in fields {
        message.extend_from_slice(&(field.len() as u64).to_be_bytes());
        message.extend_from_slice(field);
    }
    sha256(&message).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A key this module produced: 64 lowercase hex digits. Checked before a key
/// becomes part of a path.
pub fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// SHA-256 (FIPS 180-4). Hand-rolled because the crate has no hashing
/// dependency and this is the only use; `DefaultHasher` is not stable across
/// Rust releases, which a persisted key needs.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (index, word) in block.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7) ^ w[index - 15].rotate_right(18) ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17) ^ w[index - 2].rotate_right(19) ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0u8; 32];
    for (chunk, word) in digest.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    digest
}

// ---------------------------------------------------------------------------
// Filesystem backend
// ---------------------------------------------------------------------------

/// Renders on disk as `<root>/<design id>/<key>.json`. The modification time
/// is the last-used stamp: a hit touches it, and eviction removes the oldest.
///
/// Lives in a subdirectory of the design store's root, which the store's
/// boot scan skips (it indexes files only). Every operation is a plain file
/// operation, so concurrent requests need no lock: a racing eviction turns
/// a hit into a miss, never into a torn read (writes are temp file + rename).
#[derive(Clone, Debug)]
pub struct FsRenderCache {
    root: PathBuf,
    max_total_bytes: u64,
}

static TEMP_NONCE: AtomicU64 = AtomicU64::new(0);

impl FsRenderCache {
    pub fn new(root: PathBuf) -> Self {
        Self::with_total_cap(root, MAX_TOTAL_BYTES)
    }

    pub fn with_total_cap(root: PathBuf, max_total_bytes: u64) -> Self {
        Self { root, max_total_bytes }
    }

    fn design_dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn entry_path(&self, id: &str, key: &str) -> PathBuf {
        self.design_dir(id).join(format!("{key}.json"))
    }

    /// The stored body for `(id, key)`, marking it used.
    pub fn get(&self, id: &str, key: &str) -> Option<Vec<u8>> {
        if !crate::valid_workspace_id(id) || !valid_key(key) {
            return None;
        }
        let path = self.entry_path(id, key);
        let bytes = fs::read(&path).ok()?;
        touch(&path);
        Some(bytes)
    }

    /// Stores `bytes` for `(id, key)` and enforces the bounds. Best effort:
    /// a full disk or a vanished design is a missed cache, not an error.
    pub fn put(&self, id: &str, key: &str, bytes: &[u8]) {
        if !crate::valid_workspace_id(id) || !valid_key(key) || bytes.len() > MAX_ENTRY_BYTES {
            return;
        }
        let directory = self.design_dir(id);
        if fs::create_dir_all(&directory).is_err() {
            return;
        }
        let temporary = directory.join(format!(
            ".{key}-{}-{}.tmp",
            std::process::id(),
            TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| -> io::Result<()> {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            fs::rename(&temporary, self.entry_path(id, key))
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
            return;
        }
        self.trim_design(id);
        self.trim_total();
    }

    /// Drops every render of `id`; called when the design is evicted.
    pub fn remove_design(&self, id: &str) {
        if crate::valid_workspace_id(id) {
            let _ = fs::remove_dir_all(self.design_dir(id));
        }
    }

    /// Removes render directories of designs `exists` does not know, and
    /// temporary files a crash left behind. Run once at boot.
    pub fn sweep(&self, exists: impl Fn(&str) -> bool) {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !path.is_dir() {
                continue;
            }
            if !crate::valid_workspace_id(&name) || !exists(&name) {
                let _ = fs::remove_dir_all(&path);
                continue;
            }
            for (file, _, _) in entries_of(&path) {
                if file.file_name().is_some_and(|name| name.to_string_lossy().ends_with(".tmp")) {
                    let _ = fs::remove_file(file);
                }
            }
        }
    }

    fn trim_design(&self, id: &str) {
        let mut entries: Vec<_> = entries_of(&self.design_dir(id))
            .into_iter()
            .filter(|(path, _, _)| is_entry(path))
            .collect();
        if entries.len() <= ENTRIES_PER_DESIGN {
            return;
        }
        entries.sort_by(|a, b| b.1.cmp(&a.1));
        for (path, _, _) in entries.into_iter().skip(ENTRIES_PER_DESIGN) {
            let _ = fs::remove_file(path);
        }
    }

    fn trim_total(&self) {
        let Ok(designs) = fs::read_dir(&self.root) else {
            return;
        };
        let mut entries: Vec<(PathBuf, SystemTime, u64)> = designs
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .flat_map(|path| entries_of(&path))
            .filter(|(path, _, _)| is_entry(path))
            .collect();
        let mut total: u64 = entries.iter().map(|(_, _, size)| size).sum();
        if total <= self.max_total_bytes {
            return;
        }
        entries.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, _, size) in entries {
            if total <= self.max_total_bytes {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
            }
        }
    }
}

fn is_entry(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "json")
}

/// `(path, modified, size)` of every file in `directory`.
fn entries_of(directory: &Path) -> Vec<(PathBuf, SystemTime, u64)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    entry.path(),
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    metadata.len(),
                )
            })
        })
        .collect()
}

/// Marks an entry used by moving its modification time to now.
fn touch(path: &Path) {
    if let Ok(file) = File::options().append(true).open(path) {
        let _ = file.set_modified(SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    fn temp_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "reopenscad-render-cache-{name}-{}-{}",
            std::process::id(),
            TEMP_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        root
    }

    fn design_id(index: usize) -> String {
        format!("brave-otter-{index:04}-cachetest")
    }

    fn key(index: usize) -> String {
        render_key("preview", &format!("cube({index});"), "", &json!([]), None)
    }

    #[test]
    fn asset_digest_keys_only_designs_that_have_assets() {
        let plain = key(1);
        assert_eq!(
            plain,
            render_key_with_assets("preview", "cube(1);", "", &json!([]), None, "")
        );
        let with_scan = render_key_with_assets(
            "preview",
            "cube(1);",
            "",
            &json!([]),
            None,
            "scan.stl=aaaa",
        );
        let replaced = render_key_with_assets(
            "preview",
            "cube(1);",
            "",
            &json!([]),
            None,
            "scan.stl=bbbb",
        );
        assert!(valid_key(&with_scan));
        assert_ne!(plain, with_scan);
        assert_ne!(with_scan, replaced);

        let mesh = std::sync::Arc::new(crate::assets::TriMesh::default());
        let set = crate::assets::AssetSet::new([crate::assets::Asset {
            name: "scan.stl".into(),
            sha256: "aaaa".into(),
            mesh,
        }]);
        let scoped = crate::assets::with_assets(Some(std::sync::Arc::new(set)), || key(1));
        assert_eq!(scoped, with_scan);
    }

    /// Sets an entry's last-used stamp, so ordering does not depend on the
    /// filesystem's timestamp resolution.
    fn age(cache: &FsRenderCache, id: &str, key: &str, seconds_ago: u64) {
        let file = File::options().append(true).open(cache.entry_path(id, key)).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(seconds_ago)).unwrap();
    }

    #[test]
    fn sha256_matches_the_reference_vectors() {
        let hex = |data: &[u8]| -> String { sha256(data).iter().map(|b| format!("{b:02x}")).collect() };
        assert_eq!(hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn entries_round_trip_and_splice_request_fields() {
        let response = json!({"ok": true, "mesh": "AAAA", "objects": [{"id": "o1"}]});
        let summaries = json!([{"id": "o1"}]);
        let entry = encode_entry(&summaries, &response).unwrap();
        let (decoded, open) = decode_entry(entry).unwrap();
        assert_eq!(decoded, summaries);
        let mut fields = serde_json::Map::new();
        fields.insert("revision".into(), json!(7));
        fields.insert("cached".into(), json!(true));
        let body: Value = serde_json::from_slice(&close_response(open, &fields)).unwrap();
        assert_eq!(body["mesh"], "AAAA");
        assert_eq!(body["revision"], 7);
        assert_eq!(body["cached"], true);
        let (_, empty) = decode_entry(encode_entry(&json!(null), &json!({})).unwrap()).unwrap();
        assert_eq!(close_response(empty, &fields), br#"{"cached":true,"revision":7}"#.to_vec());
        for malformed in [vec![], vec![0, 0, 0, 9, b'{'], b"\0\0\0\x02[]x".to_vec()] {
            assert!(decode_entry(malformed).is_none());
        }
    }

    #[test]
    fn hit_permits_are_bounded() {
        let held: Vec<_> = std::iter::from_fn(HitPermit::try_acquire).take(MAX_HITS_IN_FLIGHT + 1).collect();
        assert!(held.len() <= MAX_HITS_IN_FLIGHT);
        drop(held);
        assert!(HitPermit::try_acquire().is_some());
    }

    #[test]
    fn keys_separate_every_input() {
        let base = render_key("preview", "cube(1);", "cube(1);", &json!([]), None);
        assert!(valid_key(&base));
        assert_eq!(base, render_key("preview", "cube(1);", "cube(1);", &json!([]), None));
        for other in [
            render_key("render", "cube(1);", "cube(1);", &json!([]), None),
            render_key("preview", "cube(2);", "cube(1);", &json!([]), None),
            render_key("preview", "cube(1);", "cube(2);", &json!([]), None),
            render_key("preview", "cube(1);", "cube(1);", &json!([{"name": "a"}]), None),
            render_key("preview", "cube(1);", "cube(1);", &json!([]), Some(&[])),
            render_key("preview", "cube(1);", "cube(1);", &json!([]), Some(&["o1".into()])),
            // Length prefixes: moving a byte between fields changes the key.
            render_key("preview", "cube(1);c", "ube(1);", &json!([]), None),
        ] {
            assert_ne!(base, other);
        }
        assert!(!valid_key("../../etc/passwd"));
        assert!(!valid_key(&base.to_uppercase()));
    }

    #[test]
    fn a_stored_render_round_trips_and_misses_on_other_keys() {
        let root = temp_root("roundtrip");
        let cache = FsRenderCache::new(root.clone());
        let id = design_id(1);
        assert!(cache.get(&id, &key(1)).is_none());
        cache.put(&id, &key(1), b"{\"ok\":true}");
        assert_eq!(cache.get(&id, &key(1)).as_deref(), Some(&b"{\"ok\":true}"[..]));
        assert!(cache.get(&id, &key(2)).is_none());
        assert!(cache.get(&design_id(2), &key(1)).is_none());
        // Never a path built from an unvalidated key or id.
        cache.put(&id, "../escape", b"x");
        cache.put("../escape", &key(1), b"x");
        assert!(!root.join("escape").exists());
        assert!(!root.parent().unwrap().join("escape").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_entries_are_not_stored() {
        let root = temp_root("oversize");
        let cache = FsRenderCache::new(root.clone());
        let id = design_id(1);
        cache.put(&id, &key(1), &vec![b' '; MAX_ENTRY_BYTES + 1]);
        assert!(cache.get(&id, &key(1)).is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn each_design_keeps_only_its_most_recently_used_renders() {
        let root = temp_root("per-design");
        let cache = FsRenderCache::new(root.clone());
        let id = design_id(1);
        for index in 0..ENTRIES_PER_DESIGN {
            cache.put(&id, &key(index), b"{}");
            age(&cache, &id, &key(index), 100 - index as u64);
        }
        // Using the oldest makes it the newest, so the next put evicts key(1).
        assert!(cache.get(&id, &key(0)).is_some());
        cache.put(&id, &key(99), b"{}");
        assert!(cache.get(&id, &key(0)).is_some());
        assert!(cache.get(&id, &key(1)).is_none(), "least recently used must go");
        assert!(cache.get(&id, &key(99)).is_some());
        let stored = entries_of(&cache.design_dir(&id)).len();
        assert_eq!(stored, ENTRIES_PER_DESIGN);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_total_cap_evicts_least_recently_used_across_designs() {
        let root = temp_root("total");
        let cache = FsRenderCache::with_total_cap(root.clone(), 250);
        for index in 0..3 {
            cache.put(&design_id(index), &key(index), &[b' '; 100]);
            age(&cache, &design_id(index), &key(index), 100 - index as u64);
        }
        assert!(cache.get(&design_id(0), &key(0)).is_none(), "oldest must be evicted");
        assert!(cache.get(&design_id(1), &key(1)).is_some());
        assert!(cache.get(&design_id(2), &key(2)).is_some());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn removing_or_sweeping_a_design_drops_its_renders() {
        let root = temp_root("remove");
        let cache = FsRenderCache::new(root.clone());
        cache.put(&design_id(1), &key(1), b"{}");
        cache.put(&design_id(2), &key(2), b"{}");
        cache.put(&design_id(3), &key(3), b"{}");
        fs::write(cache.design_dir(&design_id(3)).join(".stale.tmp"), b"x").unwrap();
        cache.remove_design(&design_id(1));
        assert!(cache.get(&design_id(1), &key(1)).is_none());
        assert!(!cache.design_dir(&design_id(1)).exists());
        let keep = design_id(3);
        cache.sweep(|id| id == keep);
        assert!(cache.get(&design_id(2), &key(2)).is_none());
        assert!(cache.get(&design_id(3), &key(3)).is_some());
        assert!(!cache.design_dir(&design_id(3)).join(".stale.tmp").exists());
        let _ = fs::remove_dir_all(root);
    }
}
