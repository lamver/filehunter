use anyhow::Result;
use clap::{Parser, Subcommand};
use filecat_core::dupes::{find_duplicates, DupeOptions};
use filecat_core::scan::{scan_walk, ScanOptions};
use filecat_core::search::{search, Query, SortBy};
use filecat_core::util::{format_size, parse_size};
use filecat_core::Index;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "filehunter", about = "Каталог файлов: быстрый поиск и дубликаты")]
struct Cli {
    /// Папка с индексом и кэшем (по умолчанию %LOCALAPPDATA%\FileHunter)
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Просканировать диски или папки и обновить индекс
    Scan {
        paths: Vec<PathBuf>,
        #[arg(long)]
        threads: Option<usize>,
        /// Обычный приоритет вместо фонового
        #[arg(long)]
        no_background: bool,
        /// Быстрый режим: все потоки и без ограничения темпа (по умолчанию — бережный)
        #[arg(long)]
        fast: bool,
    },
    /// Поиск по именам: слова, *.mp4, ext:pdf, size:>100mb, type:dir, path:work, src:c:
    Find {
        query: Vec<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, value_parser = ["name", "size", "mtime"])]
        sort: Option<String>,
    },
    /// Найти дубликаты
    Dupes {
        #[arg(long, default_value = "1")]
        min_size: String,
        #[arg(long, default_value_t = 2)]
        threads: usize,
        /// Лимит чтения, МБ/с (0 — без лимита)
        #[arg(long, default_value_t = 50)]
        rate: u64,
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[arg(long)]
        no_background: bool,
        /// Сохранить все группы в JSON
        #[arg(long)]
        json: Option<PathBuf>,
    },
    /// Список источников в индексе
    Sources,
    /// Удалить источник из индекса
    Forget { root: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data = cli
        .data_dir
        .unwrap_or_else(|| dirs::data_local_dir().unwrap_or_else(|| ".".into()).join("FileHunter"));
    let index_path = data.join("index.bin");
    let cancel = AtomicBool::new(false);

    let t = Instant::now();
    let mut index = Index::load(&index_path)?;
    let load_ms = t.elapsed().as_millis();

    match cli.cmd {
        Cmd::Scan { paths, threads, no_background, fast } => {
            let mut opts = if fast { ScanOptions::default() } else { ScanOptions::gentle() };
            if let Some(n) = threads {
                opts.threads = n;
            }
            opts.background = !no_background;
            for p in paths {
                let seg = scan_walk(&p, &opts, &cancel, &|pr| {
                    eprint!("\r  {}: {} файлов, {} папок, {}   ", pr.root, pr.files, pr.dirs, format_size(pr.bytes));
                    let _ = std::io::stderr().flush();
                })?;
                eprintln!();
                let i = &seg.info;
                println!(
                    "{}: {} файлов, {} папок, {}, ошибок доступа {} — {:.1} с",
                    i.root, i.files, i.dirs, format_size(i.bytes), i.errors, i.scan_ms as f64 / 1000.0
                );
                index.upsert(seg);
            }
            let t = Instant::now();
            index.save(&index_path)?;
            println!("индекс сохранён за {} мс: {}", t.elapsed().as_millis(), index_path.display());
        }
        Cmd::Find { query, limit, sort } => {
            let q = Query::parse(&query.join(" "))?;
            let sort = match sort.as_deref() {
                Some("name") => SortBy::Name,
                Some("size") => SortBy::Size,
                Some("mtime") => SortBy::Mtime,
                _ => SortBy::None,
            };
            let t = Instant::now();
            let res = search(&index, &q, sort, sort != SortBy::Name, limit);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            for r in &res.hits {
                let e = index.entry(*r);
                let size = if e.is_dir() { "<папка>".to_string() } else { format_size(e.size) };
                println!("{:>10}  {}", size, index.path(*r).display());
            }
            eprintln!("найдено {} (показано {}), поиск {ms:.1} мс, загрузка индекса {load_ms} мс", res.total, res.hits.len());
        }
        Cmd::Dupes { min_size, threads, rate, top, no_background, json } => {
            let opts = DupeOptions {
                min_size: parse_size(&min_size).ok_or_else(|| anyhow::anyhow!("bad --min-size"))?,
                threads,
                background: !no_background,
                max_bytes_per_sec: rate * 1024 * 1024,
                cache_path: Some(data.join("hashes.db")),
                only_sizes: None,
                include_system: false,
            };
            let rep = find_duplicates(&index, &opts, &cancel, &|p| {
                eprint!("\r  {:?}: {}/{} прочитано {}, из кэша {}   ", p.stage, p.done, p.total, format_size(p.bytes_read), p.cache_hits);
                let _ = std::io::stderr().flush();
            })?;
            eprintln!();
            let wasted: u64 = rep.groups.iter().map(|g| g.wasted()).sum();
            for g in rep.groups.iter().take(top) {
                println!("\n{} × {}  (лишнее {})", g.files.len(), format_size(g.size), format_size(g.wasted()));
                for r in &g.files {
                    println!("    {}", index.path(*r).display());
                }
            }
            let s = &rep.stats;
            println!(
                "\nгрупп {}, можно освободить {}. кандидатов {}, полных хешей {}, прочитано {}, из кэша {}, ошибок {}, {:.1} с",
                rep.groups.len(), format_size(wasted), s.candidates, s.full_hashed, format_size(s.bytes_read),
                s.cache_hits, s.errors, s.elapsed_ms as f64 / 1000.0
            );
            if let Some(path) = json {
                let out: Vec<_> = rep
                    .groups
                    .iter()
                    .map(|g| serde_json::json!({
                        "size": g.size,
                        "hash": g.hash.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                        "files": g.files.iter().map(|r| index.path(*r)).collect::<Vec<_>>(),
                    }))
                    .collect();
                std::fs::write(&path, serde_json::to_string_pretty(&out)?)?;
                println!("JSON: {}", path.display());
            }
        }
        Cmd::Sources => {
            for s in &index.segments {
                let i = &s.info;
                println!("{:<20} {:>10} файлов {:>10}  {:?}", i.root, i.files, format_size(i.bytes), i.kind);
            }
            println!("всего записей {}, загрузка индекса {load_ms} мс", index.len());
        }
        Cmd::Forget { root } => {
            if index.remove(&root) {
                index.save(&index_path)?;
                println!("удалён {root}");
            } else {
                println!("нет такого источника: {root}");
            }
        }
    }
    Ok(())
}
