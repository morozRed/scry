//! Sizes kept in the store between readings. FSEvents says which folders changed since each
//! was measured, and only those are walked again; a size is never patched from an event.
//! A folder replaced by another (a new inode), dropped events, an event log that was reset
//! or an event counter that went backwards make the size stale too.
//!
//! A single file, such as Docker's disk, is read again with `lstat` every time: it grows in
//! place, and the events name only its folder.
//!
//! A hard-linked file is counted in the first part a walk meets it in. A part walked alone
//! could count it again while another part keeps a cached size that counts it, so when a
//! walk meets a hard-linked file with a link outside what it walked, or a part walked again
//! held one before, every part is walked again together.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;

use super::fsevents;
use crate::paths::Paths;
use crate::protected::file_id;
use crate::size::{SubtreeSize, Walker, measure_file};
use crate::store::{SizeRow, Store};

/// How long the FSEvents history may take before every size counts as stale.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(5);
/// A folder with more entries than this is measured in parts from then on. A browser's
/// cache or a build's output changes all the time, and rereading 200,000 entries for each
/// change took most of a warm reading on the reference Mac.
pub const SPLIT_ABOVE: u64 = 20_000;
/// A folder with more files of its own than [`SPLIT_ABOVE`] cannot be cut further, and a
/// browser rewrites its cache folder all the time: 150,000 files there took 11 s to read on
/// the reference Mac. Such a folder is read again at most this often.
pub const HOLD_FLAT_MS: i64 = 10 * 60 * 1000;

/// One measured unit: a folder, or with `top` only the files directly in it, whose folders
/// are parts of their own.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Part {
    pub path: PathBuf,
    pub top: bool,
}

impl Part {
    pub fn whole(path: PathBuf) -> Part {
        Part { path, top: false }
    }
}

/// What the cache held for each part, and whether it must be measured again.
#[derive(Clone, Debug, Default)]
pub struct Look {
    pub cached: Vec<Option<SubtreeSize>>,
    /// Measured again by `measure`.
    pub stale: Vec<bool>,
    /// Changed, but held back: a folder with tens of thousands of files of its own, read
    /// again at most every [`HOLD_FLAT_MS`]. Its cached size shows meanwhile.
    pub held: Vec<bool>,
    pub stats: CacheStats,
}

impl Look {
    /// Parts whose size shown is not up to date: stale or held back.
    pub fn changed(&self) -> Vec<bool> {
        self.stale
            .iter()
            .zip(&self.held)
            .map(|(s, h)| *s || *h)
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct CacheStats {
    pub parts: usize,
    /// Parts whose cached size no change touched.
    pub fresh: usize,
    /// Parts that changed but were held back; see [`Look::held`].
    pub held: usize,
    /// Parts walked again.
    pub measured: usize,
    /// FSEvents lost events, its log was reset, or its counter went backwards: everything
    /// was measured.
    pub lost: bool,
    /// A part walked again shares hard-linked files with parts that were not, so every part
    /// was walked again together.
    pub relinked: bool,
    pub history_ms: f64,
    pub walk_ms: f64,
}

/// The cache over the store. Without a store (it could not be opened), every size is
/// measured each time.
pub struct SizeCache {
    store: Option<Store>,
    split_above: u64,
}

impl SizeCache {
    pub fn new(store: Option<Store>) -> SizeCache {
        SizeCache {
            store,
            split_above: SPLIT_ABOVE,
        }
    }

    /// The store at its usual place, created if need be.
    pub fn open(paths: &Paths) -> SizeCache {
        let store = paths
            .ensure_state()
            .and_then(|()| Store::open(&paths.db()))
            .ok();
        SizeCache::new(store)
    }

    /// Stores folders with more than `n` entries in parts; [`SPLIT_ABOVE`] by default.
    pub fn split_above(mut self, n: u64) -> SizeCache {
        self.split_above = n;
        self
    }

    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// Folders the cache holds in parts one folder down: those a walk found to have more than
    /// [`SPLIT_ABOVE`] entries. A layout cuts them the same way, so a change deep inside one
    /// rereads that part alone.
    pub fn split(&self) -> HashSet<PathBuf> {
        let Some(store) = &self.store else {
            return HashSet::new();
        };
        store
            .split_paths()
            .unwrap_or_default()
            .into_iter()
            .collect()
    }

    /// The cached size of each part and whether it is stale. Parts that no change touched
    /// are marked good up to now, so the next look reads only the history after this one.
    pub fn look(&self, parts: &[Part], home: &Path) -> Look {
        let start = Instant::now();
        let n = parts.len();
        let mut look = Look {
            cached: vec![None; n],
            stale: vec![true; n],
            held: vec![false; n],
            stats: CacheStats {
                parts: n,
                ..CacheStats::default()
            },
        };
        let Some(store) = &self.store else {
            return look;
        };
        let Some(dev) = std::fs::metadata(home).ok().map(|m| m.dev() as i32) else {
            return look;
        };
        let Some(uuid) = fsevents::log_uuid(dev) else {
            return look;
        };
        if store.size_log().ok().flatten().as_deref() != Some(uuid.as_str()) {
            look.stats.lost = store.size_log().ok().flatten().is_some();
            if store.reset_sizes(&uuid).is_err() {
                return look;
            }
        }
        // Paths compare by component, so `x` and an older Scry's `x/` are one key: the row
        // checked last wins.
        let mut rows: HashMap<(PathBuf, bool), SizeRow> = HashMap::new();
        for r in store.size_rows().unwrap_or_default() {
            let key = (r.path.clone(), r.top);
            if rows.get(&key).is_none_or(|old| old.event_id < r.event_id) {
                rows.insert(key, r);
            }
        }

        // A part is a candidate when its row is for the same folder on the logged volume.
        let mut since = vec![u64::MAX; n];
        let mut measured = vec![0; n];
        let mut own_entries = vec![0; n];
        let mut stale_now = vec![false; n];
        for (i, p) in parts.iter().enumerate() {
            let Some(row) = rows.remove(&(p.path.clone(), p.top)) else {
                continue;
            };
            let meta = std::fs::symlink_metadata(&p.path);
            let same = meta
                .as_ref()
                .is_ok_and(|m| file_id(m) == row.id && m.dev() as i32 == dev && !m.is_file());
            if same {
                since[i] = row.event_id;
                stale_now[i] = row.dirty;
            }
            measured[i] = row.measured_ms;
            own_entries[i] = row.size.files;
            look.cached[i] = Some(row.size);
        }
        let candidates: Vec<usize> = (0..n).filter(|&i| since[i] != u64::MAX).collect();
        if candidates.is_empty() {
            look.stats.history_ms = ms(start);
            return look;
        }
        // A row stamped after the counter's present comes from a log that went back, as on a
        // restored volume; nothing it says can be trusted.
        let present = fsevents::current_id();
        if candidates.iter().any(|&i| since[i] > present) {
            look.stats.lost = true;
            look.stats.history_ms = ms(start);
            return look;
        }
        let oldest = candidates.iter().map(|&i| since[i]).min().unwrap_or(0);
        let watch = outermost(candidates.iter().map(|&i| parts[i].path.as_path()));
        let history = fsevents::since(&watch, oldest, HISTORY_TIMEOUT);
        look.stats.history_ms = ms(start);
        if history.lost {
            look.stats.lost = true;
            return look;
        }
        let mut by_path: HashMap<(&Path, bool), usize> = HashMap::new();
        for &i in &candidates {
            by_path.insert((parts[i].path.as_path(), parts[i].top), i);
        }
        for (x, id) in &history.changed {
            if let Some(&i) = by_path.get(&(x.as_path(), true))
                && since[i] < *id
            {
                stale_now[i] = true;
            }
            for a in x.ancestors() {
                if let Some(&i) = by_path.get(&(a, false))
                    && since[i] < *id
                {
                    stale_now[i] = true;
                }
            }
        }
        for (x, id) in &history.rescan {
            for &i in &candidates {
                let p = &parts[i].path;
                let hit = if parts[i].top {
                    x == p || p.starts_with(x)
                } else {
                    x.starts_with(p) || p.starts_with(x)
                };
                if hit && since[i] < *id {
                    stale_now[i] = true;
                }
            }
        }
        let now = crate::sys::now_ms();
        let mut advance = Vec::new();
        for &i in &candidates {
            if !stale_now[i] {
                look.stale[i] = false;
                look.stats.fresh += 1;
            } else if parts[i].top
                && own_entries[i] > self.split_above
                && now - measured[i] < HOLD_FLAT_MS
            {
                look.stale[i] = false;
                look.held[i] = true;
                look.stats.held += 1;
            }
            // Every row moves on, so the next look reads a short history; one that changed
            // keeps that as its dirty mark until it is measured.
            advance.push((parts[i].path.as_path(), parts[i].top, stale_now[i]));
        }
        let _ = store.advance_size_rows(&advance, history.upto);
        look
    }

    /// Measures the stale parts in one walk and stores what it read. `exclude` lists folders
    /// that belong to something else, such as another item inside a split one. Returns a
    /// size for every part: the cached one where it is fresh.
    pub fn measure(
        &self,
        parts: &[Part],
        look: &Look,
        home: &Path,
        exclude: &[PathBuf],
    ) -> (Vec<SubtreeSize>, CacheStats) {
        let start = Instant::now();
        let mut stats = look.stats;
        // Stamped before the walk, so a change made while it runs is seen by the next look.
        let event_id = fsevents::current_id();
        let now = crate::sys::now_ms();
        let mut out: Vec<SubtreeSize> = look
            .cached
            .iter()
            .map(|c| c.clone().unwrap_or_default())
            .collect();
        let mut rows = Vec::new();
        let is_file =
            |i: usize| std::fs::symlink_metadata(&parts[i].path).is_ok_and(|m| m.is_file());
        let (files, mut folders): (Vec<usize>, Vec<usize>) = (0..parts.len())
            .filter(|&i| look.stale[i])
            .partition(|&i| is_file(i));
        for &i in &files {
            out[i] = measure_file(&parts[i].path);
            let id = std::fs::symlink_metadata(&parts[i].path).ok();
            if let (None, Some(m)) = (&out[i].error, id) {
                rows.push(SizeRow {
                    path: parts[i].path.clone(),
                    top: parts[i].top,
                    id: file_id(&m),
                    size: out[i].clone(),
                    event_id,
                    measured_ms: now,
                    dirty: false,
                });
            }
        }
        let held_links = folders
            .iter()
            .any(|&i| look.cached[i].as_ref().is_some_and(|c| c.shared_links));
        let mut walked = self.walk(parts, &folders, home, exclude);
        let met_links = walked.iter().any(|(_, s)| s.shared_links);
        if !folders.is_empty() && (held_links || met_links) {
            let every: Vec<usize> = (0..parts.len()).filter(|&i| !is_file(i)).collect();
            if every.len() > folders.len() {
                folders = every;
                walked = self.walk(parts, &folders, home, exclude);
                stats.relinked = true;
            }
        }
        for (i, size) in walked {
            let big = size.files + size.dirs > self.split_above;
            if size.error.is_none() && big && !parts[i].top {
                // The same walk, in parts: stored so the next layout measures them apart.
                for p in &size.parts {
                    // Joined with an empty part, a path gains a trailing slash, and the store
                    // would key it apart from the part itself.
                    let path = if p.rel.as_os_str().is_empty() {
                        parts[i].path.clone()
                    } else {
                        parts[i].path.join(&p.rel)
                    };
                    if let Ok(m) = std::fs::symlink_metadata(&path) {
                        rows.push(SizeRow {
                            path,
                            top: p.top,
                            id: file_id(&m),
                            size: p.size.clone(),
                            event_id,
                            measured_ms: now,
                            dirty: false,
                        });
                    }
                }
            }
            let id = std::fs::symlink_metadata(&parts[i].path)
                .ok()
                .map(|m| file_id(&m));
            if let (None, Some(id)) = (&size.error, id) {
                rows.push(SizeRow {
                    path: parts[i].path.clone(),
                    top: parts[i].top,
                    id,
                    size: size.clone(),
                    event_id,
                    measured_ms: now,
                    dirty: false,
                });
            }
            out[i] = size;
        }
        if let Some(store) = &self.store {
            let _ = store.put_size_rows(&rows);
        }
        stats.measured = files.len() + folders.len();
        stats.walk_ms = ms(start);
        (out, stats)
    }

    /// The folder parts `which` measured in one walk, with one record of hard links.
    fn walk(
        &self,
        parts: &[Part],
        which: &[usize],
        home: &Path,
        exclude: &[PathBuf],
    ) -> Vec<(usize, SubtreeSize)> {
        if which.is_empty() {
            return Vec::new();
        }
        let folders: Vec<PathBuf> = which.iter().map(|&i| parts[i].path.clone()).collect();
        // Every folder that is a part of its own, so a `top` part reads only its files and
        // no folder is counted twice. A walk still enters its own root.
        let mut fence: Vec<PathBuf> = parts.iter().map(|p| p.path.clone()).collect();
        fence.extend(exclude.iter().cloned());
        let sizes = Walker::new(home).exclude(fence).parts(1).measure(&folders);
        which.iter().copied().zip(sizes).collect()
    }
}

/// The paths with no other path above them, which is all FSEvents needs to watch.
fn outermost<'a>(paths: impl Iterator<Item = &'a Path>) -> Vec<PathBuf> {
    let mut v: Vec<&Path> = paths.collect();
    v.sort();
    v.dedup();
    let mut out: Vec<PathBuf> = Vec::new();
    for p in v {
        if !out.last().is_some_and(|o| p.starts_with(o)) {
            out.push(p.to_path_buf());
        }
    }
    out
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn cache(dir: &Path) -> SizeCache {
        SizeCache::new(Some(Store::open(&dir.join("scry.db")).unwrap()))
    }

    /// Looks until the history shows `want` stale parts, since fseventsd takes in a write a
    /// moment after it happens.
    fn look_until(c: &SizeCache, parts: &[Part], home: &Path, want: &[bool]) -> Look {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let l = c.look(parts, home);
            if l.stale == want || Instant::now() > deadline {
                return l;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    #[test]
    fn a_change_makes_only_its_part_stale_and_the_rescan_reads_the_new_size() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        let caches = home.join("Caches");
        fs::create_dir_all(caches.join("a")).unwrap();
        fs::create_dir_all(caches.join("b")).unwrap();
        fs::write(caches.join("a/f"), vec![1u8; 64 * 1024]).unwrap();
        fs::write(caches.join("b/f"), vec![2u8; 64 * 1024]).unwrap();
        fs::write(caches.join("loose"), vec![3u8; 8 * 1024]).unwrap();
        let parts = vec![
            Part::whole(caches.join("a")),
            Part::whole(caches.join("b")),
            Part {
                path: caches.clone(),
                top: true,
            },
        ];
        let c = cache(state.path());

        let first = c.look(&parts, &home);
        assert_eq!(first.stale, [true, true, true], "nothing cached yet");
        let (sizes, stats) = c.measure(&parts, &first, &home, &[]);
        assert_eq!(stats.measured, 3);
        assert_eq!(sizes[2].files, 1, "the top part holds only the loose file");
        let b_before = sizes[1].size.allocated;

        let warm = look_until(&c, &parts, &home, &[false, false, false]);
        assert_eq!(warm.stale, [false, false, false]);
        assert_eq!(warm.cached[1].as_ref().unwrap().size.allocated, b_before);

        fs::write(caches.join("b/g"), vec![4u8; 256 * 1024]).unwrap();
        let changed = look_until(&c, &parts, &home, &[false, true, false]);
        assert_eq!(changed.stale, [false, true, false], "only b changed");
        let (sizes, stats) = c.measure(&parts, &changed, &home, &[]);
        assert_eq!(stats.measured, 1);
        assert!(
            sizes[1].size.allocated >= b_before + 256 * 1024,
            "{sizes:?}"
        );
        assert_eq!(sizes[1].files, 2);

        // A loose file in the split folder makes its top part stale, not its folders.
        fs::write(caches.join("loose2"), b"x").unwrap();
        let top = look_until(&c, &parts, &home, &[false, false, true]);
        assert_eq!(top.stale, [false, false, true]);
    }

    #[test]
    fn a_big_folder_is_stored_in_parts_and_then_rereads_only_the_part_that_changed() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        let big = home.join("big");
        for d in ["a", "b", "c"] {
            for i in 0..4 {
                fs::create_dir_all(big.join(d)).unwrap();
                fs::write(big.join(d).join(i.to_string()), vec![1u8; 4096]).unwrap();
            }
        }
        let c = cache(state.path()).split_above(10);
        let whole = vec![Part::whole(big.clone())];
        let l = c.look(&whole, &home);
        let (first, _) = c.measure(&whole, &l, &home, &[]);
        assert_eq!(c.split(), HashSet::from([big.clone()]));

        // The next layout cuts it as the walk did; every part is already known.
        let parts = vec![
            Part::whole(big.join("a")),
            Part::whole(big.join("b")),
            Part::whole(big.join("c")),
            Part {
                path: big.clone(),
                top: true,
            },
        ];
        let l = look_until(&c, &parts, &home, &[false; 4]);
        assert_eq!(l.stale, [false; 4]);
        let sum: u64 = l.cached.iter().map(|s| s.as_ref().unwrap().files).sum();
        assert_eq!(sum, first[0].files);

        fs::write(big.join("b/new"), b"x").unwrap();
        let l = look_until(&c, &parts, &home, &[false, true, false, false]);
        assert_eq!(l.stale, [false, true, false, false]);
        let (sizes, stats) = c.measure(&parts, &l, &home, &[]);
        assert_eq!(stats.measured, 1);
        assert_eq!(sizes[1].files, 5);
    }

    #[test]
    fn a_huge_flat_folder_that_changed_is_held_back_and_its_change_remembered() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        let flat = home.join("flat");
        fs::create_dir_all(&flat).unwrap();
        for i in 0..12 {
            fs::write(flat.join(i.to_string()), b"x").unwrap();
        }
        let c = cache(state.path()).split_above(10);
        let parts = vec![Part {
            path: flat.clone(),
            top: true,
        }];
        let l = c.look(&parts, &home);
        c.measure(&parts, &l, &home, &[]);
        fs::write(flat.join("new"), b"x").unwrap();
        let l = look_until(&c, &parts, &home, &[false]);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut l = l;
        while !l.held[0] && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            l = c.look(&parts, &home);
        }
        assert!(l.held[0] && !l.stale[0], "{l:?}");
        assert_eq!(l.changed(), [true]);
        let (sizes, stats) = c.measure(&parts, &l, &home, &[]);
        assert_eq!(stats.measured, 0);
        assert_eq!(sizes[0].files, 12, "the last size read");
        // The change is remembered without the history: the row is dirty.
        let row = &c.store().unwrap().size_rows().unwrap()[0];
        assert!(row.dirty);
        assert!(c.look(&parts, &home).held[0]);
    }

    #[test]
    fn a_folder_swapped_for_another_is_stale_without_any_event() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.join("x")).unwrap();
        let parts = vec![Part::whole(home.join("x"))];
        let c = cache(state.path());
        let l = c.look(&parts, &home);
        c.measure(&parts, &l, &home, &[]);
        fs::rename(home.join("x"), home.join("old")).unwrap();
        fs::create_dir_all(home.join("x")).unwrap();
        assert_eq!(c.look(&parts, &home).stale, [true]);
    }

    #[test]
    fn a_reset_event_log_forgets_every_size() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.join("x")).unwrap();
        let parts = vec![Part::whole(home.join("x"))];
        let c = cache(state.path());
        let l = c.look(&parts, &home);
        c.measure(&parts, &l, &home, &[]);
        let store = c.store().unwrap();
        assert_eq!(store.size_rows().unwrap().len(), 1);
        store
            .reset_sizes("00000000-0000-0000-0000-000000000000")
            .unwrap();
        let l = c.look(&parts, &home);
        assert_eq!(l.stale, [true]);
        assert!(l.stats.lost, "a different log reads as lost history");
        assert!(l.cached[0].is_none());
    }

    fn write_mib(path: &Path, seed: u8) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Varied bytes, so nothing stores it sparse or compressed.
        let data: Vec<u8> = (0..1 << 20)
            .map(|i: u32| (i as u8).wrapping_mul(13) ^ seed)
            .collect();
        fs::write(path, data).unwrap();
    }

    #[test]
    fn a_part_walked_again_alone_never_counts_a_hard_link_twice() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        let caches = home.join("Caches");
        write_mib(&caches.join("a/big"), 1);
        fs::create_dir_all(caches.join("b")).unwrap();
        fs::hard_link(caches.join("a/big"), caches.join("b/link")).unwrap();
        let parts = vec![Part::whole(caches.join("a")), Part::whole(caches.join("b"))];
        let c = cache(state.path());
        let l = c.look(&parts, &home);
        let (cold, _) = c.measure(&parts, &l, &home, &[]);
        let total = |s: &[SubtreeSize]| s.iter().map(|x| x.size.allocated).sum::<u64>();
        let first = total(&cold);
        assert!(
            (1 << 20..1 << 21).contains(&first),
            "counted once: {cold:?}"
        );
        assert!(cold.iter().all(|s| s.shared_links));

        // Only the part the link was not counted in changes.
        let zero = cold
            .iter()
            .position(|s| s.size.allocated < 1 << 19)
            .unwrap();
        fs::write(parts[zero].path.join("small"), b"x").unwrap();
        let mut want = [false, false];
        want[zero] = true;
        let l = look_until(&c, &parts, &home, &want);
        assert_eq!(l.stale, want);
        let (warm, stats) = c.measure(&parts, &l, &home, &[]);
        assert!(stats.relinked, "{stats:?}");
        assert_eq!(stats.measured, 2);
        let second = total(&warm);
        assert!(
            second < first + (1 << 19),
            "still counted once: {first} then {second}"
        );
    }

    #[test]
    fn a_single_file_is_read_again_on_every_look() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        let raw = home.join("vms/0/data/Docker.raw");
        fs::create_dir_all(raw.parent().unwrap()).unwrap();
        fs::write(&raw, vec![1u8; 64 * 1024]).unwrap();
        let parts = vec![Part::whole(raw.clone())];
        let c = cache(state.path());
        let l = c.look(&parts, &home);
        let (before, _) = c.measure(&parts, &l, &home, &[]);
        // Grown in place, the same inode: an event would name only its folder.
        let mut f = fs::OpenOptions::new().append(true).open(&raw).unwrap();
        std::io::Write::write_all(&mut f, &vec![2u8; 1 << 20]).unwrap();
        drop(f);
        let l = c.look(&parts, &home);
        assert_eq!(l.stale, [true], "never trusted from the cache");
        assert!(l.cached[0].is_some(), "the last size shows meanwhile");
        let (after, _) = c.measure(&parts, &l, &home, &[]);
        assert!(
            after[0].size.allocated >= before[0].size.allocated + (1 << 20),
            "{before:?} then {after:?}"
        );
    }

    #[test]
    fn an_event_counter_that_went_backwards_makes_every_size_stale() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.join("x")).unwrap();
        let parts = vec![Part::whole(home.join("x"))];
        let c = cache(state.path());
        let l = c.look(&parts, &home);
        c.measure(&parts, &l, &home, &[]);
        let store = c.store().unwrap();
        // As if the volume came back from a backup: the rows are ahead of the counter.
        let mut rows = store.size_rows().unwrap();
        rows[0].event_id = fsevents::current_id() + 1_000_000_000;
        store.put_size_rows(&rows).unwrap();
        let l = c.look(&parts, &home);
        assert_eq!(l.stale, [true]);
        assert!(l.stats.lost);
    }

    #[test]
    fn without_a_store_everything_is_measured() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().canonicalize().unwrap();
        fs::write(home.join("f"), b"x").unwrap();
        let parts = vec![Part::whole(home.clone())];
        let c = SizeCache::new(None);
        let l = c.look(&parts, &home);
        assert_eq!(l.stale, [true]);
        let (sizes, _) = c.measure(&parts, &l, &home, &[]);
        assert_eq!(sizes[0].files, 1);
    }

    #[test]
    fn outermost_keeps_only_paths_with_nothing_above_them() {
        let ps = [
            Path::new("/a/b"),
            Path::new("/a"),
            Path::new("/c/d"),
            Path::new("/c/de"),
            Path::new("/a/b/c"),
        ];
        assert_eq!(
            outermost(ps.into_iter()),
            [PathBuf::from("/a"), "/c/d".into(), "/c/de".into()]
        );
    }
}
