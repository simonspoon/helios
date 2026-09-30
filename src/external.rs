//! Third-party source: locate locked dependencies on disk and index their
//! definitions into `.helios/deps.db`, separate from the project index.
//!
//! Cargo packages come from `Cargo.lock` + `$CARGO_HOME/registry/src`; Python
//! packages from the `.venv` site-packages (the installed venv is the ground
//! truth for on-disk paths — a lockfile alone has none).

use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::db::Database;
use crate::indexer;
use crate::parsers;

/// One locked package and where its source lives.
pub struct Locked {
    pub ecosystem: &'static str,
    pub name: String,
    pub version: String,
    /// Source roots on disk (dirs, or single-file modules). Empty when the
    /// package is locked but its source is not present.
    pub paths: Vec<PathBuf>,
    /// Files the package itself installed (from RECORD), when known. Keeps
    /// namespace packages (several dists sharing `google/`) apart.
    pub files: Option<HashSet<PathBuf>>,
    /// Why the source cannot be resolved, for packages helios does not handle.
    pub unresolved: Option<String>,
}

impl Locked {
    pub fn id(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// Names compare equal across `-`, `_`, `.` and case (PEP 503 style).
fn normalize(name: &str) -> String {
    name.to_lowercase().replace(['-', '.'], "_")
}

pub fn deps_db_path(cwd: &Path) -> PathBuf {
    cwd.join(".helios/deps.db")
}

fn cargo_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CARGO_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(h));
    }
    let var = |k: &str| std::env::var_os(k).filter(|h| !h.is_empty());
    let home = if cfg!(windows) {
        var("USERPROFILE").or_else(|| var("HOME"))?
    } else {
        var("HOME").or_else(|| var("USERPROFILE"))?
    };
    Some(PathBuf::from(home).join(".cargo"))
}

fn cargo_packages(cwd: &Path) -> Result<Vec<Locked>> {
    let lock = cwd.join("Cargo.lock");
    if !lock.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(&lock).context("reading Cargo.lock")?;
    let doc: toml::Table = toml::from_str(&text).context("parsing Cargo.lock")?;

    // Index dirs under registry/src (one per registry), in a stable order.
    let mut index_dirs: Vec<PathBuf> = cargo_home()
        .and_then(|h| std::fs::read_dir(h.join("registry/src")).ok())
        .map(|rd| rd.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    index_dirs.sort();

    let mut out = Vec::new();
    let Some(pkgs) = doc.get("package").and_then(|p| p.as_array()) else {
        return Ok(out);
    };
    for pkg in pkgs {
        let (Some(name), Some(version)) = (
            pkg.get("name").and_then(|v| v.as_str()),
            pkg.get("version").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        // No `source` = workspace member / path dependency: the project itself.
        let Some(source) = pkg.get("source").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut locked = Locked {
            ecosystem: "cargo",
            name: name.to_string(),
            version: version.to_string(),
            paths: Vec::new(),
            files: None,
            unresolved: None,
        };
        if source.starts_with("registry+") {
            let dirname = format!("{name}-{version}");
            if let Some(dir) = index_dirs
                .iter()
                .map(|d| d.join(&dirname))
                .find(|d| d.is_dir())
            {
                locked.paths.push(dir);
            }
        } else {
            locked.unresolved = Some(format!(
                "unsupported source ({})",
                source.split('+').next().unwrap_or(source)
            ));
        }
        out.push(locked);
    }
    Ok(out)
}

/// `site-packages` directories of the project's `.venv`.
fn site_packages_dirs(cwd: &Path) -> Vec<PathBuf> {
    let venv = cwd.join(".venv");
    let mut dirs = Vec::new();
    let win = venv.join("Lib/site-packages");
    if win.is_dir() {
        dirs.push(win);
    }
    if let Ok(rd) = std::fs::read_dir(venv.join("lib")) {
        let mut pys: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("python"))
            })
            .collect();
        pys.sort();
        dirs.extend(
            pys.into_iter()
                .map(|p| p.join("site-packages"))
                .filter(|p| p.is_dir()),
        );
    }
    dirs
}

/// `Name:` / `Version:` from a dist-info METADATA file.
fn read_metadata(dist_info: &Path) -> (Option<String>, Option<String>) {
    let text = std::fs::read_to_string(dist_info.join("METADATA")).unwrap_or_default();
    let (mut name, mut version) = (None, None);
    for line in text.lines() {
        if line.is_empty() {
            break; // end of headers
        }
        if name.is_none()
            && let Some(v) = line.strip_prefix("Name:")
        {
            name = Some(v.trim().to_string());
        } else if version.is_none()
            && let Some(v) = line.strip_prefix("Version:")
        {
            version = Some(v.trim().to_string());
        }
    }
    (name, version)
}

/// Top-level source entries of an installed distribution: `top_level.txt`,
/// else the first path components of RECORD.
fn dist_sources(site: &Path, dist_info: &Path) -> Vec<PathBuf> {
    let mut entries: BTreeSet<String> = BTreeSet::new();
    if let Ok(text) = std::fs::read_to_string(dist_info.join("top_level.txt")) {
        entries.extend(
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from),
        );
    } else if let Ok(text) = std::fs::read_to_string(dist_info.join("RECORD")) {
        for line in text.lines() {
            // First CSV field is the path (paths with commas are quoted; rare).
            let path = line.split(',').next().unwrap_or("").trim_matches('"');
            let first = path.split('/').next().unwrap_or("");
            if first.is_empty()
                || first == ".."
                || first == "__pycache__"
                || first.ends_with(".dist-info")
            {
                continue;
            }
            entries.insert(first.to_string());
        }
    }
    entries
        .into_iter()
        .filter_map(|e| {
            let dir = site.join(&e);
            if dir.is_dir() || dir.is_file() {
                return Some(dir);
            }
            // top_level.txt names a module: `six` -> `six.py`
            let file = site.join(format!("{e}.py"));
            file.is_file().then_some(file)
        })
        .collect()
}

/// Absolute paths listed in a dist-info RECORD (None when absent or empty).
fn record_files(site: &Path, dist_info: &Path) -> Option<HashSet<PathBuf>> {
    let text = std::fs::read_to_string(dist_info.join("RECORD")).ok()?;
    let files: HashSet<PathBuf> = text
        .lines()
        .map(|l| l.split(',').next().unwrap_or("").trim_matches('"'))
        .filter(|p| !p.is_empty() && !p.starts_with(".."))
        .map(|p| site.join(p))
        .collect();
    (!files.is_empty()).then_some(files)
}

fn python_packages(cwd: &Path) -> Vec<Locked> {
    let mut out = Vec::new();
    for site in site_packages_dirs(cwd) {
        let Ok(rd) = std::fs::read_dir(&site) else {
            continue;
        };
        let mut infos: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "dist-info") && p.is_dir())
            .collect();
        infos.sort();
        for info in infos {
            let stem = info
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let (dir_name, dir_version) = match stem.split_once('-') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None => continue,
            };
            let (meta_name, meta_version) = read_metadata(&info);
            out.push(Locked {
                ecosystem: "python",
                name: meta_name.unwrap_or(dir_name),
                version: meta_version.unwrap_or(dir_version),
                paths: dist_sources(&site, &info),
                files: record_files(&site, &info),
                unresolved: None,
            });
        }
    }
    out
}

/// Every locked third-party package of the project in `cwd`.
pub fn locked_packages(cwd: &Path) -> Result<Vec<Locked>> {
    let mut all = cargo_packages(cwd)?;
    all.extend(python_packages(cwd));
    Ok(all)
}

/// Locked packages called `name`; errors when none is.
pub fn find_named(all: Vec<Locked>, name: &str) -> Result<Vec<Locked>> {
    let want = normalize(name);
    let found: Vec<Locked> = all
        .into_iter()
        .filter(|l| normalize(&l.name) == want)
        .collect();
    if found.is_empty() {
        anyhow::bail!("package '{name}' not found in Cargo.lock or .venv");
    }
    Ok(found)
}

fn ensure_schema(db: &Database) -> Result<()> {
    db.conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS packages (
            id INTEGER PRIMARY KEY,
            ecosystem TEXT NOT NULL,
            name TEXT NOT NULL,
            version TEXT NOT NULL,
            root TEXT NOT NULL,
            indexed_at TEXT NOT NULL DEFAULT (datetime('now')),
            UNIQUE(ecosystem, name, version)
        );
        CREATE TABLE IF NOT EXISTS package_files (
            file_id INTEGER NOT NULL,
            package_id INTEGER NOT NULL,
            PRIMARY KEY (package_id, file_id)
        );
        CREATE INDEX IF NOT EXISTS idx_package_files_file ON package_files(file_id);
        CREATE INDEX IF NOT EXISTS idx_package_files_pkg ON package_files(package_id);",
    )?;
    Ok(())
}

/// Open (creating `.helios/` and the dependency tables if needed) the
/// dependency database.
pub fn open_deps_db(cwd: &Path) -> Result<Database> {
    std::fs::create_dir_all(cwd.join(".helios")).context("creating .helios")?;
    let db = Database::open(&deps_db_path(cwd)).context("opening dependency database")?;
    // Concurrent cold runs wait for each other's write lock instead of failing.
    db.conn.busy_timeout(std::time::Duration::from_secs(5))?;
    ensure_schema(&db)?;
    Ok(db)
}

fn is_indexed(db: &Database, l: &Locked) -> Result<bool> {
    let n: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM packages WHERE ecosystem = ?1 AND name = ?2 AND version = ?3",
        rusqlite::params![l.ecosystem, l.name, l.version],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn index_package(db: &Database, l: &Locked) -> Result<()> {
    db.conn.execute_batch("BEGIN")?;
    let res = (|| -> Result<()> {
        let root = l
            .paths
            .first()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        db.conn.execute(
            "INSERT INTO packages (ecosystem, name, version, root) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![l.ecosystem, l.name, l.version, root],
        )?;
        let package_id = db.conn.last_insert_rowid();
        for root in &l.paths {
            for entry in walkdir::WalkDir::new(root)
                .into_iter()
                .filter_entry(|e| {
                    !matches!(
                        e.file_name().to_str(),
                        Some("target" | "__pycache__" | "node_modules" | ".git")
                    )
                })
                .flatten()
            {
                if !entry.file_type().is_file() {
                    continue;
                }
                let abs = entry.path();
                if l.files.as_ref().is_some_and(|f| !f.contains(abs)) {
                    continue;
                }
                let mut abs_str = abs.to_string_lossy().into_owned();
                if cfg!(windows) {
                    abs_str = abs_str.replace('\\', "/");
                }
                let Some(language) = parsers::detect_language(&abs_str) else {
                    continue;
                };
                if parsers::get_parser(language).is_none() {
                    continue;
                }
                // Definitions only; a file that cannot be read or parsed is skipped.
                if indexer::index_file_definitions(db, abs, &abs_str, language, false).is_err() {
                    continue;
                }
                if let Some(f) = db.get_file_by_path(&abs_str)? {
                    db.conn.execute(
                        "INSERT OR IGNORE INTO package_files (file_id, package_id) VALUES (?1, ?2)",
                        rusqlite::params![f.id, package_id],
                    )?;
                }
            }
        }
        Ok(())
    })();
    match res {
        Ok(()) => db.conn.execute_batch("COMMIT")?,
        Err(e) => {
            let _ = db.conn.execute_batch("ROLLBACK");
            return Err(e);
        }
    }
    Ok(())
}

/// Index each of `selected` that the database does not hold yet.
pub fn ensure_indexed(db: &Database, selected: &[&Locked]) -> Result<()> {
    let todo: Vec<&&Locked> = selected
        .iter()
        .filter(|l| !l.paths.is_empty())
        .filter(|l| !is_indexed(db, l).unwrap_or(false))
        .collect();
    if todo.is_empty() {
        return Ok(());
    }
    eprintln!("indexing {} dependency package(s)...", todo.len());
    for l in todo {
        index_package(db, l).with_context(|| format!("indexing {}", l.id()))?;
    }
    Ok(())
}

/// file id -> `name@version`, for files of the `selected` packages only.
pub fn package_file_map(db: &Database, selected: &[&Locked]) -> Result<HashMap<i64, String>> {
    let want: HashSet<(&str, &str, &str)> = selected
        .iter()
        .map(|l| (l.ecosystem, l.name.as_str(), l.version.as_str()))
        .collect();
    let mut stmt = db.conn.prepare(
        "SELECT pf.file_id, p.ecosystem, p.name, p.version
         FROM package_files pf JOIN packages p ON p.id = pf.package_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (id, eco, name, version) = row?;
        if want.contains(&(eco.as_str(), name.as_str(), version.as_str())) {
            // A file shared by several selected packages keeps the first.
            map.entry(id).or_insert_with(|| format!("{name}@{version}"));
        }
    }
    Ok(map)
}

/// `helios where <pkg>`.
pub fn run_where(package: &str, json: bool, compact: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("getting current directory")?;
    let found = find_named(locked_packages(&cwd)?, package)?;
    if json {
        let items: Vec<_> = found
            .iter()
            .map(|l| {
                let mut obj = serde_json::json!({
                    "ecosystem": l.ecosystem,
                    "name": l.name,
                    "version": l.version,
                    "path": l.paths.first().map(|p| p.to_string_lossy()),
                    "paths": l.paths.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
                    "found": !l.paths.is_empty(),
                });
                if let Some(u) = &l.unresolved {
                    obj["unresolved"] = serde_json::json!(u);
                }
                obj
            })
            .collect();
        let formatted = if compact {
            serde_json::to_string(&items)?
        } else {
            serde_json::to_string_pretty(&items)?
        };
        println!("{formatted}");
    } else {
        for l in &found {
            let loc = if l.paths.is_empty() {
                match &l.unresolved {
                    Some(u) => format!("not found on disk ({u})"),
                    None => "not found on disk".to_string(),
                }
            } else {
                l.paths
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            println!("{} {} {} {}", l.ecosystem, l.name, l.version, loc);
        }
    }
    Ok(())
}
