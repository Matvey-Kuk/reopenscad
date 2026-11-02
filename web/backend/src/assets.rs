//! Mesh assets a design can `import()`: scans and other triangle meshes
//! uploaded next to the single SCAD source.
//!
//! The evaluator never touches storage. The server installs a design's
//! [`AssetSet`] and runs the compile inside [`with_assets`]; the `import()`
//! builtin then resolves file names through [`try_lookup`]. Every cache that keys
//! on the source must also key on [`current_digest`], or replacing `scan.stl`
//! under the same name would serve the old geometry.
//!
//! A set is usually lazy ([`AssetSet::lazy`]): installing it costs nothing,
//! the asset index (names and hashes, no meshes) is read on the first
//! [`try_lookup`] or [`current_digest`], and each mesh is fetched and decoded only
//! when `import()` names it. Routes that never compile therefore never read
//! assets, and compiles read them only after they hold a render permit.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};

/// An indexed triangle mesh in millimetres, wound counter-clockwise seen from
/// outside (the STL convention).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TriMesh {
    pub points: Vec<[f64; 3]>,
    pub triangles: Vec<[u32; 3]>,
}

/// One stored asset: its name as written in `import("...")`, the SHA-256 of
/// its cleaned mesh, and the mesh itself.
#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub sha256: String,
    pub mesh: Arc<TriMesh>,
}

/// How long an uploaded STL is kept, from
/// the moment its upload completes. Nothing renews it.
pub const ASSET_TTL_MS: u64 = 3 * 60 * 60 * 1000;

/// `2026-11-01T14:05:00Z`, from unix milliseconds.
pub fn iso_utc(ms: u64) -> String {
    let seconds = ms / 1000;
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since the epoch (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest / 60 % 60, rest % 60)
}

/// `2h 14m`, `45m`, or `under a minute`.
pub fn time_left(ms: u64) -> String {
    let minutes = ms / 60_000;
    match (minutes / 60, minutes % 60) {
        (0, 0) => "under a minute".to_string(),
        (0, m) => format!("{m}m"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// Why an `import()` of an expired STL fails. A write that keeps the import is
/// rejected with this too, so it tells the agent both ways out.
pub fn expired_import_message(file: &str, expired_at: u64) -> String {
    format!(
        "import(\"{file}\") failed: this STL expired at {} (uploaded STLs are kept for 3 hours, with no renewal) and is \
         deleted. Remove the import from the code, or upload the file again with upload_asset or create_asset_upload.",
        iso_utc(expired_at)
    )
}

/// Why a tool cannot find asset `name` in the current set: it expired, or
/// there never was one.
pub fn no_asset_message(name: &str) -> String {
    match expired_at(name) {
        Some(at) => format!(
            "{name} expired at {} (uploaded STLs are kept for 3 hours, with no renewal) and is deleted. \
             Upload the file again with upload_asset or create_asset_upload.",
            iso_utc(at)
        ),
        None => format!("This design has no asset {name:?}; list_assets shows its assets."),
    }
}

/// Where a lazy [`AssetSet`] reads from.
pub trait AssetSource: Send + Sync {
    /// `(name, sha256)` of every asset. Metadata only; called at most once
    /// per set.
    fn index(&self) -> Result<Vec<(String, String)>, String>;
    /// The mesh of asset `name` whose content hash is `sha256`. An error
    /// (storage failure, or the asset changed since [`AssetSource::index`])
    /// fails the compile rather than dropping the asset.
    fn mesh(&self, name: &str, sha256: &str) -> Result<Arc<TriMesh>, String>;
    /// `(name, when)` of assets that expired. Read right after
    /// [`AssetSource::index`], from the same read.
    fn expired(&self) -> Vec<(String, u64)> {
        Vec::new()
    }
}

struct Index {
    /// name -> sha256
    shas: BTreeMap<String, String>,
    /// name -> when it expired
    expired: BTreeMap<String, u64>,
    digest: String,
}

/// The assets visible to one compile.
#[derive(Clone)]
pub struct AssetSet {
    source: Option<Arc<dyn AssetSource>>,
    index: Arc<OnceLock<Result<Index, String>>>,
    loaded: Arc<Mutex<BTreeMap<String, Arc<Asset>>>>,
    /// Digest reported when the index cannot be read: unique, so no cache
    /// entry is ever written or hit under it.
    unavailable: String,
}

impl fmt::Debug for AssetSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssetSet")
            .field("lazy", &self.source.is_some())
            .field("resolved", &self.is_resolved())
            .finish()
    }
}

impl Default for AssetSet {
    fn default() -> Self {
        Self::new([])
    }
}

fn digest_of<'a>(pairs: impl Iterator<Item = (&'a String, &'a String)>) -> String {
    pairs
        .map(|(name, sha)| format!("{name}={sha}"))
        .collect::<Vec<_>>()
        .join(";")
}

/// [`digest_of`] plus a `!name` entry per expired asset: an expired import
/// fails to compile while a never-uploaded one only warns, so the two must
/// never share a cache entry.
fn digest_with_expired(shas: &BTreeMap<String, String>, expired: &BTreeMap<String, u64>) -> String {
    let mut digest = digest_of(shas.iter());
    for name in expired.keys().filter(|name| !shas.contains_key(*name)) {
        if !digest.is_empty() {
            digest.push(';');
        }
        digest.push('!');
        digest.push_str(name);
    }
    digest
}

fn unavailable_digest() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "unavailable:{}:{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

impl AssetSet {
    /// A set of meshes already in memory.
    pub fn new(assets: impl IntoIterator<Item = Asset>) -> Self {
        let assets: BTreeMap<String, Arc<Asset>> = assets
            .into_iter()
            .map(|asset| (asset.name.clone(), Arc::new(asset)))
            .collect();
        let shas: BTreeMap<String, String> = assets
            .values()
            .map(|asset| (asset.name.clone(), asset.sha256.clone()))
            .collect();
        let digest = digest_of(shas.iter());
        let index = OnceLock::new();
        let _ = index.set(Ok(Index { shas, expired: BTreeMap::new(), digest }));
        Self {
            source: None,
            index: Arc::new(index),
            loaded: Arc::new(Mutex::new(assets)),
            unavailable: String::new(),
        }
    }

    /// A set that reads `source` only when a compile first asks for it.
    pub fn lazy(source: impl AssetSource + 'static) -> Self {
        Self {
            source: Some(Arc::new(source)),
            index: Arc::new(OnceLock::new()),
            loaded: Arc::new(Mutex::new(BTreeMap::new())),
            unavailable: unavailable_digest(),
        }
    }

    /// Whether the index has been read (always true for an in-memory set).
    pub fn is_resolved(&self) -> bool {
        self.index.get().is_some()
    }

    fn resolved_index(&self) -> &Result<Index, String> {
        self.index.get_or_init(|| {
            let source = self.source.as_ref().expect("an eager set is always resolved");
            source.index().map(|pairs| {
                let shas: BTreeMap<String, String> = pairs.into_iter().collect();
                let expired: BTreeMap<String, u64> = source.expired().into_iter().collect();
                let digest = digest_with_expired(&shas, &expired);
                Index { shas, expired, digest }
            })
        })
    }

    /// The asset named `name`: `Ok(None)` when the design has none of that
    /// name, `Err` when it has one that cannot be read right now.
    pub fn try_get(&self, name: &str) -> Result<Option<Arc<Asset>>, String> {
        let index = match self.resolved_index() {
            Ok(index) => index,
            Err(error) => return Err(format!("The design's assets could not be read: {error}")),
        };
        let Some(sha) = index.shas.get(name) else {
            return Ok(None);
        };
        if let Some(asset) = self.loaded.lock().ok().and_then(|loaded| loaded.get(name).cloned()) {
            return Ok(Some(asset));
        }
        let Some(source) = self.source.as_ref() else {
            return Ok(None);
        };
        let mesh = source.mesh(name, sha)?;
        let asset = Arc::new(Asset { name: name.to_string(), sha256: sha.clone(), mesh });
        if let Ok(mut loaded) = self.loaded.lock() {
            loaded.insert(name.to_string(), Arc::clone(&asset));
        }
        Ok(Some(asset))
    }

    /// When asset `name` expired, if it did and has not been uploaded again.
    pub fn expired_at(&self, name: &str) -> Option<u64> {
        let index = self.resolved_index().as_ref().ok()?;
        if index.shas.contains_key(name) {
            return None;
        }
        index.expired.get(name).copied()
    }

    /// `name=sha;name=sha` in name order; empty for no assets, so a design
    /// without assets keeps the cache keys it always had. When the index
    /// cannot be read, a digest unique to this set, so nothing is cached.
    pub fn digest(&self) -> &str {
        match self.resolved_index() {
            Ok(index) => &index.digest,
            Err(_) => &self.unavailable,
        }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<AssetSet>>> = const { RefCell::new(None) };
}

/// Runs `f` with `set` as the assets `import()` resolves against on this
/// thread, restoring the previous set afterwards (also on panic).
pub fn with_assets<R>(set: Option<Arc<AssetSet>>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<AssetSet>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            CURRENT.with(|current| *current.borrow_mut() = previous);
        }
    }
    let previous = CURRENT.with(|current| std::mem::replace(&mut *current.borrow_mut(), set));
    let _restore = Restore(previous);
    f()
}

fn current() -> Option<Arc<AssetSet>> {
    CURRENT.with(|current| current.borrow().clone())
}

/// The asset named `name` in the current set: `Ok(None)` when there is no
/// such asset, `Err` when it exists but cannot be read.
pub fn try_lookup(name: &str) -> Result<Option<Arc<Asset>>, String> {
    match current() {
        Some(set) => set.try_get(name),
        None => Ok(None),
    }
}

/// When asset `name` of the current set expired, if it did.
pub fn expired_at(name: &str) -> Option<u64> {
    current().and_then(|set| set.expired_at(name))
}

/// The current set's digest, empty when there is none.
pub fn current_digest() -> String {
    current().map(|set| set.digest().to_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn asset(name: &str, sha: &str) -> Asset {
        Asset {
            name: name.into(),
            sha256: sha.into(),
            mesh: Arc::new(TriMesh::default()),
        }
    }

    #[test]
    fn lookup_is_scoped_to_with_assets() {
        assert!(try_lookup("a.stl").unwrap().is_none());
        let set = Arc::new(AssetSet::new([asset("b.stl", "2"), asset("a.stl", "1")]));
        with_assets(Some(set), || {
            assert_eq!(try_lookup("a.stl").unwrap().unwrap().sha256, "1");
            assert_eq!(current_digest(), "a.stl=1;b.stl=2");
        });
        assert!(try_lookup("a.stl").unwrap().is_none());
        assert_eq!(current_digest(), "");
    }

    struct Counting {
        index_reads: Arc<AtomicUsize>,
        mesh_reads: Arc<AtomicUsize>,
        fail_mesh: bool,
    }

    impl AssetSource for Counting {
        fn index(&self) -> Result<Vec<(String, String)>, String> {
            self.index_reads.fetch_add(1, Ordering::SeqCst);
            Ok(vec![("b.stl".into(), "2".into()), ("a.stl".into(), "1".into())])
        }
        fn mesh(&self, _name: &str, _sha256: &str) -> Result<Arc<TriMesh>, String> {
            self.mesh_reads.fetch_add(1, Ordering::SeqCst);
            if self.fail_mesh {
                Err("storage is down".into())
            } else {
                Ok(Arc::new(TriMesh::default()))
            }
        }
    }

    #[test]
    fn a_lazy_set_reads_nothing_until_asked_and_each_mesh_once() {
        let (index_reads, mesh_reads) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let set = Arc::new(AssetSet::lazy(Counting {
            index_reads: Arc::clone(&index_reads),
            mesh_reads: Arc::clone(&mesh_reads),
            fail_mesh: false,
        }));
        with_assets(Some(Arc::clone(&set)), || {});
        assert_eq!(index_reads.load(Ordering::SeqCst), 0);
        assert!(!set.is_resolved());
        with_assets(Some(Arc::clone(&set)), || {
            assert_eq!(current_digest(), "a.stl=1;b.stl=2");
            assert_eq!(mesh_reads.load(Ordering::SeqCst), 0, "the digest reads no mesh");
            assert_eq!(try_lookup("a.stl").unwrap().unwrap().sha256, "1");
            assert_eq!(try_lookup("a.stl").unwrap().unwrap().sha256, "1");
            assert!(try_lookup("missing.stl").unwrap().is_none());
        });
        assert_eq!(index_reads.load(Ordering::SeqCst), 1);
        assert_eq!(mesh_reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_unreadable_asset_is_an_error_not_a_missing_file() {
        let set = Arc::new(AssetSet::lazy(Counting {
            index_reads: Arc::default(),
            mesh_reads: Arc::default(),
            fail_mesh: true,
        }));
        with_assets(Some(set), || {
            assert!(try_lookup("a.stl").unwrap_err().contains("storage is down"));
            assert!(try_lookup("nope.stl").unwrap().is_none());
        });
    }
}
