//! Поиск по именам. Синтаксис запроса:
//!   слова        — все должны входить в имя (без учёта регистра)
//!   *.mp4, a?c   — маска по всему имени
//!   ext:pdf,docx — расширение
//!   size:>100mb  size:<1k  size:10mb..1gb
//!   type:file | type:dir
//!   path:work    — подстрока в полном пути
//!   src:C:       — источник (корень)
//!   -слово       — исключить

use crate::index::{FileRef, Index};
use crate::util::parse_size;
use anyhow::{bail, Result};
use memchr::memmem::Finder;
use rayon::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortBy {
    #[default]
    None,
    Name,
    Size,
    Mtime,
}

enum Term {
    Sub(Finder<'static>),
    Glob(Vec<char>),
}

impl Term {
    fn matches(&self, lower: &str) -> bool {
        match self {
            Term::Sub(f) => f.find(lower.as_bytes()).is_some(),
            Term::Glob(p) => glob_match(p, &lower.chars().collect::<Vec<_>>()),
        }
    }
}

#[derive(Default)]
pub struct Query {
    terms: Vec<Term>,
    exclude: Vec<Finder<'static>>,
    exts: Vec<String>,
    min_size: Option<u64>,
    max_size: Option<u64>,
    kind: Option<Kind>,
    path_terms: Vec<String>,
    source: Option<String>,
}

impl Query {
    pub fn parse(q: &str) -> Result<Self> {
        let mut out = Query::default();
        for tok in q.split_whitespace() {
            let lower = tok.to_lowercase();
            if let Some((key, val)) = lower.split_once(':') {
                match key {
                    "ext" => {
                        out.exts.extend(
                            val.split(',').filter(|s| !s.is_empty()).map(|s| format!(".{}", s.trim_start_matches('.'))),
                        );
                        continue;
                    }
                    "size" => {
                        parse_size_filter(val, &mut out)?;
                        continue;
                    }
                    "type" => {
                        out.kind = match val {
                            "file" | "f" => Some(Kind::File),
                            "dir" | "d" | "folder" => Some(Kind::Dir),
                            _ => bail!("type: file или dir"),
                        };
                        continue;
                    }
                    "path" => {
                        out.path_terms.push(val.to_string());
                        continue;
                    }
                    "src" => {
                        out.source = Some(val.to_string());
                        continue;
                    }
                    // "c:\..." и прочее — обычное слово
                    _ => {}
                }
            }
            if let Some(neg) = lower.strip_prefix('-').filter(|s| !s.is_empty()) {
                out.exclude.push(Finder::new(neg.as_bytes()).into_owned());
            } else if lower.contains(['*', '?']) {
                out.terms.push(Term::Glob(lower.chars().collect()));
            } else {
                out.terms.push(Term::Sub(Finder::new(lower.as_bytes()).into_owned()));
            }
        }
        Ok(out)
    }

    fn matches_entry(&self, lower: &str, e: &crate::Entry) -> bool {
        if e.is_deleted() {
            return false;
        }
        let is_dir = e.is_dir();
        match self.kind {
            Some(Kind::File) if is_dir => return false,
            Some(Kind::Dir) if !is_dir => return false,
            _ => {}
        }
        if self.min_size.is_some() || self.max_size.is_some() {
            if is_dir {
                return false;
            }
            if self.min_size.is_some_and(|m| e.size < m) || self.max_size.is_some_and(|m| e.size > m) {
                return false;
            }
        }
        if !self.exts.is_empty() && !self.exts.iter().any(|x| lower.ends_with(x.as_str())) {
            return false;
        }
        if self.exclude.iter().any(|f| f.find(lower.as_bytes()).is_some()) {
            return false;
        }
        self.terms.iter().all(|t| t.matches(lower))
    }
}

fn parse_size_filter(val: &str, q: &mut Query) -> Result<()> {
    let bad = || anyhow::anyhow!("size: ожидается >10mb, <1g или 10mb..1gb");
    if let Some((a, b)) = val.split_once("..") {
        q.min_size = Some(parse_size(a).ok_or_else(bad)?);
        q.max_size = Some(parse_size(b).ok_or_else(bad)?);
    } else if let Some(v) = val.strip_prefix(">=").or_else(|| val.strip_prefix('>')) {
        q.min_size = Some(parse_size(v).ok_or_else(bad)?);
    } else if let Some(v) = val.strip_prefix("<=").or_else(|| val.strip_prefix('<')) {
        q.max_size = Some(parse_size(v).ok_or_else(bad)?);
    } else {
        let v = parse_size(val).ok_or_else(bad)?;
        q.min_size = Some(v);
        q.max_size = Some(v);
    }
    Ok(())
}

/// Маска с * и ? по всему имени.
fn glob_match(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

pub struct SearchResult {
    pub hits: Vec<FileRef>,
    pub total: usize,
}

pub fn search(index: &Index, q: &Query, sort: SortBy, desc: bool, limit: usize) -> SearchResult {
    let mut hits: Vec<FileRef> = Vec::new();
    for (si, seg) in index.segments.iter().enumerate() {
        if let Some(src) = &q.source {
            if !seg.info.root.to_lowercase().starts_with(src.as_str()) {
                continue;
            }
        }
        let mut found: Vec<FileRef> = seg
            .entries
            .par_iter()
            .enumerate()
            .filter(|(i, e)| q.matches_entry(seg.lower(*i as u32), e))
            .map(|(i, _)| FileRef { seg: si as u16, idx: i as u32 })
            .collect();
        if !q.path_terms.is_empty() {
            found.retain(|r| {
                let p = seg.path(r.idx).to_string_lossy().to_lowercase();
                q.path_terms.iter().all(|t| p.contains(t.as_str()))
            });
        }
        hits.append(&mut found);
    }

    let total = hits.len();
    match sort {
        SortBy::None => {}
        SortBy::Name => hits.par_sort_by(|a, b| index.lower(*a).cmp(index.lower(*b))),
        SortBy::Size => hits.par_sort_by_key(|r| index.entry(*r).size),
        SortBy::Mtime => hits.par_sort_by_key(|r| index.entry(*r).mtime),
    }
    if desc && sort != SortBy::None {
        hits.reverse();
    }
    hits.truncate(limit);
    SearchResult { hits, total }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globbing() {
        let g = |p: &str, s: &str| glob_match(&p.chars().collect::<Vec<_>>(), &s.chars().collect::<Vec<_>>());
        assert!(g("*.mp4", "отпуск.mp4"));
        assert!(!g("*.mp4", "a.mp4.txt"));
        assert!(g("a?c*", "abcdef"));
    }
}

/// Поиск внутри одной папки источника и всех вложенных — для поиска в обзоре.
/// Возвращает до `limit` записей (папки первыми, затем по имени) и общее число найденного.
pub fn search_in(index: &Index, seg: u16, dir: Option<u32>, q: &Query, limit: usize) -> (Vec<u32>, usize) {
    let Some(s) = index.segments.get(seg as usize) else { return (vec![], 0) };
    let inside = |mut i: u32| -> bool {
        let Some(d) = dir else { return true };
        // поднимаемся к корню: внутри ли папки `d`
        loop {
            let p = s.entries[i as usize].parent;
            if p == d {
                return true;
            }
            if p == crate::index::NO_PARENT {
                return false;
            }
            i = p;
        }
    };
    let mut hits: Vec<u32> = s
        .entries
        .par_iter()
        .enumerate()
        .filter(|(i, e)| q.matches_entry(s.lower(*i as u32), e) && inside(*i as u32))
        .map(|(i, _)| i as u32)
        .collect();
    if !q.path_terms.is_empty() {
        hits.retain(|&i| {
            let p = s.path(i).to_string_lossy().to_lowercase();
            q.path_terms.iter().all(|t| p.contains(t.as_str()))
        });
    }
    let total = hits.len();
    hits.par_sort_by(|&a, &b| {
        let (ea, eb) = (&s.entries[a as usize], &s.entries[b as usize]);
        eb.is_dir().cmp(&ea.is_dir()).then_with(|| s.lower(a).cmp(s.lower(b)))
    });
    hits.truncate(limit);
    (hits, total)
}

#[cfg(test)]
mod search_in_tests {
    use super::*;
    use crate::scan::{scan_walk, ScanOptions};
    use std::sync::atomic::AtomicBool;

    #[test]
    fn only_inside_folder() {
        let dir = std::env::temp_dir().join(format!("filecat-sin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a").join("deep")).unwrap();
        std::fs::create_dir_all(dir.join("b")).unwrap();
        std::fs::write(dir.join("a").join("deep").join("voice_1.mp3"), b"1").unwrap();
        std::fs::write(dir.join("a").join("voice_2.wav"), b"2").unwrap();
        std::fs::write(dir.join("b").join("voice_3.mp3"), b"3").unwrap();
        let cancel = AtomicBool::new(false);
        let mut index = Index::default();
        index.upsert(scan_walk(&dir, &ScanOptions::default(), &cancel, &|_| {}).unwrap());
        let s = &index.segments[0];
        let a = (0..s.entries.len() as u32).find(|&i| s.name(i) == "a").unwrap();
        let names = |(v, _): (Vec<u32>, usize)| v.into_iter().map(|i| s.name(i).to_string()).collect::<Vec<_>>();
        assert_eq!(names(search_in(&index, 0, Some(a), &Query::parse("voice").unwrap(), 100)), ["voice_1.mp3", "voice_2.wav"]);
        assert_eq!(names(search_in(&index, 0, Some(a), &Query::parse("voice ext:mp3").unwrap(), 100)), ["voice_1.mp3"]);
        assert_eq!(search_in(&index, 0, None, &Query::parse("voice").unwrap(), 100).1, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
