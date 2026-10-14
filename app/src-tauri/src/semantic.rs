//! Поиск по смыслу: «игры» находит «GOG Games», «налоги» — «НДФЛ_2023.pdf».
//!
//! Каждое осмысленное имя (папки, документы, фото и видео, аудио, архивы, дизайн; без
//! системных и служебных мест) превращается моделью multilingual-e5-small в отпечаток —
//! 384 числа. Одинаковые имена считаются один раз: отпечаток хранится по хешу текста.
//! Считается в фоне, на процессоре, в несколько потоков с фоновым приоритетом; модель
//! (~470 МБ) скачивается при первом запуске. Отпечатки — в data/semantic/vectors.bin
//! (хеш + 384 байта на имя), дописываются по ходу: перезапуск не теряет посчитанного.
//!
//! Поиск: запрос → отпечаток → сходство со всеми отпечатками → лучшие имена → записи
//! индекса с этими именами (свои источники и копии других компьютеров).

use crate::AppState;
use filecat_core::index::{Segment, NO_PARENT};
use rayon::prelude::*;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

pub const DIM: usize = 384;
const NONE: u32 = u32::MAX;
const BATCH: usize = 32;

/// Типы файлов, чьи имена считаем (кроме папок — их всегда).
const EXTS: &[&str] = &[
    // документы
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "rtf", "txt", "md", "csv", "epub", "fb2", "djvu",
    // фото
    "jpg", "jpeg", "png", "heic", "heif", "webp", "gif", "bmp", "tif", "tiff", "raw", "cr2", "cr3", "nef", "arw", "dng", "orf", "rw2",
    // видео
    "mp4", "mov", "mkv", "avi", "wmv", "m4v", "webm", "mpg", "mpeg", "3gp",
    // аудио
    "mp3", "flac", "m4a", "wav", "ogg", "aac", "wma", "opus",
    // архивы и образы
    "zip", "rar", "7z", "iso", "tar", "gz",
    // дизайн и проекты
    "psd", "ai", "fig", "sketch", "xd", "indd", "dwg", "dxf", "blend", "skp",
];

/// Папки, внутри которых имена не считаем: системное, служебное, кэши, зависимости.
const SKIP: &[&str] = &[
    "windows", "program files", "program files (x86)", "programdata", "appdata", "$recycle.bin",
    "system volume information", "recovery", "msocache", "node_modules", ".git", ".svn", ".hg",
    "__pycache__", ".cache", "cache", "caches", "temp", "tmp", ".tmp", ".venv", "venv",
    "site-packages", ".gradle", ".m2", ".cargo", ".rustup", ".npm", ".nuget", "library", "system",
];

#[derive(Clone, Default, Serialize)]
pub struct Status {
    /// off | download | indexing | ready | error
    pub state: String,
    /// Посчитано имён / всего нужно.
    pub done: u64,
    pub total: u64,
    pub error: String,
}

#[derive(Default)]
pub struct Store {
    /// Отпечатки подряд: строка — DIM байт, нормированный вектор × 127.
    vecs: Vec<i8>,
    hashes: Vec<u64>,
    by_hash: HashMap<u64, u32>,
}

/// Для одного сегмента: какая строка отпечатка у каждой записи (NONE — не считаем).
struct SegIds {
    stamp: (i64, usize),
    ids: Vec<u32>,
}

pub struct Semantic {
    store: RwLock<Store>,
    /// Ключ — «корень» своего источника или «компьютер|корень» копии.
    ids: RwLock<HashMap<String, SegIds>>,
    model: Mutex<Option<fastembed::TextEmbedding>>,
    pub status: Mutex<Status>,
    dir: PathBuf,
    wake: AtomicBool,
}

fn hash_text(t: &str) -> u64 {
    let h = blake3::hash(t.as_bytes());
    u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap())
}

/// Текст для модели: имя без расширения, разделители — пробелы. None — смысла нет
/// (пусто, одни цифры: «IMG_0001», «2023-05-01»).
pub fn text_of(name: &str, is_dir: bool) -> Option<String> {
    let stem = if is_dir { name } else { name.rsplit_once('.').map(|(s, _)| s).filter(|s| !s.is_empty()).unwrap_or(name) };
    let t: String = stem.chars().map(|c| if matches!(c, '_' | '-' | '.' | '+' | '(' | ')' | '[' | ']') { ' ' } else { c }).collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let letters = t.chars().filter(|c| c.is_alphabetic()).count();
    // «img 0001», «dsc 1234» — служебные имена камер без смысла
    let junk = matches!(t.split(' ').next(), Some("img" | "dsc" | "dscn" | "dji" | "pxl" | "vid" | "mvimg" | "screenshot" | "scan"))
        && t.split(' ').skip(1).all(|w| w.chars().all(|c| c.is_ascii_digit()));
    (letters >= 2 && !junk).then_some(t)
}

/// Что считать в сегменте: (запись, хеш текста, текст).
fn candidates(seg: &Segment) -> Vec<(u32, u64, String)> {
    let n = seg.entries.len();
    // пропуск по предкам: системные и служебные папки целиком
    let mut skip = vec![false; n];
    for i in 0..n {
        let e = &seg.entries[i];
        let parent_skip = e.parent != NO_PARENT && (e.parent as usize) < n && skip[e.parent as usize];
        skip[i] = parent_skip || e.is_deleted() || (e.is_dir() && SKIP.contains(&seg.lower(i as u32)));
    }
    (0..n as u32)
        .into_par_iter()
        .filter_map(|i| {
            let e = &seg.entries[i as usize];
            if skip[i as usize] || e.is_symlink() {
                return None;
            }
            let ok = e.is_dir() || seg.lower(i).rsplit_once('.').is_some_and(|(_, x)| EXTS.contains(&x));
            if !ok {
                return None;
            }
            let t = text_of(seg.name(i), e.is_dir())?;
            Some((i, hash_text(&t), t))
        })
        .collect()
}

fn quantize(v: &[f32]) -> Vec<i8> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    v.iter().map(|x| ((x / norm) * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
}

impl Semantic {
    pub fn new(data_dir: &std::path::Path) -> Arc<Self> {
        let dir = data_dir.join("semantic");
        let mut store = Store::default();
        // хеш (8 байт) + отпечаток (DIM байт); недописанный хвост отбрасываем
        if let Ok(b) = std::fs::read(dir.join("vectors.bin")) {
            for rec in b.chunks_exact(8 + DIM) {
                let h = u64::from_le_bytes(rec[..8].try_into().unwrap());
                if store.by_hash.contains_key(&h) {
                    continue;
                }
                store.by_hash.insert(h, store.hashes.len() as u32);
                store.hashes.push(h);
                store.vecs.extend(rec[8..].iter().map(|&x| x as i8));
            }
        }
        Arc::new(Self {
            status: Mutex::new(Status { state: if store.hashes.is_empty() { "off".into() } else { "ready".into() }, done: store.hashes.len() as u64, ..Default::default() }),
            store: RwLock::new(store),
            ids: RwLock::new(HashMap::new()),
            model: Mutex::new(None),
            dir,
            wake: AtomicBool::new(true),
        })
    }

    /// Пересмотреть кандидатов (появились новые файлы или копии).
    pub fn poke(&self) {
        self.wake.store(true, Ordering::Relaxed);
    }

    fn set_status(&self, app: &AppHandle, f: impl FnOnce(&mut Status)) {
        let s = {
            let mut s = self.status.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        let _ = app.emit("semantic-status", s);
    }

    fn load_model(&self, threads: usize) -> Result<(), String> {
        let mut m = self.model.lock().unwrap();
        if m.is_none() {
            let mut o = fastembed::TextInitOptions::new(fastembed::EmbeddingModel::MultilingualE5Small);
            o.cache_dir = self.dir.join("model");
            o.show_download_progress = false;
            o.max_length = 64;
            o.intra_threads = Some(threads);
            *m = Some(fastembed::TextEmbedding::try_new(o).map_err(|e| format!("модель не загрузилась: {e}"))?);
        }
        Ok(())
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut m = self.model.lock().unwrap();
        let model = m.as_mut().ok_or("модель не загружена")?;
        model.embed(texts, Some(texts.len().max(1))).map_err(|e| e.to_string())
    }

    /// Фоновая работа: кандидаты всех источников → отпечатки для новых имён.
    pub fn run(self: Arc<Self>, app: AppHandle, st: Arc<AppState>) {
        std::thread::spawn(move || {
            // дать программе спокойно запуститься
            std::thread::sleep(Duration::from_secs(60));
            filecat_core::throttle::enter_background_mode();
            let mut seen_gen = u64::MAX;
            loop {
                let gen = st.generation.load(Ordering::Relaxed);
                if self.wake.swap(false, Ordering::Relaxed) || gen != seen_gen {
                    seen_gen = gen;
                    if let Err(e) = self.pass(&app, &st) {
                        self.set_status(&app, |s| {
                            s.state = "error".into();
                            s.error = e;
                        });
                    }
                }
                std::thread::sleep(Duration::from_secs(120));
            }
        });
    }

    /// Один проход: разметить записи известными отпечатками, досчитать недостающие.
    fn pass(&self, app: &AppHandle, st: &AppState) -> Result<(), String> {
        // имена всех источников: свои и копии других компьютеров
        let mut todo: HashMap<u64, String> = HashMap::new();
        let mut first: Vec<u64> = Vec::new(); // свои папки пользователя — раньше остального
        let mut waiting: HashMap<u64, Vec<(String, u32)>> = HashMap::new();
        let mut fresh_ids: HashMap<String, SegIds> = HashMap::new();
        let mut each = |key: String, seg: &Segment, stamp: (i64, usize)| {
            let cands = candidates(seg);
            let store = self.store.read().unwrap();
            let mut ids = vec![NONE; seg.entries.len()];
            let users = user_rows(seg);
            for (i, h, t) in cands {
                match store.by_hash.get(&h) {
                    Some(&row) => ids[i as usize] = row,
                    None => {
                        if !todo.contains_key(&h) {
                            if users[i as usize] {
                                first.push(h);
                            }
                            todo.insert(h, t);
                        }
                        waiting.entry(h).or_default().push((key.clone(), i));
                    }
                }
            }
            fresh_ids.insert(key, SegIds { stamp, ids });
        };
        {
            let idx = st.index.read().unwrap();
            for s in &idx.segments {
                each(s.info.root.to_lowercase(), s, (s.info.scanned_at, s.entries.len()));
            }
        }
        {
            let reps = st.replicas.read().unwrap();
            for (peer, r) in reps.iter() {
                for (s, m) in r.index.segments.iter().zip(&r.meta) {
                    each(format!("{peer}|{}", m.root.to_lowercase()), s, (m.fetched, s.entries.len()));
                }
            }
        }
        *self.ids.write().unwrap() = fresh_ids;
        let total = self.store.read().unwrap().hashes.len() as u64 + todo.len() as u64;
        if todo.is_empty() {
            self.set_status(app, |s| {
                s.state = "ready".into();
                s.done = total;
                s.total = total;
            });
            return Ok(());
        }
        // порядок: сначала свои папки пользователя, потом остальное
        let firsts: HashSet<u64> = first.iter().copied().collect();
        let mut order: Vec<u64> = first;
        order.extend(todo.keys().filter(|h| !firsts.contains(h)).copied());

        self.set_status(app, |s| {
            s.state = "download".into();
            s.error.clear();
        });
        // бережно: пара потоков с фоновым приоритетом; без него — четверть ядер
        let gentle = true;
        let threads = if gentle { 2 } else { (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 4).max(2) };
        self.load_model(threads)?;
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("vectors.bin")).map_err(|e| e.to_string())?;
        let mut done = total - todo.len() as u64;
        let mut last = Instant::now();
        self.set_status(app, |s| {
            s.state = "indexing".into();
            s.done = done;
            s.total = total;
        });
        for chunk in order.chunks(BATCH) {
            let texts: Vec<String> = chunk.iter().map(|h| format!("passage: {}", todo[h])).collect();
            let vecs = self.embed(&texts)?;
            let mut rec = Vec::with_capacity(chunk.len() * (8 + DIM));
            {
                let mut store = self.store.write().unwrap();
                let mut ids = self.ids.write().unwrap();
                for (h, v) in chunk.iter().zip(vecs) {
                    let q = quantize(&v);
                    let row = store.hashes.len() as u32;
                    store.by_hash.insert(*h, row);
                    store.hashes.push(*h);
                    store.vecs.extend_from_slice(&q);
                    rec.extend_from_slice(&h.to_le_bytes());
                    rec.extend(q.iter().map(|&x| x as u8));
                    for (key, i) in waiting.get(h).into_iter().flatten() {
                        if let Some(s) = ids.get_mut(key) {
                            if let Some(slot) = s.ids.get_mut(*i as usize) {
                                *slot = row;
                            }
                        }
                    }
                }
            }
            file.write_all(&rec).map_err(|e| e.to_string())?;
            done += chunk.len() as u64;
            if last.elapsed() >= Duration::from_secs(1) {
                last = Instant::now();
                self.set_status(app, |s| s.done = done);
            }
            // новые файлы или копии — начнём проход заново со свежим списком
            if self.wake.load(Ordering::Relaxed) {
                break;
            }
        }
        let _ = file.flush();
        self.set_status(app, |s| {
            s.done = done;
            if done >= s.total {
                s.state = "ready".into();
            }
        });
        Ok(())
    }

    /// Строки отпечатков, похожие на запрос, с оценкой сходства (лучшие первыми).
    pub fn nearest(&self, query: &str, k: usize) -> Result<Vec<(u32, f32)>, String> {
        let t = query.trim().to_lowercase();
        // ещё ничего не посчитано — модель не грузим (первая загрузка — это скачивание)
        if t.chars().filter(|c| c.is_alphabetic()).count() < 2 || self.store.read().unwrap().hashes.is_empty() {
            return Ok(vec![]);
        }
        if self.model.lock().unwrap().is_none() {
            // модель уже скачана, если отпечатки есть; загрузить для запросов
            self.load_model(2)?;
        }
        let q = self.embed(&[format!("query: {t}")])?.pop().ok_or("пустой ответ модели")?;
        let norm = q.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
        let q: Vec<f32> = q.iter().map(|x| x / norm / 127.0).collect();
        let store = self.store.read().unwrap();
        let mut scored: Vec<(u32, f32)> = store
            .vecs
            .par_chunks_exact(DIM)
            .enumerate()
            .map(|(i, row)| (i as u32, row.iter().zip(&q).map(|(&a, &b)| a as f32 * b).sum::<f32>()))
            .collect();
        let k = k.min(scored.len());
        if k == 0 {
            return Ok(vec![]);
        }
        scored.select_nth_unstable_by(k - 1, |a, b| b.1.total_cmp(&a.1));
        scored.truncate(k);
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        // отсекаем заведомо далёкое: заметно хуже лучшего совпадения
        let best = scored.first().map(|x| x.1).unwrap_or(0.0);
        scored.retain(|x| x.1 >= 0.80 && x.1 >= best - 0.07);
        Ok(scored)
    }

    /// Записи сегмента с этими строками отпечатков: (запись, сходство).
    pub fn entries_of(&self, key: &str, rows: &HashMap<u32, f32>, stamp: (i64, usize)) -> Vec<(u32, f32)> {
        let ids = self.ids.read().unwrap();
        let Some(s) = ids.get(key) else { return vec![] };
        // сегмент пересканирован — номера записей другие, ждём следующего прохода
        if s.stamp.0 != stamp.0 {
            return vec![];
        }
        s.ids.par_iter().enumerate().filter_map(|(i, row)| rows.get(row).map(|sc| (i as u32, *sc))).collect()
    }
}

/// Записи внутри папок пользователя (C:\Users\…, /Users/…, /home/…) — их считаем первыми.
fn user_rows(seg: &Segment) -> Vec<bool> {
    let n = seg.entries.len();
    let mut u = vec![false; n];
    for i in 0..n {
        let e = &seg.entries[i];
        u[i] = if e.parent == NO_PARENT {
            matches!(seg.lower(i as u32), "users" | "home")
        } else {
            (e.parent as usize) < n && u[e.parent as usize]
        };
    }
    u
}

/// Для поиска по копиям: индекс копии и её ключ.
pub fn replica_key(peer: &str, root: &str) -> String {
    format!("{peer}|{}", root.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_to_text() {
        assert_eq!(text_of("Налоги_2023-final.pdf", false).as_deref(), Some("налоги 2023 final"));
        assert_eq!(text_of("GOG Games", true).as_deref(), Some("gog games"));
        assert_eq!(text_of("IMG_0001.jpg", false), None);
        assert_eq!(text_of("2023-05-01.pdf", false), None);
        assert_eq!(text_of("DSC 1234.JPG", false), None);
        assert_eq!(text_of("Отпуск Сочи", true).as_deref(), Some("отпуск сочи"));
    }

    #[test]
    fn quantized_is_normalized() {
        let q = quantize(&[3.0, 4.0]);
        assert_eq!(q, vec![76, 102]);
    }
}
