//! Сверка индекса с диском после перерыва: что изменилось, пока программа не работала.
//!
//! Слежение за файлами видит изменения, только пока программа запущена. Сверка не
//! сканирует диск заново: она сравнивает время изменения каждой папки с запомненным —
//! когда в папке появляется, пропадает или переименовывается файл, время папки меняется.
//! Перечитываются только такие папки. Файл, изменённый на месте, времени папки не
//! меняет: его размер обновится вместе с папкой или при следующем сканировании.

use super::walk::skip_inside;
use crate::index::{Change, FileRef, Index, Segment, NO_PARENT};
use crate::throttle::RateLimiter;
use crate::util::{entry_meta, mtime_of};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

/// Сколько папок проверять за одно взятие индекса.
const CHUNK: usize = 2000;

/// Сверяет источник `root` с диском. Индекс берётся ненадолго и кусками: окно и
/// слежение за файлами работают всё это время. Источник пересканировали или убрали —
/// сверка прекращается. `limiter` — темп, в папках в секунду.
pub fn catch_up(index: &RwLock<Index>, root: &str, limiter: &RateLimiter) -> Vec<(FileRef, Change)> {
    let mut out = Vec::new();
    let found = {
        let idx = index.read().unwrap();
        idx.segments.iter().position(|s| s.info.root == root).map(|si| (si, idx.segments[si].info.scanned_at, idx.segments[si].dir_ids()))
    };
    let Some((si, scanned_at, dirs)) = found else { return out };
    // диск не подключён: без него сверять не с чем
    if !Path::new(root).is_dir() {
        return out;
    }
    let same = |idx: &Index| idx.segments.get(si).is_some_and(|s: &Segment| s.info.root == root && s.info.scanned_at == scanned_at);

    // папки, у которых сменилось время; корень — всегда (своего времени в индексе у него нет)
    let mut queue = VecDeque::from([NO_PARENT]);
    for chunk in dirs.chunks(CHUNK) {
        let items: Vec<(u32, PathBuf, i64)> = {
            let idx = index.read().unwrap();
            if !same(&*idx) {
                return out;
            }
            let s = &idx.segments[si];
            chunk.iter().filter(|&&d| !s.entries[d as usize].is_deleted()).map(|&d| (d, s.path(d), s.entries[d as usize].mtime)).collect()
        };
        for (d, path, mtime) in items {
            limiter.acquire(1);
            // пропавшую папку заметит сверка её родителя: его время тоже сменилось
            if std::fs::symlink_metadata(&path).is_ok_and(|md| md.is_dir() && mtime_of(&md) != mtime) {
                queue.push_back(d);
            }
        }
    }

    while let Some(d) = queue.pop_front() {
        let path = {
            let idx = index.read().unwrap();
            if !same(&*idx) {
                return out;
            }
            let s = &idx.segments[si];
            if d == NO_PARENT {
                PathBuf::from(root)
            } else if s.entries[d as usize].is_deleted() {
                continue;
            } else {
                s.path(d)
            }
        };
        if skip_inside(&path) {
            continue;
        }
        limiter.acquire(1);
        let Some(live) = list(&path) else { continue };
        let dir_mtime = std::fs::symlink_metadata(&path).map(|md| mtime_of(&md)).unwrap_or(0);
        // запись — только когда индекс свободен: ожидающий писатель на Windows
        // не пускает и читателей, и окно зависло бы
        let new_dirs = loop {
            if let Ok(mut idx) = index.try_write() {
                if !same(&*idx) {
                    return out;
                }
                let mut changes = Vec::new();
                let new_dirs = idx.segments[si].sync_local(d, dir_mtime, &live, &mut changes);
                out.extend(changes.into_iter().map(|(i, c)| (FileRef { seg: si as u16, idx: i }, c)));
                break new_dirs;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        queue.extend(new_dirs);
    }
    out
}

/// Содержимое папки целиком или ничего: если хоть одну запись не прочитать,
/// по неполному списку она сошла бы за удалённую.
fn list(path: &Path) -> Option<Vec<(String, u16, u64, i64)>> {
    let mut live = Vec::new();
    for de in std::fs::read_dir(path).ok()? {
        let de = de.ok()?;
        let (fl, size, mtime) = entry_meta(&de.metadata().ok()?);
        live.push((de.file_name().to_string_lossy().into_owned(), fl, size, mtime));
    }
    Some(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan_walk, ScanOptions};
    use std::sync::atomic::AtomicBool;

    fn find(idx: &Index, name: &str) -> Option<u32> {
        let s = &idx.segments[0];
        (0..s.entries.len() as u32).find(|&i| !s.entries[i as usize].is_deleted() && s.name(i) == name)
    }

    #[test]
    fn catches_up_with_changes_made_while_closed() {
        let dir = std::env::temp_dir().join(format!("filecat-catchup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("keep")).unwrap();
        std::fs::write(dir.join("sub").join("a.txt"), b"1").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), b"1").unwrap();
        std::fs::write(dir.join("keep").join("c.txt"), b"1").unwrap();
        let seg = scan_walk(&dir, &ScanOptions::default(), &AtomicBool::new(false), &|_| {}).unwrap();
        let root = seg.info.root.clone();
        let mut index = Index::default();
        index.upsert(seg);
        let index = RwLock::new(index);

        // время папок — в секундах: изменения должны прийтись на другую секунду
        std::thread::sleep(Duration::from_millis(1100));
        std::fs::remove_file(dir.join("sub").join("a.txt")).unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), b"55555").unwrap();
        std::fs::create_dir_all(dir.join("new").join("deep")).unwrap();
        std::fs::write(dir.join("new").join("deep").join("d.txt"), b"1").unwrap();

        let changes = catch_up(&index, &root, &RateLimiter::new(0));
        {
            let idx = index.read().unwrap();
            assert!(find(&idx, "a.txt").is_none(), "удалённый файл убран");
            assert_eq!(idx.segments[0].entries[find(&idx, "b.txt").unwrap() as usize].size, 5, "размер в изменившейся папке обновлён");
            assert!(find(&idx, "d.txt").is_some(), "новая папка прочитана целиком");
            let c = find(&idx, "c.txt").unwrap();
            assert!(changes.iter().all(|(r, _)| r.idx != c), "нетронутую папку не трогали");
            let totals = idx.segments[0].subtree_totals();
            assert_eq!(totals.1[find(&idx, "sub").unwrap() as usize], 1);
        }
        // повторная сверка без изменений — ничего не меняет
        assert!(catch_up(&index, &root, &RateLimiter::new(0)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
