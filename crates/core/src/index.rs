//! Индекс в памяти: по сегменту на источник. Имена хранятся одним буфером,
//! записи — компактным массивом, путь восстанавливается по цепочке родителей.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use crate::throttle::{RateLimiter, ThrottledWriter};
use std::path::{Path, PathBuf};

pub const NO_PARENT: u32 = u32::MAX;
const FORMAT_VERSION: u32 = 2;
/// Скорость записи индекса на диск — чтобы не было пиков нагрузки.
const SAVE_BYTES_PER_SEC: u64 = 40 * 1024 * 1024;

pub mod flags {
    pub const DIR: u16 = 1;
    pub const SYMLINK: u16 = 2;
    pub const HIDDEN: u16 = 4;
    pub const SYSTEM: u16 = 8;
    /// Удалён после сканирования (запись остаётся до пересканирования).
    pub const DELETED: u16 = 16;
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub parent: u32,
    pub name_off: u32,
    /// Нижний регистр не храним на диске, пересчитываем при загрузке.
    #[serde(skip)]
    pub lower_off: u32,
    pub name_len: u16,
    #[serde(skip)]
    pub lower_len: u16,
    pub flags: u16,
    pub size: u64,
    /// mtime, секунды Unix. Вместе с size образует content_version.
    pub mtime: i64,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }
    pub fn is_symlink(&self) -> bool {
        self.flags & flags::SYMLINK != 0
    }
    pub fn is_deleted(&self) -> bool {
        self.flags & flags::DELETED != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind {
    LocalWalk,
    NtfsMft,
    /// Объектное хранилище S3; корень вида `s3://имя-подключения`.
    S3,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceInfo {
    pub root: String,
    pub kind: SourceKind,
    pub scanned_at: i64,
    pub scan_ms: u64,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
}

/// Ссылка на запись: сегмент + индекс внутри сегмента.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FileRef {
    pub seg: u16,
    pub idx: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Segment {
    pub info: SourceInfo,
    pub entries: Vec<Entry>,
    names: String,
    #[serde(skip)]
    lower: String,
    /// Дерево «родитель → дети», строится при первом точечном изменении.
    #[serde(skip)]
    children: Option<Children>,
}

/// Дети каждой записи: компактный массив, упорядоченный по родителю,
/// плюс записи, добавленные после построения.
#[derive(Debug, Default)]
pub struct Children {
    /// start[p]..start[p+1] — дети записи p в `list`; слот n — корневые записи.
    start: Vec<u32>,
    list: Vec<u32>,
    added: HashMap<u32, Vec<u32>>,
}

impl Children {
    pub fn build(entries: &[Entry]) -> Self {
        let n = entries.len();
        let slot = |p: u32| if p == NO_PARENT { n } else { p as usize };
        let mut start = vec![0u32; n + 2];
        for e in entries {
            start[slot(e.parent) + 1] += 1;
        }
        for i in 1..start.len() {
            start[i] += start[i - 1];
        }
        let mut fill = start.clone();
        let mut list = vec![0u32; n];
        for (i, e) in entries.iter().enumerate() {
            let s = slot(e.parent);
            list[fill[s] as usize] = i as u32;
            fill[s] += 1;
        }
        Self { start, list, added: HashMap::new() }
    }

    pub fn of(&self, p: u32) -> impl Iterator<Item = u32> + '_ {
        let n = self.start.len() - 2;
        // записи, добавленные после построения, в `start` не входят
        let s = if p == NO_PARENT { Some(n) } else { ((p as usize) < n).then_some(p as usize) };
        let base: &[u32] = match s {
            Some(s) => &self.list[self.start[s] as usize..self.start[s + 1] as usize],
            None => &[],
        };
        base.iter().copied().chain(self.added.get(&p).into_iter().flatten().copied())
    }
}

/// Что произошло с записью при точечном обновлении.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Created,
    Modified,
    Removed,
}

fn is_sep(c: char) -> bool {
    c == '\\' || c == '/'
}

impl Segment {
    /// Строит дерево «родитель → дети», если его ещё нет (нужно для обзора папок).
    pub fn ensure_children(&mut self) {
        if self.children.is_none() {
            self.children = Some(Children::build(&self.entries));
        }
    }

    pub fn has_children_map(&self) -> bool {
        self.children.is_some()
    }

    /// Неудалённые дети папки (`NO_PARENT` — корень). Нужен `ensure_children`.
    pub fn children(&self, dir: u32) -> Vec<u32> {
        let Some(ch) = self.children.as_ref() else { return vec![] };
        self.children_in(ch, dir)
    }

    /// То же по отдельно построенному дереву — когда менять сегмент нельзя.
    pub fn children_in(&self, ch: &Children, dir: u32) -> Vec<u32> {
        ch.of(dir).filter(|&c| (c as usize) < self.entries.len() && !self.entries[c as usize].is_deleted()).collect()
    }

    /// Полный размер и число файлов внутри каждой папки (для файла — он сам).
    /// Родитель всегда записан раньше детей, поэтому хватает одного прохода с конца.
    pub fn subtree_totals(&self) -> (Vec<u64>, Vec<u32>) {
        let n = self.entries.len();
        let mut size = vec![0u64; n];
        let mut count = vec![0u32; n];
        for i in (0..n).rev() {
            let e = &self.entries[i];
            if e.is_deleted() {
                continue;
            }
            if !e.is_dir() {
                size[i] += e.size;
                count[i] += 1;
            }
            if e.parent != NO_PARENT {
                let p = e.parent as usize;
                size[p] += size[i];
                count[p] += count[i];
            }
        }
        (size, count)
    }

    fn find_child(&self, parent: u32, lower: &str) -> Option<u32> {
        let ch = self.children.as_ref()?;
        ch.of(parent).find(|&c| !self.entries[c as usize].is_deleted() && self.lower(c) == lower)
    }

    /// Компоненты пути (в нижнем регистре) относительно корня сегмента.
    fn rel_parts(&self, path: &Path) -> Option<Vec<String>> {
        let root = self.info.root.to_lowercase();
        let p = path.to_string_lossy().to_lowercase();
        let rest = p.strip_prefix(root.trim_end_matches(is_sep))?;
        if !(rest.is_empty() || rest.starts_with(is_sep)) {
            return None;
        }
        Some(rest.split(is_sep).filter(|s| !s.is_empty()).map(|s| s.to_string()).collect())
    }

    fn resolve(&self, parts: &[String]) -> Option<u32> {
        let mut cur = NO_PARENT;
        for part in parts {
            cur = self.find_child(cur, part)?;
        }
        (cur != NO_PARENT).then_some(cur)
    }

    fn push_entry(&mut self, parent: u32, name: &str, fl: u16, size: u64, mtime: i64) -> u32 {
        let name = truncate(name);
        let lower_s = name.to_lowercase();
        let lower = truncate(&lower_s);
        self.entries.push(Entry {
            parent,
            name_off: self.names.len() as u32,
            lower_off: self.lower.len() as u32,
            name_len: name.len() as u16,
            lower_len: lower.len() as u16,
            flags: fl,
            size,
            mtime,
        });
        self.names.push_str(name);
        self.lower.push_str(lower);
        let i = self.entries.len() as u32 - 1;
        if let Some(ch) = self.children.as_mut() {
            ch.added.entry(parent).or_default().push(i);
        }
        i
    }

    fn mark_deleted(&mut self, i: u32, out: &mut Vec<(u32, Change)>) {
        let mut stack = vec![i];
        while let Some(c) = stack.pop() {
            let e = &mut self.entries[c as usize];
            if e.is_deleted() {
                continue;
            }
            e.flags |= flags::DELETED;
            let is_dir = e.is_dir();
            out.push((c, Change::Removed));
            if is_dir {
                if let Some(ch) = self.children.as_ref() {
                    stack.extend(ch.of(c));
                }
            }
        }
    }

    /// Сверяет детей папки `dir` с её актуальным списком (например, из S3):
    /// добавляет новые, обновляет изменённые, помечает пропавшие удалёнными.
    /// Возвращает число изменений.
    pub fn sync_dir(&mut self, dir: u32, live: &[(String, bool, u64, i64)]) -> usize {
        self.ensure_children();
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (name, is_dir, size, mtime) in live {
            let lower = name.to_lowercase();
            match self.find_child(dir, &lower) {
                Some(i) => {
                    seen.insert(i);
                    let e = &mut self.entries[i as usize];
                    if e.is_dir() != *is_dir {
                        self.mark_deleted(i, &mut out);
                        let fl = if *is_dir { flags::DIR } else { 0 };
                        let n = self.push_entry(dir, name, fl, *size, *mtime);
                        seen.insert(n);
                        out.push((n, Change::Created));
                    } else if !is_dir && (e.size != *size || e.mtime != *mtime) {
                        e.size = *size;
                        e.mtime = *mtime;
                        out.push((i, Change::Modified));
                    }
                }
                None => {
                    let fl = if *is_dir { flags::DIR } else { 0 };
                    let n = self.push_entry(dir, name, fl, *size, *mtime);
                    seen.insert(n);
                    out.push((n, Change::Created));
                }
            }
        }
        for c in self.children(dir) {
            if !seen.contains(&c) {
                self.mark_deleted(c, &mut out);
            }
        }
        out.len()
    }

    /// Приводит запись для `path` в соответствие с диском: добавляет, обновляет
    /// или помечает удалённой. Новые папки индексируются вместе с содержимым.
    pub fn apply(&mut self, path: &Path, out: &mut Vec<(u32, Change)>) {
        let Some(parts) = self.rel_parts(path) else { return };
        if parts.is_empty() {
            return;
        }
        self.ensure_children();
        let existing = self.resolve(&parts);
        match (existing, std::fs::symlink_metadata(path)) {
            (Some(i), Err(_)) => self.mark_deleted(i, out),
            (None, Err(_)) => {}
            (Some(i), Ok(md)) => {
                let (fl, size, mtime) = crate::util::entry_meta(&md);
                let e = &mut self.entries[i as usize];
                if (e.flags & flags::DIR) != (fl & flags::DIR) {
                    // файл заменили папкой или наоборот
                    self.mark_deleted(i, out);
                    self.apply(path, out);
                } else if e.size != size || e.mtime != mtime || e.flags != fl {
                    e.size = size;
                    e.mtime = mtime;
                    e.flags = fl;
                    out.push((i, Change::Modified));
                }
            }
            (None, Ok(md)) => {
                let parent = if parts.len() == 1 {
                    NO_PARENT
                } else {
                    match self.resolve(&parts[..parts.len() - 1]) {
                        Some(p) => p,
                        None => {
                            // родительской папки нет в индексе — добавляем её целиком
                            if let Some(dir) = path.parent() {
                                self.apply(dir, out);
                            }
                            return;
                        }
                    }
                };
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let (fl, size, mtime) = crate::util::entry_meta(&md);
                let i = self.push_entry(parent, &name, fl, size, mtime);
                out.push((i, Change::Created));
                if fl & flags::DIR != 0 {
                    self.add_tree(path, i, out);
                }
            }
        }
    }

    fn add_tree(&mut self, dir: &Path, parent: u32, out: &mut Vec<(u32, Change)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for de in rd.flatten() {
            let Ok(md) = de.metadata() else { continue };
            let name = de.file_name().to_string_lossy().into_owned();
            if self.find_child(parent, &name.to_lowercase()).is_some() {
                continue;
            }
            let (fl, size, mtime) = crate::util::entry_meta(&md);
            let i = self.push_entry(parent, &name, fl, size, mtime);
            out.push((i, Change::Created));
            if fl & flags::DIR != 0 {
                self.add_tree(&de.path(), i, out);
            }
        }
    }

    fn rebuild_lower(&mut self) {
        use rayon::prelude::*;
        let lowers: Vec<String> = (0..self.entries.len() as u32)
            .into_par_iter()
            .map(|i| truncate(&self.name(i).to_lowercase()).to_string())
            .collect();
        let mut buf = String::with_capacity(self.names.len() + self.names.len() / 8);
        for (e, l) in self.entries.iter_mut().zip(&lowers) {
            e.lower_off = buf.len() as u32;
            e.lower_len = l.len() as u16;
            buf.push_str(l);
        }
        buf.shrink_to_fit();
        self.lower = buf;
    }

    pub fn name(&self, i: u32) -> &str {
        let e = &self.entries[i as usize];
        &self.names[e.name_off as usize..e.name_off as usize + e.name_len as usize]
    }

    pub fn lower(&self, i: u32) -> &str {
        let e = &self.entries[i as usize];
        &self.lower[e.lower_off as usize..e.lower_off as usize + e.lower_len as usize]
    }

    /// Удалённый источник (S3 и т.п.): файлы нельзя открыть по пути на диске.
    pub fn is_remote(&self) -> bool {
        self.info.root.contains("://")
    }

    pub fn path(&self, i: u32) -> PathBuf {
        if self.is_remote() {
            // у удалённых источников разделитель всегда '/'
            let mut chain = Vec::new();
            let mut cur = i;
            while cur != NO_PARENT {
                chain.push(self.name(cur));
                cur = self.entries[cur as usize].parent;
            }
            chain.reverse();
            return PathBuf::from(format!("{}/{}", self.info.root.trim_end_matches('/'), chain.join("/")));
        }
        let mut chain = Vec::new();
        let mut cur = i;
        while cur != NO_PARENT {
            chain.push(cur);
            cur = self.entries[cur as usize].parent;
        }
        let mut p = PathBuf::from(&self.info.root);
        for &c in chain.iter().rev() {
            p.push(self.name(c));
        }
        p
    }

    pub fn parent_path(&self, i: u32) -> PathBuf {
        match self.entries[i as usize].parent {
            NO_PARENT => PathBuf::from(&self.info.root),
            p => self.path(p),
        }
    }
}

#[derive(Default)]
pub struct SegmentBuilder {
    entries: Vec<Entry>,
    names: String,
    lower: String,
}

impl SegmentBuilder {
    pub fn len(&self) -> u32 {
        self.entries.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(&mut self, parent: u32, name: &str, flags: u16, size: u64, mtime: i64) -> u32 {
        let lower = name.to_lowercase();
        self.push_lowered(parent, name, &lower, flags, size, mtime)
    }

    /// Как push, но нижний регистр уже посчитан (вне блокировки).
    pub fn push_lowered(&mut self, parent: u32, name: &str, lower: &str, flags: u16, size: u64, mtime: i64) -> u32 {
        let name = truncate(name);
        let lower = truncate(lower);
        let e = Entry {
            parent,
            name_off: self.names.len() as u32,
            lower_off: self.lower.len() as u32,
            name_len: name.len() as u16,
            lower_len: lower.len() as u16,
            flags,
            size,
            mtime,
        };
        self.names.push_str(name);
        self.lower.push_str(lower);
        self.entries.push(e);
        self.entries.len() as u32 - 1
    }

    pub fn finish(mut self, info: SourceInfo) -> Segment {
        self.entries.shrink_to_fit();
        self.names.shrink_to_fit();
        self.lower.shrink_to_fit();
        Segment { info, entries: self.entries, names: self.names, lower: self.lower, children: None }
    }
}

/// Имена NTFS ≤ 255 UTF-16 символов, но на всякий случай режем по u16::MAX байт.
fn truncate(s: &str) -> &str {
    if s.len() <= u16::MAX as usize {
        return s;
    }
    let mut end = u16::MAX as usize;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[derive(Default, Serialize, Deserialize)]
pub struct Index {
    pub segments: Vec<Segment>,
}

#[derive(Deserialize)]
struct Stored {
    magic: [u8; 8],
    version: u32,
    index: Index,
}

/// То же, что Stored, но по ссылке: сохраняем, не забирая индекс у читателей.
#[derive(Serialize)]
struct StoredRef<'a> {
    magic: [u8; 8],
    version: u32,
    index: &'a Index,
}

impl Index {
    /// Загружает индекс. Если файл повреждён — пробует резервную копию (`.bak`).
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        match Self::load_file(path) {
            Ok(i) => Ok(i),
            Err(e) => {
                let bak = path.with_extension("bak");
                if bak.exists() {
                    if let Ok(i) = Self::load_file(&bak) {
                        return Ok(i);
                    }
                }
                Err(e)
            }
        }
    }

    fn load_file(path: &Path) -> Result<Self> {
        use bincode::Options;
        let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // Ограничение объёма: повреждённая длина внутри файла даёт ошибку,
        // а не попытку выделить терабайт памяти и аварийное завершение.
        let limit = (f.metadata()?.len().saturating_mul(16)).max(64 << 20);
        let dec = zstd::Decoder::with_buffer(BufReader::with_capacity(1 << 20, f))?;
        let opts = bincode::DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes().with_limit(limit);
        let Ok(mut stored) = opts.deserialize_from::<_, Stored>(dec) else {
            bail!("index format changed or file corrupted, rescan needed");
        };
        if &stored.magic != b"FILECAT\0" || stored.version != FORMAT_VERSION {
            bail!("unsupported index format, rescan needed");
        }
        for seg in &mut stored.index.segments {
            seg.rebuild_lower();
        }
        Ok(stored.index)
    }

    /// Сохраняет индекс: во временный файл, затем подмена. Сохранения идут строго по
    /// одному — два одновременных (например, фоновое и при выходе) иначе испортили бы файл.
    /// Прежний индекс остаётся резервной копией `.bak`.
    pub fn save(&self, path: &Path) -> Result<()> {
        static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let _one_at_a_time = SAVING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp{}-{n}", std::process::id()));
        let stored = StoredRef { magic: *b"FILECAT\0", version: FORMAT_VERSION, index: self };
        {
            // Пишем сжатым потоком, плавно и с фоновым приоритетом I/O.
            let _bg = crate::throttle::BackgroundGuard::enter();
            let limiter = RateLimiter::new(SAVE_BYTES_PER_SEC);
            let file = ThrottledWriter::new(File::create(&tmp)?, &limiter);
            let mut enc = zstd::Encoder::new(BufWriter::with_capacity(1 << 20, file), 3)?;
            bincode::serialize_into(&mut enc, &stored)?;
            enc.finish()?.flush()?;
        }
        if path.exists() {
            let _ = std::fs::rename(path, path.with_extension("bak"));
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Точечно обновляет индекс по изменившемуся пути.
    /// Возвращает затронутые записи; путь вне известных источников игнорируется.
    pub fn apply_change(&mut self, path: &Path) -> Vec<(FileRef, Change)> {
        let p = path.to_string_lossy().to_lowercase();
        // самый длинный подходящий корень: вложенный источник важнее объемлющего
        let best = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                let r = s.info.root.to_lowercase();
                let r = r.trim_end_matches(is_sep);
                p.starts_with(r) && p[r.len()..].starts_with(is_sep)
            })
            .max_by_key(|(_, s)| s.info.root.len())
            .map(|(i, _)| i);
        let Some(si) = best else { return vec![] };
        let mut out = Vec::new();
        self.segments[si].apply(path, &mut out);
        out.into_iter().map(|(idx, c)| (FileRef { seg: si as u16, idx }, c)).collect()
    }

    /// Добавляет сегмент, заменяя прежний с тем же корнем.
    pub fn upsert(&mut self, seg: Segment) {
        let key = seg.info.root.to_lowercase();
        if let Some(old) = self.segments.iter_mut().find(|s| s.info.root.to_lowercase() == key) {
            *old = seg;
        } else {
            self.segments.push(seg);
        }
    }

    pub fn remove(&mut self, root: &str) -> bool {
        let key = root.to_lowercase();
        let before = self.segments.len();
        self.segments.retain(|s| s.info.root.to_lowercase() != key);
        before != self.segments.len()
    }

    pub fn entry(&self, r: FileRef) -> &Entry {
        &self.segments[r.seg as usize].entries[r.idx as usize]
    }

    pub fn name(&self, r: FileRef) -> &str {
        self.segments[r.seg as usize].name(r.idx)
    }

    pub fn lower(&self, r: FileRef) -> &str {
        self.segments[r.seg as usize].lower(r.idx)
    }

    pub fn path(&self, r: FileRef) -> PathBuf {
        self.segments[r.seg as usize].path(r.idx)
    }

    pub fn len(&self) -> usize {
        self.segments.iter().map(|s| s.entries.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{scan_walk, ScanOptions};
    use std::sync::atomic::AtomicBool;

    fn alive(idx: &Index, name: &str) -> bool {
        let s = &idx.segments[0];
        s.entries.iter().enumerate().any(|(i, e)| !e.is_deleted() && s.name(i as u32) == name)
    }

    #[test]
    fn live_changes() {
        let dir = std::env::temp_dir().join(format!("filecat-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("a.txt"), b"1").unwrap();
        let cancel = AtomicBool::new(false);
        let mut index = Index::default();
        index.upsert(scan_walk(&dir, &ScanOptions::default(), &cancel, &|_| {}).unwrap());

        // новый файл
        let b = dir.join("sub").join("b.txt");
        std::fs::write(&b, b"22").unwrap();
        let ch = index.apply_change(&b);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].1, Change::Created);
        assert_eq!(index.path(ch[0].0), b);

        // изменение
        std::fs::write(&b, b"333").unwrap();
        let ch = index.apply_change(&b);
        assert_eq!(ch[0].1, Change::Modified);
        assert_eq!(index.entry(ch[0].0).size, 3);

        // папка, появившаяся целиком (например, перемещённая)
        std::fs::create_dir_all(dir.join("new").join("deep")).unwrap();
        std::fs::write(dir.join("new").join("deep").join("c.txt"), b"x").unwrap();
        index.apply_change(&dir.join("new").join("deep").join("c.txt"));
        assert!(alive(&index, "c.txt"));

        // удаление папки помечает и её содержимое
        std::fs::remove_dir_all(dir.join("sub")).unwrap();
        let ch = index.apply_change(&dir.join("sub"));
        assert_eq!(ch.len(), 3);
        assert!(!alive(&index, "a.txt") && !alive(&index, "b.txt"));

        // сохранение и загрузка с удалёнными записями
        let file = dir.join("index.bin");
        index.save(&file).unwrap();
        let back = Index::load(&file).unwrap();
        assert!(alive(&back, "c.txt") && !alive(&back, "a.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod browse_tests {
    use super::*;
    use crate::scan::{scan_walk, ScanOptions};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn folder_sizes_and_children() {
        let dir = std::env::temp_dir().join(format!("filecat-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a").join("b")).unwrap();
        std::fs::write(dir.join("a").join("x.bin"), [0u8; 100]).unwrap();
        std::fs::write(dir.join("a").join("b").join("y.bin"), [0u8; 50]).unwrap();
        std::fs::write(dir.join("top.txt"), [0u8; 7]).unwrap();
        let cancel = AtomicBool::new(false);
        let mut seg = scan_walk(&dir, &ScanOptions::default(), &cancel, &|_| {}).unwrap();
        seg.ensure_children();
        let top = seg.children(NO_PARENT);
        assert_eq!(top.len(), 2);
        let a = *top.iter().find(|&&i| seg.name(i) == "a").unwrap();
        assert_eq!(seg.children(a).len(), 2);
        let (size, count) = seg.subtree_totals();
        assert_eq!((size[a as usize], count[a as usize]), (150, 2));
        // отдельно построенное дерево даёт то же самое
        let ch = Children::build(&seg.entries);
        assert_eq!(seg.children_in(&ch, a).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    #[test]
    fn sync_dir_adds_updates_removes() {
        let mut b = SegmentBuilder::default();
        let d = b.push(NO_PARENT, "photos", flags::DIR, 0, 0);
        b.push(d, "a.jpg", 0, 10, 1);
        b.push(d, "old.jpg", 0, 20, 1);
        let info = SourceInfo { root: "s3://t".into(), kind: SourceKind::S3, scanned_at: 0, scan_ms: 0, files: 2, dirs: 1, bytes: 30, errors: 0 };
        let mut s = b.finish(info);
        let live = vec![("a.jpg".to_string(), false, 11, 2), ("new.jpg".to_string(), false, 5, 3), ("sub".to_string(), true, 0, 0)];
        assert_eq!(s.sync_dir(d, &live), 4); // изменён, добавлен, добавлена папка, удалён
        let mut names: Vec<_> = s.children(d).into_iter().map(|i| (s.name(i).to_string(), s.entries[i as usize].size)).collect();
        names.sort();
        assert_eq!(names, vec![("a.jpg".into(), 11), ("new.jpg".into(), 5), ("sub".into(), 0)]);
        assert_eq!(s.sync_dir(d, &live), 0); // повтор — без изменений
    }
}

#[cfg(test)]
mod save_tests {
    use super::*;

    fn small() -> Index {
        let mut b = SegmentBuilder::default();
        b.push(NO_PARENT, "a.txt", 0, 1, 1);
        let info = SourceInfo { root: "X:\\".into(), kind: SourceKind::LocalWalk, scanned_at: 0, scan_ms: 0, files: 1, dirs: 0, bytes: 1, errors: 0 };
        let mut i = Index::default();
        i.upsert(b.finish(info));
        i
    }

    #[test]
    fn concurrent_saves_and_corruption_fallback() {
        let dir = std::env::temp_dir().join(format!("filecat-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.bin");
        let idx = small();
        // одновременные сохранения не портят файл
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| idx.save(&path).unwrap());
            }
        });
        assert_eq!(Index::load(&path).unwrap().len(), 1);
        // испорченный файл: загрузка не падает, а берёт резервную копию
        std::fs::write(&path, b"\x28\xb5\x2f\xfd garbage garbage garbage").unwrap();
        assert_eq!(Index::load(&path).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
