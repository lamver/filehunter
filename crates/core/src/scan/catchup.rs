//! Сверка индекса с диском: что изменилось, пока программа не работала.
//!
//! Слежение за файлами видит изменения, только пока программа запущена. Сверка не
//! сканирует диск заново: она сравнивает время изменения каждой папки с запомненным —
//! когда в папке появляется, пропадает или переименовывается файл, время папки меняется.
//! Перечитываются только такие папки, и каждая исправляется сразу, как найдена.
//! Файл, изменённый на месте, времени папки не меняет: его размер обновится вместе
//! с папкой или при следующем сканировании.

use super::walk::skip_inside;
use crate::index::{Change, FileRef, Index, NO_PARENT};
use crate::throttle::RateLimiter;
use crate::util::{entry_meta, mtime_of};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::Duration;

/// Сколько папок проверять за одно взятие индекса.
const CHUNK: usize = 2000;

/// Изменения, найденные сверкой: приходят пачками по мере работы.
pub type Report<'a> = dyn FnMut(Vec<(FileRef, Change)>) + 'a;

/// Сверяет с диском источник `root` целиком или папку `under` со всем, что внутри.
/// Сначала перечитывается сама папка (корень), затем проверяются вложенные.
///
/// Индекс берётся ненадолго и кусками: окно и слежение за файлами работают всё это
/// время. `limiter` — темп в папках в секунду; `stop` — прекратить (открыли другую
/// папку). Возвращает true, если сверка дошла до конца: не остановлена и источник
/// за это время не пересканировали и не убрали.
pub fn catch_up(index: &RwLock<Index>, root: &str, under: Option<u32>, limiter: &RateLimiter, stop: &dyn Fn() -> bool, report: &mut Report) -> bool {
    let found = {
        let idx = index.read().unwrap();
        idx.segments.iter().position(|s| s.info.root == root).map(|si| {
            let s = &idx.segments[si];
            let dirs = match under {
                Some(d) => s.dir_ids_under(d),
                None => s.dir_ids(),
            };
            (si, s.info.scanned_at, dirs)
        })
    };
    let Some((si, scanned_at, dirs)) = found else { return false };
    // диск не подключён: без него сверять не с чем
    if !Path::new(root).is_dir() {
        return false;
    }
    let run = Run { index, root, si, scanned_at, limiter, stop };

    let mut queue = VecDeque::from([under.unwrap_or(NO_PARENT)]);
    if !run.drain(&mut queue, report) {
        return false;
    }
    for chunk in dirs.chunks(CHUNK) {
        let items: Vec<(u32, PathBuf, i64)> = {
            let idx = index.read().unwrap();
            if !run.same(&idx) {
                return false;
            }
            let s = &idx.segments[si];
            chunk.iter().filter(|&&d| !s.entries[d as usize].is_deleted()).map(|&d| (d, s.path(d), s.entries[d as usize].mtime)).collect()
        };
        for (d, path, mtime) in items {
            if stop() {
                return false;
            }
            limiter.acquire(1);
            // пропавшую папку заметит сверка её родителя: его время тоже сменилось
            if std::fs::symlink_metadata(&path).is_ok_and(|md| md.is_dir() && mtime_of(&md) != mtime) {
                queue.push_back(d);
            }
        }
        if !run.drain(&mut queue, report) {
            return false;
        }
    }
    true
}

struct Run<'a> {
    index: &'a RwLock<Index>,
    root: &'a str,
    si: usize,
    scanned_at: i64,
    limiter: &'a RateLimiter,
    stop: &'a dyn Fn() -> bool,
}

impl Run<'_> {
    /// Источник тот же, что в начале сверки.
    fn same(&self, idx: &Index) -> bool {
        idx.segments.get(self.si).is_some_and(|s| s.info.root == self.root && s.info.scanned_at == self.scanned_at)
    }

    /// Перечитывает папки из очереди вместе с новыми, найденными в них.
    /// false — остановили или источник сменился.
    fn drain(&self, queue: &mut VecDeque<u32>, report: &mut Report) -> bool {
        let mut out = Vec::new();
        let mut ok = true;
        while let Some(d) = queue.pop_front() {
            if (self.stop)() {
                ok = false;
                break;
            }
            let path = {
                let idx = self.index.read().unwrap();
                if !self.same(&idx) {
                    ok = false;
                    break;
                }
                let s = &idx.segments[self.si];
                if d == NO_PARENT {
                    PathBuf::from(self.root)
                } else if s.entries[d as usize].is_deleted() {
                    continue;
                } else {
                    s.path(d)
                }
            };
            if skip_inside(&path) {
                continue;
            }
            self.limiter.acquire(1);
            let Some(live) = list(&path) else { continue };
            let dir_mtime = std::fs::symlink_metadata(&path).map(|md| mtime_of(&md)).unwrap_or(0);
            // запись — только когда индекс свободен: ожидающий писатель на Windows
            // не пускает и читателей, и окно зависло бы
            let new_dirs = loop {
                if let Ok(mut idx) = self.index.try_write() {
                    if !self.same(&idx) {
                        ok = false;
                        queue.clear();
                        break Vec::new();
                    }
                    let mut changes = Vec::new();
                    let new_dirs = idx.segments[self.si].sync_local(d, dir_mtime, &live, &mut changes);
                    out.extend(changes.into_iter().map(|(i, c)| (FileRef { seg: self.si as u16, idx: i }, c)));
                    break new_dirs;
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            queue.extend(new_dirs);
        }
        if !out.is_empty() {
            report(out);
        }
        ok
    }
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

    fn run(index: &RwLock<Index>, root: &str, under: Option<u32>) -> (bool, Vec<(FileRef, Change)>) {
        let mut all = Vec::new();
        let done = catch_up(index, root, under, &RateLimiter::new(0), &|| false, &mut |ch| all.extend(ch));
        (done, all)
    }

    fn setup(name: &str) -> (PathBuf, String, RwLock<Index>) {
        let dir = std::env::temp_dir().join(format!("filecat-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub").join("inner")).unwrap();
        std::fs::create_dir_all(dir.join("keep")).unwrap();
        std::fs::write(dir.join("sub").join("a.txt"), b"1").unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), b"1").unwrap();
        std::fs::write(dir.join("sub").join("inner").join("e.txt"), b"1").unwrap();
        std::fs::write(dir.join("keep").join("c.txt"), b"1").unwrap();
        let seg = scan_walk(&dir, &ScanOptions::default(), &AtomicBool::new(false), &|_| {}).unwrap();
        let root = seg.info.root.clone();
        let mut index = Index::default();
        index.upsert(seg);
        // время папок — в секундах: изменения должны прийтись на другую секунду
        std::thread::sleep(Duration::from_millis(1100));
        (dir, root, RwLock::new(index))
    }

    #[test]
    fn catches_up_with_changes_made_while_closed() {
        let (dir, root, index) = setup("catchup");
        std::fs::remove_file(dir.join("sub").join("a.txt")).unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), b"55555").unwrap();
        std::fs::create_dir_all(dir.join("new").join("deep")).unwrap();
        std::fs::write(dir.join("new").join("deep").join("d.txt"), b"1").unwrap();

        let (done, changes) = run(&index, &root, None);
        assert!(done);
        {
            let idx = index.read().unwrap();
            assert!(find(&idx, "a.txt").is_none(), "удалённый файл убран");
            assert_eq!(idx.segments[0].entries[find(&idx, "b.txt").unwrap() as usize].size, 5, "размер в изменившейся папке обновлён");
            assert!(find(&idx, "d.txt").is_some(), "новая папка прочитана целиком");
            let c = find(&idx, "c.txt").unwrap();
            assert!(changes.iter().all(|(r, _)| r.idx != c), "нетронутую папку не трогали");
            let totals = idx.segments[0].subtree_totals();
            assert_eq!(totals.1[find(&idx, "sub").unwrap() as usize], 2);
        }
        // повторная сверка без изменений — ничего не меняет
        assert!(run(&index, &root, None).1.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checks_only_inside_the_opened_folder() {
        let (dir, root, index) = setup("catchup-under");
        std::fs::remove_file(dir.join("sub").join("inner").join("e.txt")).unwrap();
        std::fs::remove_file(dir.join("keep").join("c.txt")).unwrap();
        let sub = find(&index.read().unwrap(), "sub").unwrap();

        let (done, _) = run(&index, &root, Some(sub));
        assert!(done);
        let idx = index.read().unwrap();
        assert!(find(&idx, "e.txt").is_none(), "глубже открытой папки — проверено");
        assert!(find(&idx, "c.txt").is_some(), "вне открытой папки — не тронуто");
        drop(idx);

        // остановка: открыли другую папку
        let stopped = catch_up(&index, &root, None, &RateLimiter::new(0), &|| true, &mut |_| {});
        assert!(!stopped);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
