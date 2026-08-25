use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::HashMap;

pub struct Database {
    pub conn: Connection,
}

/// `?,?,?` for an `IN` clause of `n` bound values.
fn placeholders(n: usize) -> String {
    std::iter::repeat_n("?", n).collect::<Vec<_>>().join(",")
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FileRecord {
    pub id: i64,
    pub path: String,
    pub content_hash: String,
    pub language: String,
    pub last_indexed_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolRecord {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub file_id: i64,
    pub line: i64,
    pub column: i64,
    pub end_line: i64,
    pub visibility: String,
    pub scope: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportRecord {
    pub id: i64,
    pub source_file_id: i64,
    pub import_path: String,
    pub alias: Option<String>,
    pub resolved_file_id: Option<i64>,
}

/// An import row as the resolution pass sees it.
pub struct ImportToResolve {
    pub id: i64,
    pub source_path: String,
    pub language: String,
    pub import_path: String,
    pub resolved_file_id: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct ReferenceRecord {
    pub id: i64,
    pub symbol_id: i64,
    pub file_id: i64,
    pub line: i64,
    pub column: i64,
}

/// One definition a `deps` target resolved to.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolDefinition {
    pub id: i64,
    pub path: String,
    pub line: i64,
    pub scope: Option<String>,
}

/// One `type_relations` edge, joined with enough of the declaring symbol and
/// file to print without a further lookup — what `deps` needs for its
/// Supertypes/Implementors/Overrides sections.
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct TypeEdge {
    pub sub_name: String,
    pub sub_scope: Option<String>,
    pub super_name: String,
    /// "extends" | "implements" | "overrides".
    pub kind: String,
    /// Path of the file declaring `sub_name`.
    pub file: String,
    /// Language of the file declaring `sub_name`.
    pub language: String,
    /// `sub_name`'s declaration line.
    pub line: i64,
    /// True when `super_name` did not resolve to an indexed symbol.
    pub external: bool,
}

/// Per-file metadata with aggregated symbol/import counts.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileWithCounts {
    pub path: String,
    pub language: String,
    pub symbol_count: i64,
    pub import_count: i64,
    pub last_indexed_at: String,
}

/// Parsed symbol data before insertion (no id yet)
#[derive(Debug, Clone)]
pub struct ParsedSymbol {
    pub name: String,
    pub kind: String,
    pub line: i64,
    pub column: i64,
    pub end_line: i64,
    pub visibility: String,
    pub scope: Option<String>,
}

/// Parsed import data before insertion
#[derive(Debug, Clone)]
pub struct ParsedImport {
    pub import_path: String,
    pub alias: Option<String>,
    /// Local names this import binds (`import { formatMoney }` binds
    /// `formatMoney`, `import Money from` binds `Money`). Read by
    /// `symbol_references` to attribute a file's usages to the definition it
    /// imported rather than to every same-named definition. Empty for
    /// languages whose parsers do not extract names, or for imports that bind
    /// nothing usable.
    pub names: Vec<String>,
}

/// Parsed reference data before insertion
#[derive(Debug, Clone)]
pub struct ParsedReference {
    pub symbol_name: String,
    pub line: i64,
    pub column: i64,
    /// Enclosing scope (class/namespace) of the reference site, when known.
    /// Used at index time to prefer a same-scope definition over same-named
    /// definitions elsewhere. `None` when the parser cannot supply scope.
    pub from_scope: Option<String>,
    /// True when the usage is reached through a receiver (`wallet.format()`,
    /// `money.formatMoney()`) rather than spelled bare (`format()`). An
    /// import binds a bare name only, so qualified usages are exempt from
    /// import-based attribution in `symbol_references`.
    pub qualified: bool,
}

/// Parsed type-relation data before insertion (`class C extends B`, `impl
/// Trait for Type`, ...).
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ParsedTypeRelation {
    /// The declared type's name, matched at index time to the symbol just
    /// inserted for this file.
    pub sub_name: String,
    /// Declaration line, to disambiguate same-named symbols in one file.
    pub sub_line: i64,
    pub super_name: String,
    /// "extends" | "implements".
    pub kind: String,
}

impl Database {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        let db = Self { conn };
        db.create_tables()?;
        db.migrate()?;
        Ok(db)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn };
        db.create_tables()?;
        db.migrate()?;
        Ok(db)
    }

    fn create_tables(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                content_hash TEXT NOT NULL,
                language TEXT NOT NULL,
                last_indexed_at TEXT NOT NULL DEFAULT (datetime('now'))
            );

            CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                line INTEGER NOT NULL,
                column INTEGER NOT NULL,
                end_line INTEGER NOT NULL DEFAULT 0,
                visibility TEXT NOT NULL DEFAULT 'private',
                scope TEXT,
                docid TEXT
            );

            CREATE TABLE IF NOT EXISTS imports (
                id INTEGER PRIMARY KEY,
                source_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                import_path TEXT NOT NULL,
                alias TEXT,
                resolved_file_id INTEGER REFERENCES files(id) ON DELETE SET NULL
            );

            CREATE TABLE IF NOT EXISTS import_names (
                id INTEGER PRIMARY KEY,
                import_id INTEGER NOT NULL REFERENCES imports(id) ON DELETE CASCADE,
                name TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS references_ (
                id INTEGER PRIMARY KEY,
                symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                line INTEGER NOT NULL,
                column INTEGER NOT NULL,
                qualified INTEGER NOT NULL DEFAULT 0,
                container_symbol_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL
            );

            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- sub extends/implements super. super_symbol_id is NULL when the
            -- supertype resolves to nothing indexed (an external base type) —
            -- the row still exists, with super_name carrying the raw source
            -- text, so external supertypes are not silently dropped. A new
            -- table rather than a column, so it needs no migrate() ALTER: the
            -- CREATE TABLE/INDEX IF NOT EXISTS below already covers old DBs.
            CREATE TABLE IF NOT EXISTS type_relations (
                id INTEGER PRIMARY KEY,
                sub_symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
                super_symbol_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL,
                super_name TEXT NOT NULL,
                kind TEXT NOT NULL,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE
            );

            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
            CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(kind);
            CREATE INDEX IF NOT EXISTS idx_imports_source ON imports(source_file_id);
            CREATE INDEX IF NOT EXISTS idx_import_names_import ON import_names(import_id);
            CREATE INDEX IF NOT EXISTS idx_import_names_name ON import_names(name);
            CREATE INDEX IF NOT EXISTS idx_refs_symbol ON references_(symbol_id);
            CREATE INDEX IF NOT EXISTS idx_refs_file ON references_(file_id);
            CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);
            CREATE INDEX IF NOT EXISTS idx_type_rel_sub ON type_relations(sub_symbol_id);
            CREATE INDEX IF NOT EXISTS idx_type_rel_super ON type_relations(super_symbol_id);
            CREATE INDEX IF NOT EXISTS idx_type_rel_super_name ON type_relations(super_name);
            CREATE INDEX IF NOT EXISTS idx_type_rel_file ON type_relations(file_id);",
        )?;
        Ok(())
    }

    /// Run schema migrations for backward compatibility with older databases.
    fn migrate(&self) -> Result<()> {
        // Check if end_line column exists in symbols table
        let has_end_line: bool = self
            .conn
            .prepare("SELECT end_line FROM symbols LIMIT 0")
            .is_ok();

        if !has_end_line {
            self.conn.execute_batch(
                "ALTER TABLE symbols ADD COLUMN end_line INTEGER NOT NULL DEFAULT 0",
            )?;
        }

        // Check if docid column exists in symbols table
        let has_docid: bool = self
            .conn
            .prepare("SELECT docid FROM symbols LIMIT 0")
            .is_ok();

        if !has_docid {
            self.conn
                .execute_batch("ALTER TABLE symbols ADD COLUMN docid TEXT")?;
        }

        // Check if qualified column exists in references_ table
        let has_qualified: bool = self
            .conn
            .prepare("SELECT qualified FROM references_ LIMIT 0")
            .is_ok();

        if !has_qualified {
            self.conn.execute_batch(
                "ALTER TABLE references_ ADD COLUMN qualified INTEGER NOT NULL DEFAULT 0",
            )?;
        }

        // Check if container_symbol_id column exists in references_ table
        let has_container_symbol_id: bool = self
            .conn
            .prepare("SELECT container_symbol_id FROM references_ LIMIT 0")
            .is_ok();

        if !has_container_symbol_id {
            self.conn.execute_batch(
                "ALTER TABLE references_ ADD COLUMN container_symbol_id INTEGER REFERENCES symbols(id) ON DELETE SET NULL",
            )?;
        }

        // Check if resolved_file_id column exists in imports table
        let has_resolved: bool = self
            .conn
            .prepare("SELECT resolved_file_id FROM imports LIMIT 0")
            .is_ok();

        if !has_resolved {
            self.conn.execute_batch(
                "ALTER TABLE imports ADD COLUMN resolved_file_id INTEGER REFERENCES files(id) ON DELETE SET NULL",
            )?;
        }

        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_imports_resolved ON imports(resolved_file_id)",
        )?;

        // Index must be created here (after the column is ensured), not in
        // create_tables: on a pre-existing DB the CREATE TABLE IF NOT EXISTS
        // is a no-op and the docid column does not exist yet at that point.
        self.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS idx_symbols_docid ON symbols(docid)")?;

        // Same reasoning as idx_symbols_docid above: on a pre-existing DB the
        // column does not exist until the ALTER TABLE above runs.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_refs_container ON references_(container_symbol_id)",
        )?;

        Ok(())
    }

    // --- File operations ---

    pub fn upsert_file(&self, path: &str, content_hash: &str, language: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files (path, content_hash, language, last_indexed_at)
             VALUES (?1, ?2, ?3, datetime('now'))
             ON CONFLICT(path) DO UPDATE SET
                content_hash = excluded.content_hash,
                language = excluded.language,
                last_indexed_at = excluded.last_indexed_at",
            params![path, content_hash, language],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    pub fn get_file_by_path(&self, path: &str) -> Result<Option<FileRecord>> {
        self.conn
            .query_row(
                "SELECT id, path, content_hash, language, last_indexed_at FROM files WHERE path = ?1",
                params![path],
                |row| {
                    Ok(FileRecord {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        content_hash: row.get(2)?,
                        language: row.get(3)?,
                        last_indexed_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .context("querying file by path")
    }

    pub fn delete_file(&self, path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM files WHERE path = ?1", params![path])?;
        Ok(())
    }

    pub fn all_files(&self) -> Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, path, content_hash, language, last_indexed_at FROM files ORDER BY path",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(FileRecord {
                id: row.get(0)?,
                path: row.get(1)?,
                content_hash: row.get(2)?,
                language: row.get(3)?,
                last_indexed_at: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().context("listing files")
    }

    // --- Symbol operations ---

    pub fn insert_symbol(&self, file_id: i64, sym: &ParsedSymbol) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO symbols (name, kind, file_id, line, column, end_line, visibility, scope)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                sym.name,
                sym.kind,
                file_id,
                sym.line,
                sym.column,
                sym.end_line,
                sym.visibility,
                sym.scope,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_symbols_for_file(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    /// `(id, line, end_line)` of every symbol in a file, for matching a
    /// reference to its innermost enclosing symbol at index time.
    pub fn symbol_ranges_for_file(&self, file_id: i64) -> Result<Vec<(i64, i64, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, line, end_line FROM symbols WHERE file_id = ?1")?;
        let rows = stmt.query_map(params![file_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying symbol ranges for file")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn query_symbols(
        &self,
        file: Option<&str>,
        kind: Option<&str>,
        grep: Option<&str>,
        scope: Option<&str>,
        visibility: Option<&str>,
        limit: Option<i64>,
        offset: Option<i64>,
    ) -> Result<Vec<(SymbolRecord, String)>> {
        let mut sql = String::from(
            "SELECT s.id, s.name, s.kind, s.file_id, s.line, s.column, s.end_line, s.visibility, s.scope, f.path
             FROM symbols s JOIN files f ON s.file_id = f.id WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(f) = file {
            params_vec.push(Box::new(format!("%{f}%")));
            sql.push_str(&format!(" AND f.path LIKE ?{}", params_vec.len()));
        }
        if let Some(k) = kind {
            params_vec.push(Box::new(k.to_string()));
            sql.push_str(&format!(" AND s.kind = ?{}", params_vec.len()));
        }
        if let Some(g) = grep {
            params_vec.push(Box::new(format!("%{g}%")));
            sql.push_str(&format!(" AND s.name LIKE ?{}", params_vec.len()));
        }
        if let Some(s) = scope {
            params_vec.push(Box::new(s.to_string()));
            sql.push_str(&format!(" AND s.scope = ?{}", params_vec.len()));
        }
        if let Some(v) = visibility {
            params_vec.push(Box::new(v.to_string()));
            sql.push_str(&format!(" AND s.visibility = ?{}", params_vec.len()));
        }

        sql.push_str(" ORDER BY f.path, s.line");

        if let Some(l) = limit {
            params_vec.push(Box::new(l));
            sql.push_str(&format!(" LIMIT ?{}", params_vec.len()));
        }
        if let Some(o) = offset {
            params_vec.push(Box::new(o));
            sql.push_str(&format!(" OFFSET ?{}", params_vec.len()));
        }

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok((
                SymbolRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    file_id: row.get(3)?,
                    line: row.get(4)?,
                    column: row.get(5)?,
                    end_line: row.get(6)?,
                    visibility: row.get(7)?,
                    scope: row.get(8)?,
                },
                row.get::<_, String>(9)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying symbols")
    }

    /// Count symbols matching the given filters (for pagination metadata).
    pub fn count_symbols(
        &self,
        file: Option<&str>,
        kind: Option<&str>,
        grep: Option<&str>,
        scope: Option<&str>,
        visibility: Option<&str>,
    ) -> Result<i64> {
        let mut sql = String::from(
            "SELECT COUNT(*)
             FROM symbols s JOIN files f ON s.file_id = f.id WHERE 1=1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(f) = file {
            params_vec.push(Box::new(format!("%{f}%")));
            sql.push_str(&format!(" AND f.path LIKE ?{}", params_vec.len()));
        }
        if let Some(k) = kind {
            params_vec.push(Box::new(k.to_string()));
            sql.push_str(&format!(" AND s.kind = ?{}", params_vec.len()));
        }
        if let Some(g) = grep {
            params_vec.push(Box::new(format!("%{g}%")));
            sql.push_str(&format!(" AND s.name LIKE ?{}", params_vec.len()));
        }
        if let Some(s) = scope {
            params_vec.push(Box::new(s.to_string()));
            sql.push_str(&format!(" AND s.scope = ?{}", params_vec.len()));
        }
        if let Some(v) = visibility {
            params_vec.push(Box::new(v.to_string()));
            sql.push_str(&format!(" AND s.visibility = ?{}", params_vec.len()));
        }

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        self.conn
            .query_row(&sql, params_refs.as_slice(), |row| row.get(0))
            .context("counting symbols")
    }

    pub fn find_symbol_by_name(&self, name: &str) -> Result<Vec<(SymbolRecord, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.name, s.kind, s.file_id, s.line, s.column, s.end_line, s.visibility, s.scope, f.path
             FROM symbols s JOIN files f ON s.file_id = f.id
             WHERE s.name = ?1 ORDER BY f.path, s.line",
        )?;
        let rows = stmt.query_map(params![name], |row| {
            Ok((
                SymbolRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    file_id: row.get(3)?,
                    line: row.get(4)?,
                    column: row.get(5)?,
                    end_line: row.get(6)?,
                    visibility: row.get(7)?,
                    scope: row.get(8)?,
                },
                row.get::<_, String>(9)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("finding symbol by name")
    }

    // --- Import operations ---

    pub fn insert_import(&self, file_id: i64, imp: &ParsedImport) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO imports (source_file_id, import_path, alias) VALUES (?1, ?2, ?3)",
            params![file_id, imp.import_path, imp.alias],
        )?;
        let import_id = self.conn.last_insert_rowid();
        for name in &imp.names {
            self.conn.execute(
                "INSERT INTO import_names (import_id, name) VALUES (?1, ?2)",
                params![import_id, name],
            )?;
        }
        Ok(import_id)
    }

    pub fn delete_imports_for_file(&self, file_id: i64) -> Result<()> {
        // Deleted explicitly rather than by cascade: a re-indexed file whose
        // import names survived would attribute its references to whatever it
        // used to import.
        self.conn.execute(
            "DELETE FROM import_names WHERE import_id IN
             (SELECT id FROM imports WHERE source_file_id = ?1)",
            params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM imports WHERE source_file_id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    pub fn get_imports_for_file(&self, file_id: i64) -> Result<Vec<ImportRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source_file_id, import_path, alias, resolved_file_id
             FROM imports WHERE source_file_id = ?1 ORDER BY import_path",
        )?;
        let rows = stmt.query_map(params![file_id], |row| {
            Ok(ImportRecord {
                id: row.get(0)?,
                source_file_id: row.get(1)?,
                import_path: row.get(2)?,
                alias: row.get(3)?,
                resolved_file_id: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("getting imports for file")
    }

    /// What does this file import (outgoing deps)? Imports resolved to an
    /// indexed file report that file's path — which is what makes transitive
    /// traversal possible; the rest report their raw specifier (packages,
    /// namespaces, unindexed files).
    pub fn file_dependencies(&self, path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT COALESCE(t.path, i.import_path) AS dep
             FROM imports i
             JOIN files f ON i.source_file_id = f.id
             LEFT JOIN files t ON i.resolved_file_id = t.id
             WHERE f.path = ?1 ORDER BY dep",
        )?;
        let rows = stmt.query_map(params![path], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying file dependencies")
    }

    /// What files import this file/module (incoming deps)?
    ///
    /// For an indexed file this is the resolved file -> file edge, so the
    /// natural query (the file's own path) returns every importer regardless of
    /// how each one spelled the specifier. A target that is not an indexed file
    /// — a raw specifier such as `../util/money` or a package name — falls back
    /// to substring-matching the specifier text.
    pub fn file_dependents(&self, path: &str) -> Result<Vec<String>> {
        if let Some(file) = self.get_file_by_path(path)? {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT f.path
                 FROM imports i JOIN files f ON i.source_file_id = f.id
                 WHERE i.resolved_file_id = ?1 ORDER BY f.path",
            )?;
            let rows = stmt.query_map(params![file.id], |row| row.get::<_, String>(0))?;
            return rows
                .collect::<Result<Vec<_>, _>>()
                .context("querying file dependents");
        }

        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT f.path
             FROM imports i JOIN files f ON i.source_file_id = f.id
             WHERE i.import_path LIKE ?1 ORDER BY f.path",
        )?;
        let rows = stmt.query_map(params![format!("%{path}%")], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying file dependents")
    }

    /// Every import row with its source file's path and language and its
    /// current resolution — the input to the post-index resolution pass.
    pub fn all_imports_with_source(&self) -> Result<Vec<ImportToResolve>> {
        let mut stmt = self.conn.prepare(
            "SELECT i.id, f.path, f.language, i.import_path, i.resolved_file_id
             FROM imports i JOIN files f ON i.source_file_id = f.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ImportToResolve {
                id: row.get(0)?,
                source_path: row.get(1)?,
                language: row.get(2)?,
                import_path: row.get(3)?,
                resolved_file_id: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("listing imports")
    }

    /// Write `(import id, resolved file id)` pairs in one transaction. The
    /// resolution pass touches every import row on every run, so a per-row
    /// autocommit here costs seconds of fsync on a large repo.
    pub fn apply_import_resolutions(&self, updates: &[(i64, Option<i64>)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare("UPDATE imports SET resolved_file_id = ?1 WHERE id = ?2")?;
            for (import_id, resolved_file_id) in updates {
                stmt.execute(params![resolved_file_id, import_id])?;
            }
        }
        tx.commit().context("writing import resolutions")
    }

    /// What does a symbol depend on (via its file's imports)?
    pub fn symbol_dependencies(&self, symbol_ids: &[i64]) -> Result<Vec<String>> {
        if symbol_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT i.import_path
             FROM symbols s
             JOIN imports i ON i.source_file_id = s.file_id
             WHERE s.id IN ({})
             ORDER BY i.import_path",
            placeholders(symbol_ids.len())
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(symbol_ids), |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying symbol dependencies")
    }

    /// Definitions named `name`, optionally narrowed to one scope (exact match,
    /// as `symbols --scope`) and/or one defining file (substring, as `symbols
    /// --file`). This is how a `deps` target picks between same-named
    /// definitions.
    pub fn find_definitions(
        &self,
        name: &str,
        scope: Option<&str>,
        file: Option<&str>,
    ) -> Result<Vec<SymbolDefinition>> {
        let mut sql = String::from(
            "SELECT s.id, f.path, s.line, s.scope
             FROM symbols s JOIN files f ON s.file_id = f.id
             WHERE s.name = ?1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(name.to_string())];

        if let Some(s) = scope {
            params_vec.push(Box::new(s.to_string()));
            sql.push_str(&format!(" AND s.scope = ?{}", params_vec.len()));
        }
        if let Some(f) = file {
            params_vec.push(Box::new(format!("%{f}%")));
            sql.push_str(&format!(" AND f.path LIKE ?{}", params_vec.len()));
        }
        sql.push_str(" ORDER BY f.path, s.line");

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params_vec.iter()), |row| {
            Ok(SymbolDefinition {
                id: row.get(0)?,
                path: row.get(1)?,
                line: row.get(2)?,
                scope: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("finding symbol definitions")
    }

    /// What references point to this symbol (reverse deps)?
    ///
    /// An ambiguous name gets one reference row per candidate definition (see
    /// `insert_references`), so a usage site would otherwise be emitted once
    /// per candidate. Callers want the usage sites, not the candidate fan-out,
    /// so collapse them with DISTINCT.
    ///
    /// Import-aware: a usage of a name the referencing file *imports* belongs
    /// to the definition in the file it imported, so rows pointing at any other
    /// definition of that name are dropped here — that is what makes
    /// `deps formatMoney --file src/legacy/money.ts` list the legacy callers
    /// rather than every caller of the name.
    ///
    /// Deliberately narrow, because an import binding is evidence about one
    /// spelling only. It applies to a bare-identifier usage (`formatMoney()`),
    /// never to a qualified one (`money.formatMoney()`, `wallet.format()`) —
    /// those name a member reached through some other value, which the import
    /// says nothing about. It applies only when the imported file really
    /// defines the name, so a re-export barrel or an aliased import keeps the
    /// all-candidates answer, and never hides a definition in the referencing
    /// file itself, nor one in a file the referencing file also imports the
    /// name from — a `try: from .fast import dumps / except ImportError: from
    /// .slow import dumps` pair binds one name to two files, and both are real
    /// answers. Languages whose specifiers name a package (Go, Swift, C#) bind
    /// no import names and are unaffected.
    ///
    /// Filtered at read time rather than deleted at index time: the import
    /// graph moves when *either* side of it changes, and an incremental update
    /// only re-indexes the files that changed. A pruned row could not be
    /// brought back when the file it pointed at was the one that moved.
    #[allow(clippy::type_complexity)]
    pub fn symbol_references(
        &self,
        symbol_ids: &[i64],
    ) -> Result<Vec<(String, i64, i64, Option<String>)>> {
        if symbol_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT f.path, r.line, r.column, c.name
             FROM references_ r
             JOIN files f ON r.file_id = f.id
             JOIN symbols s ON s.id = r.symbol_id
             LEFT JOIN symbols c ON c.id = r.container_symbol_id
             WHERE r.symbol_id IN ({})
               AND (
                 r.qualified = 1
                 OR s.file_id = r.file_id
                 OR EXISTS (
                     SELECT 1 FROM imports i2
                     JOIN import_names n2 ON n2.import_id = i2.id
                     WHERE i2.source_file_id = r.file_id
                       AND n2.name = s.name
                       AND i2.resolved_file_id = s.file_id
                 )
                 OR NOT EXISTS (
                     SELECT 1 FROM imports i
                     JOIN import_names n ON n.import_id = i.id
                     WHERE i.source_file_id = r.file_id
                       AND n.name = s.name
                       AND i.resolved_file_id IS NOT NULL
                       AND i.resolved_file_id <> s.file_id
                       AND EXISTS (
                           SELECT 1 FROM symbols d
                           WHERE d.file_id = i.resolved_file_id AND d.name = s.name
                       )
                 )
               )
             ORDER BY f.path, r.line, r.column",
            placeholders(symbol_ids.len())
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(symbol_ids), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying symbol references")
    }

    // --- Reference operations ---

    #[allow(clippy::too_many_arguments)]
    pub fn insert_reference(
        &self,
        symbol_id: i64,
        file_id: i64,
        line: i64,
        column: i64,
        qualified: bool,
        container_symbol_id: Option<i64>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO references_ (symbol_id, file_id, line, column, qualified, container_symbol_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![symbol_id, file_id, line, column, qualified, container_symbol_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record a reference against every candidate symbol it may resolve to.
    ///
    /// When a name is ambiguous (declared in 2+ places) we cannot statically
    /// decide which definition a usage targets, so we link the reference to all
    /// candidates rather than silently picking one and attributing the usage to
    /// the wrong definition. Returns the number of reference rows inserted.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_references(
        &self,
        symbol_ids: &[i64],
        file_id: i64,
        line: i64,
        column: i64,
        qualified: bool,
        container_symbol_id: Option<i64>,
    ) -> Result<usize> {
        for &symbol_id in symbol_ids {
            self.insert_reference(
                symbol_id,
                file_id,
                line,
                column,
                qualified,
                container_symbol_id,
            )?;
        }
        Ok(symbol_ids.len())
    }

    pub fn delete_references_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM references_ WHERE file_id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    /// Delete every reference whose *source* file has the given language.
    ///
    /// Used by the semantic ingest: in semantic mode the sidecar output is the
    /// entire `.cs` reference set, so stale rows — including those on
    /// hash-unchanged files that `clear_file_data` never touched — are cleared
    /// before the exact rows are inserted. Rows sourced from other languages'
    /// files are untouched (P3-M8).
    pub fn delete_references_from_language(&self, language: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM references_ WHERE file_id IN (SELECT id FROM files WHERE language = ?1)",
            params![language],
        )?;
        Ok(())
    }

    // --- Type-relation operations ---

    #[allow(dead_code)]
    pub fn insert_type_relation(
        &self,
        sub_symbol_id: i64,
        super_symbol_id: Option<i64>,
        super_name: &str,
        kind: &str,
        file_id: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO type_relations (sub_symbol_id, super_symbol_id, super_name, kind, file_id)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![sub_symbol_id, super_symbol_id, super_name, kind, file_id],
        )?;
        Ok(())
    }

    pub fn delete_type_relations_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM type_relations WHERE file_id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    /// Delete every type relation whose *declaring* file has the given
    /// language. Mirrors `delete_references_from_language`: used by the
    /// semantic ingest, where the sidecar output is the entire `.cs` relation
    /// set and stale rows on hash-unchanged files must still be cleared.
    #[allow(dead_code)]
    pub fn delete_type_relations_from_language(&self, language: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM type_relations WHERE file_id IN (SELECT id FROM files WHERE language = ?1)",
            params![language],
        )?;
        Ok(())
    }

    /// What the given symbols extend/implement.
    #[allow(dead_code)]
    pub fn supertypes_of(&self, symbol_ids: &[i64]) -> Result<Vec<TypeEdge>> {
        if symbol_ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(&format!(
            "SELECT s.name, s.scope, tr.super_name, tr.kind, f.path, f.language, s.line,
                    tr.super_symbol_id IS NULL AS external
             FROM type_relations tr
             JOIN symbols s ON s.id = tr.sub_symbol_id
             JOIN files f ON f.id = tr.file_id
             WHERE tr.sub_symbol_id IN ({})
             ORDER BY f.path, s.line",
            placeholders(symbol_ids.len())
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(symbol_ids), |row| {
            Ok(TypeEdge {
                sub_name: row.get(0)?,
                sub_scope: row.get(1)?,
                super_name: row.get(2)?,
                kind: row.get(3)?,
                file: row.get(4)?,
                language: row.get(5)?,
                line: row.get(6)?,
                external: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying supertypes")
    }

    /// What extends/implements the given symbols. A row also matches by
    /// `name` when its supertype never resolved (`super_symbol_id IS NULL`),
    /// so an external base class recorded only as raw text is still findable
    /// by name — mirrors the `super_name` matching in `symbol_references`.
    /// `symbol_ids` may legitimately be empty (an unresolved target still has
    /// a name to search by), so unlike `supertypes_of` this does not
    /// short-circuit on that.
    #[allow(dead_code)]
    pub fn implementors_of(&self, symbol_ids: &[i64], name: &str) -> Result<Vec<TypeEdge>> {
        let id_clause = if symbol_ids.is_empty() {
            "0".to_string()
        } else {
            format!("tr.super_symbol_id IN ({})", placeholders(symbol_ids.len()))
        };
        let mut stmt = self.conn.prepare(&format!(
            "SELECT DISTINCT s.name, s.scope, tr.super_name, tr.kind, f.path, f.language, s.line,
                    tr.super_symbol_id IS NULL AS external
             FROM type_relations tr
             JOIN symbols s ON s.id = tr.sub_symbol_id
             JOIN files f ON f.id = tr.file_id
             WHERE {id_clause}
                OR (tr.super_symbol_id IS NULL
                    AND (tr.super_name = ?{n} OR tr.super_name LIKE '%.' || ?{n}))
             ORDER BY f.path, s.line",
            n = symbol_ids.len() + 1
        ))?;
        let mut params_vec: Vec<&dyn rusqlite::types::ToSql> = symbol_ids
            .iter()
            .map(|id| id as &dyn rusqlite::types::ToSql)
            .collect();
        params_vec.push(&name);
        let rows = stmt.query_map(params_vec.as_slice(), |row| {
            Ok(TypeEdge {
                sub_name: row.get(0)?,
                sub_scope: row.get(1)?,
                super_name: row.get(2)?,
                kind: row.get(3)?,
                file: row.get(4)?,
                language: row.get(5)?,
                line: row.get(6)?,
                external: row.get(7)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying implementors")
    }

    /// Members named `name` whose scope is a subtype (one level) of `scope`,
    /// per `type_relations`. Derived, not stored: overriding is implied by
    /// same-name-in-a-subtype, so there is no second edge kind to maintain.
    #[allow(dead_code)]
    pub fn overrides_of(&self, name: &str, scope: &str) -> Result<Vec<TypeEdge>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.name, s.scope, f.path, f.language, s.line
             FROM symbols s
             JOIN files f ON f.id = s.file_id
             WHERE s.name = ?1
               AND s.scope IN (
                   SELECT sub.name FROM symbols sub
                   JOIN type_relations tr ON tr.sub_symbol_id = sub.id
                   WHERE tr.super_name = ?2 OR tr.super_name LIKE '%.' || ?2
               )
             ORDER BY f.path, s.line",
        )?;
        let rows = stmt.query_map(params![name, scope], |row| {
            Ok(TypeEdge {
                sub_name: row.get(0)?,
                sub_scope: row.get(1)?,
                super_name: scope.to_string(),
                kind: "overrides".to_string(),
                file: row.get(2)?,
                language: row.get(3)?,
                line: row.get(4)?,
                external: false,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying overrides")
    }

    /// `(type_relations.id, super_name)` for every row still unresolved
    /// (`super_symbol_id IS NULL`) — the file declaring the supertype may not
    /// have been indexed yet when its subtype was. Mirrors
    /// `all_imports_with_source`: resolution is a whole-index second pass, run
    /// after every file is in, so a forward reference resolves once the type
    /// it names shows up anywhere in the repo.
    #[allow(dead_code)]
    pub fn unresolved_type_relations(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, super_name FROM type_relations WHERE super_symbol_id IS NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("listing unresolved type relations")
    }

    /// Write `(relation id, resolved symbol id)` pairs in one transaction, as
    /// `apply_import_resolutions` does for imports. Unlike that method's
    /// `Option<i64>`, a plain `i64` is right here: an unresolved relation is
    /// simply absent from `updates` and its `super_symbol_id` stays NULL —
    /// there is no resolved-to-then-un-resolved case to express.
    #[allow(dead_code)]
    pub fn apply_type_relation_resolutions(&self, updates: &[(i64, i64)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt =
                tx.prepare("UPDATE type_relations SET super_symbol_id = ?1 WHERE id = ?2")?;
            for (relation_id, symbol_id) in updates {
                stmt.execute(params![symbol_id, relation_id])?;
            }
        }
        tx.commit().context("writing type relation resolutions")
    }

    /// Stamp a sidecar DocId onto the symbol row(s) `index_file` inserted for
    /// this `(file_id, line, name)` (P3-M3). Returns the number of rows
    /// updated; 0 means no match (e.g. a symbol kind the tree-sitter path does
    /// not index) — callers skip those silently.
    pub fn stamp_symbol_docid(
        &self,
        file_id: i64,
        line: i64,
        name: &str,
        docid: &str,
    ) -> Result<usize> {
        let updated = self.conn.execute(
            "UPDATE symbols SET docid = ?4 WHERE file_id = ?1 AND line = ?2 AND name = ?3",
            params![file_id, line, name, docid],
        )?;
        Ok(updated)
    }

    /// In-memory DocId → symbol-id map over stamped rows (P3-M4). A docid
    /// stamped onto multiple rows (partial types) maps to all of them.
    pub fn docid_symbol_map(&self) -> Result<HashMap<String, Vec<i64>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT docid, id FROM symbols WHERE docid IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut map: HashMap<String, Vec<i64>> = HashMap::new();
        for row in rows {
            let (docid, id) = row?;
            map.entry(docid).or_default().push(id);
        }
        Ok(map)
    }

    /// `docid → (symbol_id, file_id)` pairs, for `ingest_semantic` to narrow a
    /// reference's `container_docid` to the candidate in the reference's own
    /// file (a container must be a symbol in the same file — see
    /// `ingest_semantic` for the full rule). A sibling of `docid_symbol_map`
    /// rather than a change to it: that map's callers want ids only and one
    /// already asserts its exact return type in a test, and a reference's
    /// container needs the file_id besides.
    pub fn docid_symbol_file_map(&self) -> Result<HashMap<String, Vec<(i64, i64)>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT docid, id, file_id FROM symbols WHERE docid IS NOT NULL")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut map: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
        for row in rows {
            let (docid, id, file_id) = row?;
            map.entry(docid).or_default().push((id, file_id));
        }
        Ok(map)
    }

    // --- Metadata operations ---

    /// `metadata` key holding the index-format version an on-disk index was
    /// built with. Bumped whenever a change to what indexing populates (a new
    /// table, a new pass over already-parsed files) needs every file
    /// re-parsed to take effect — the content-hash short-circuit in
    /// `index_file_definitions` has no way to know that on its own, since the
    /// file's *content* didn't change, only what helios extracts from it.
    /// Absent (pre-versioning index) or lower than `CURRENT_INDEX_FORMAT_VERSION`
    /// means a full re-parse is owed; see `indexer::index_full`.
    pub const INDEX_FORMAT_VERSION_KEY: &str = "index_format_version";

    /// Bump this when adding a table or pass that a full re-index must
    /// populate for every file, not just ones whose content changed.
    /// Current: 2 — introduced to backfill `type_relations`, which the
    /// content-hash cache was silently leaving empty on an unchanged file for
    /// anyone upgrading into an existing index.
    pub const CURRENT_INDEX_FORMAT_VERSION: &str = "2";

    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .context("querying metadata")
    }

    // --- Cleanup for re-indexing a file ---

    pub fn clear_file_data(&self, file_id: i64) -> Result<()> {
        self.delete_type_relations_for_file(file_id)?;
        self.delete_references_for_file(file_id)?;
        self.delete_symbols_for_file(file_id)?;
        self.delete_imports_for_file(file_id)?;
        Ok(())
    }

    // --- Summary queries ---

    pub fn file_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .context("counting files")
    }

    pub fn symbol_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |row| row.get(0))
            .context("counting symbols")
    }

    pub fn symbols_by_kind(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT kind, COUNT(*) FROM symbols GROUP BY kind ORDER BY kind")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("counting symbols by kind")
    }

    pub fn import_count(&self) -> Result<i64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM imports", [], |row| row.get(0))
            .context("counting imports")
    }

    pub fn files_by_language(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT language, COUNT(*) FROM files GROUP BY language ORDER BY language")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("counting files by language")
    }

    /// Return files with per-file symbol and import counts, optionally filtered by language.
    pub fn files_with_counts(&self, language: Option<&str>) -> Result<Vec<FileWithCounts>> {
        let mut sql = String::from(
            "SELECT f.path, f.language,
                    COALESCE(s.cnt, 0) AS symbol_count,
                    COALESCE(i.cnt, 0) AS import_count,
                    f.last_indexed_at
             FROM files f
             LEFT JOIN (SELECT file_id, COUNT(*) AS cnt FROM symbols GROUP BY file_id) s
               ON s.file_id = f.id
             LEFT JOIN (SELECT source_file_id, COUNT(*) AS cnt FROM imports GROUP BY source_file_id) i
               ON i.source_file_id = f.id",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

        if let Some(lang) = language {
            params_vec.push(Box::new(lang.to_string()));
            sql.push_str(&format!(" WHERE f.language = ?{}", params_vec.len()));
        }

        sql.push_str(" ORDER BY f.path");

        let params_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|p| p.as_ref()).collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_refs.as_slice(), |row| {
            Ok(FileWithCounts {
                path: row.get(0)?,
                language: row.get(1)?,
                symbol_count: row.get(2)?,
                import_count: row.get(3)?,
                last_indexed_at: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying files with counts")
    }

    pub fn symbols_in_directory(&self, dir_prefix: &str) -> Result<Vec<(SymbolRecord, String)>> {
        let pattern = if dir_prefix.is_empty() {
            "%".to_string()
        } else {
            format!("{dir_prefix}%")
        };
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.name, s.kind, s.file_id, s.line, s.column, s.end_line, s.visibility, s.scope, f.path
             FROM symbols s JOIN files f ON s.file_id = f.id
             WHERE f.path LIKE ?1
             ORDER BY f.path, s.line",
        )?;
        let rows = stmt.query_map(params![pattern], |row| {
            Ok((
                SymbolRecord {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    kind: row.get(2)?,
                    file_id: row.get(3)?,
                    line: row.get(4)?,
                    column: row.get(5)?,
                    end_line: row.get(6)?,
                    visibility: row.get(7)?,
                    scope: row.get(8)?,
                },
                row.get::<_, String>(9)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying symbols in directory")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_tables() {
        let db = Database::open_in_memory().unwrap();
        assert_eq!(db.file_count().unwrap(), 0);
        assert_eq!(db.symbol_count().unwrap(), 0);
    }

    #[test]
    fn test_file_crud() {
        let db = Database::open_in_memory().unwrap();
        let id = db.upsert_file("src/main.rs", "abc123", "rust").unwrap();
        assert!(id > 0);

        let file = db.get_file_by_path("src/main.rs").unwrap().unwrap();
        assert_eq!(file.content_hash, "abc123");
        assert_eq!(file.language, "rust");

        // Update
        let id2 = db.upsert_file("src/main.rs", "def456", "rust").unwrap();
        assert_eq!(id, id2);
        let file = db.get_file_by_path("src/main.rs").unwrap().unwrap();
        assert_eq!(file.content_hash, "def456");

        // Delete
        db.delete_file("src/main.rs").unwrap();
        assert!(db.get_file_by_path("src/main.rs").unwrap().is_none());
    }

    #[test]
    fn test_symbol_crud() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file("src/lib.rs", "hash", "rust").unwrap();

        let sym = ParsedSymbol {
            name: "my_function".to_string(),
            kind: "fn".to_string(),
            line: 10,
            column: 0,
            end_line: 15,
            visibility: "pub".to_string(),
            scope: Some("MyStruct".to_string()),
        };
        let sym_id = db.insert_symbol(file_id, &sym).unwrap();
        assert!(sym_id > 0);

        let results = db
            .query_symbols(None, None, None, None, None, None, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name, "my_function");

        // Filter by kind
        let results = db
            .query_symbols(None, Some("fn"), None, None, None, None, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        let results = db
            .query_symbols(None, Some("struct"), None, None, None, None, None)
            .unwrap();
        assert_eq!(results.len(), 0);

        // Filter by grep
        let results = db
            .query_symbols(None, None, Some("my_func"), None, None, None, None)
            .unwrap();
        assert_eq!(results.len(), 1);

        // Filter by scope
        let results = db
            .query_symbols(None, None, None, Some("MyStruct"), None, None, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name, "my_function");

        // Non-matching scope returns nothing
        let results = db
            .query_symbols(None, None, None, Some("NonExistent"), None, None, None)
            .unwrap();
        assert_eq!(results.len(), 0);

        // Filter by visibility
        let results = db
            .query_symbols(None, None, None, None, Some("pub"), None, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.visibility, "pub");

        // Non-matching visibility returns nothing
        let results = db
            .query_symbols(None, None, None, None, Some("private"), None, None)
            .unwrap();
        assert_eq!(results.len(), 0);

        // Delete
        db.delete_symbols_for_file(file_id).unwrap();
        assert_eq!(db.symbol_count().unwrap(), 0);
    }

    #[test]
    fn test_imports() {
        let db = Database::open_in_memory().unwrap();
        let file_id = db.upsert_file("src/main.rs", "hash", "rust").unwrap();

        let imp = ParsedImport {
            import_path: "std::collections::HashMap".to_string(),
            alias: None,
            names: Vec::new(),
        };
        db.insert_import(file_id, &imp).unwrap();

        let imports = db.get_imports_for_file(file_id).unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].import_path, "std::collections::HashMap");
    }

    /// Resolved imports make both directions answerable from the file's own
    /// path: dependencies report the target file (not the specifier) and
    /// dependents find every importer whatever each one spelled.
    #[test]
    fn resolved_imports_key_the_graph_by_file() {
        let db = Database::open_in_memory().unwrap();
        let money = db
            .upsert_file("src/util/money.ts", "h", "typescript")
            .unwrap();
        let cart = db
            .upsert_file("src/domain/cart.ts", "h", "typescript")
            .unwrap();
        let app = db.upsert_file("src/app.ts", "h", "typescript").unwrap();

        let add = |file_id: i64, path: &str, resolved: Option<i64>| {
            let id = db
                .insert_import(
                    file_id,
                    &ParsedImport {
                        import_path: path.to_string(),
                        alias: None,
                        names: Vec::new(),
                    },
                )
                .unwrap();
            db.apply_import_resolutions(&[(id, resolved)]).unwrap();
        };
        // Same target, two spellings — plus one package import that resolves to
        // no file.
        add(cart, "../util/money", Some(money));
        add(app, "./util/money", Some(money));
        add(app, "react", None);

        assert_eq!(
            db.file_dependents("src/util/money.ts").unwrap(),
            vec!["src/app.ts".to_string(), "src/domain/cart.ts".to_string()]
        );
        // Unresolved imports keep reporting their raw specifier.
        assert_eq!(
            db.file_dependencies("src/app.ts").unwrap(),
            vec!["react".to_string(), "src/util/money.ts".to_string()]
        );
        // A target that is not an indexed file still matches specifier text.
        assert_eq!(
            db.file_dependents("../util/money").unwrap(),
            vec!["src/domain/cart.ts".to_string()]
        );
    }

    #[test]
    fn test_metadata() {
        let db = Database::open_in_memory().unwrap();
        db.set_metadata("last_commit", "abc123").unwrap();
        assert_eq!(
            db.get_metadata("last_commit").unwrap(),
            Some("abc123".to_string())
        );

        db.set_metadata("last_commit", "def456").unwrap();
        assert_eq!(
            db.get_metadata("last_commit").unwrap(),
            Some("def456".to_string())
        );

        assert!(db.get_metadata("nonexistent").unwrap().is_none());
    }

    #[test]
    fn test_fresh_db_has_docid_column_and_index() {
        let db = Database::open_in_memory().unwrap();

        // Column exists and is queryable
        assert!(db.conn.prepare("SELECT docid FROM symbols LIMIT 0").is_ok());

        // Column is nullable: inserting through the existing symbol path
        // (which never mentions docid) reads back NULL
        let file_id = db.upsert_file("src/a.cs", "hash", "csharp").unwrap();
        let sym = ParsedSymbol {
            name: "Greet".to_string(),
            kind: "fn".to_string(),
            line: 3,
            column: 4,
            end_line: 5,
            visibility: "pub".to_string(),
            scope: Some("Person".to_string()),
        };
        let sym_id = db.insert_symbol(file_id, &sym).unwrap();
        let docid: Option<String> = db
            .conn
            .query_row(
                "SELECT docid FROM symbols WHERE id = ?1",
                params![sym_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(docid.is_none());

        // Index exists
        let mut stmt = db.conn.prepare("PRAGMA index_list('symbols')").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(
            names.iter().any(|n| n == "idx_symbols_docid"),
            "idx_symbols_docid missing from {names:?}"
        );
    }

    #[test]
    fn test_legacy_db_migrates_docid() {
        // Hand-create a DB with the v0.15.0 schema (no docid column) and rows,
        // then reopen through Database::open to exercise the migration.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (
                    id INTEGER PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE,
                    content_hash TEXT NOT NULL,
                    language TEXT NOT NULL,
                    last_indexed_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE symbols (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    line INTEGER NOT NULL,
                    column INTEGER NOT NULL,
                    end_line INTEGER NOT NULL DEFAULT 0,
                    visibility TEXT NOT NULL DEFAULT 'private',
                    scope TEXT
                );
                CREATE TABLE imports (
                    id INTEGER PRIMARY KEY,
                    source_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    import_path TEXT NOT NULL,
                    alias TEXT,
                    resolved_file_id INTEGER REFERENCES files(id) ON DELETE SET NULL
                );
                CREATE TABLE references_ (
                    id INTEGER PRIMARY KEY,
                    symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
                    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    line INTEGER NOT NULL,
                    column INTEGER NOT NULL
                );
                CREATE TABLE metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO files (id, path, content_hash, language) VALUES (1, 'src/a.cs', 'h1', 'csharp');
                INSERT INTO symbols (name, kind, file_id, line, column, end_line, visibility, scope)
                    VALUES ('Greet', 'fn', 1, 3, 4, 5, 'pub', 'Person');
                INSERT INTO symbols (name, kind, file_id, line, column, end_line, visibility, scope)
                    VALUES ('Person', 'class', 1, 1, 0, 10, 'pub', NULL);
                INSERT INTO references_ (symbol_id, file_id, line, column) VALUES (1, 1, 8, 2);",
            )
            .unwrap();
        }

        let db = Database::open(&db_path).unwrap();

        // No data loss
        assert_eq!(db.symbol_count().unwrap(), 2);
        assert_eq!(db.file_count().unwrap(), 1);
        let ref_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM references_", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ref_count, 1);

        // All prior rows have docid = NULL
        let null_docids: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM symbols WHERE docid IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(null_docids, 2);

        // Existing row data intact
        let results = db
            .query_symbols(None, None, Some("Greet"), None, None, None, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.scope.as_deref(), Some("Person"));

        // Index created by migration
        let mut stmt = db.conn.prepare("PRAGMA index_list('symbols')").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(names.iter().any(|n| n == "idx_symbols_docid"));
    }

    #[test]
    fn test_legacy_db_migrates_container_symbol_id() {
        // Hand-create a DB whose references_ table predates container_symbol_id,
        // then reopen through Database::open to exercise the migration.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("legacy.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE files (
                    id INTEGER PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE,
                    content_hash TEXT NOT NULL,
                    language TEXT NOT NULL,
                    last_indexed_at TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE TABLE symbols (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    line INTEGER NOT NULL,
                    column INTEGER NOT NULL,
                    end_line INTEGER NOT NULL DEFAULT 0,
                    visibility TEXT NOT NULL DEFAULT 'private',
                    scope TEXT,
                    docid TEXT
                );
                CREATE TABLE imports (
                    id INTEGER PRIMARY KEY,
                    source_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    import_path TEXT NOT NULL,
                    alias TEXT,
                    resolved_file_id INTEGER REFERENCES files(id) ON DELETE SET NULL
                );
                CREATE TABLE references_ (
                    id INTEGER PRIMARY KEY,
                    symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
                    file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                    line INTEGER NOT NULL,
                    column INTEGER NOT NULL,
                    qualified INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE metadata (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO files (id, path, content_hash, language) VALUES (1, 'src/a.cs', 'h1', 'csharp');
                INSERT INTO symbols (name, kind, file_id, line, column, end_line, visibility, scope)
                    VALUES ('Greet', 'fn', 1, 3, 4, 5, 'pub', 'Person');
                INSERT INTO references_ (symbol_id, file_id, line, column) VALUES (1, 1, 8, 2);",
            )
            .unwrap();
        }

        let db = Database::open(&db_path).unwrap();

        // No data loss
        let ref_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM references_", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ref_count, 1);

        // The pre-existing row has container_symbol_id = NULL
        let container: Option<i64> = db
            .conn
            .query_row(
                "SELECT container_symbol_id FROM references_ WHERE symbol_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(container.is_none());

        // A new insert through the current API works
        db.insert_reference(1, 1, 9, 0, false, None).unwrap();
        let ref_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM references_", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ref_count, 2);

        // Index created by migration
        let mut stmt = db.conn.prepare("PRAGMA index_list('references_')").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert!(
            names.iter().any(|n| n == "idx_refs_container"),
            "idx_refs_container missing from {names:?}"
        );
    }
}
