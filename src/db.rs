use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Database {
    pub conn: Connection,
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

#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct ReferenceRecord {
    pub id: i64,
    pub symbol_id: i64,
    pub file_id: i64,
    pub line: i64,
    pub column: i64,
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
}

/// Parsed reference data before insertion
#[derive(Debug, Clone)]
pub struct ParsedReference {
    pub symbol_name: String,
    pub line: i64,
    pub column: i64,
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
                scope TEXT
            );

            CREATE TABLE IF NOT EXISTS imports (
                id INTEGER PRIMARY KEY,
                source_file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                import_path TEXT NOT NULL,
                alias TEXT,
                resolved_file_id INTEGER REFERENCES files(id) ON DELETE SET NULL
            );

            CREATE TABLE IF NOT EXISTS references_ (
                id INTEGER PRIMARY KEY,
                symbol_id INTEGER NOT NULL REFERENCES symbols(id) ON DELETE CASCADE,
                file_id INTEGER NOT NULL REFERENCES files(id) ON DELETE CASCADE,
                line INTEGER NOT NULL,
                column INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_symbols_file ON symbols(file_id);
            CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
            CREATE INDEX IF NOT EXISTS idx_symbols_kind ON symbols(kind);
            CREATE INDEX IF NOT EXISTS idx_imports_source ON imports(source_file_id);
            CREATE INDEX IF NOT EXISTS idx_refs_symbol ON references_(symbol_id);
            CREATE INDEX IF NOT EXISTS idx_refs_file ON references_(file_id);
            CREATE INDEX IF NOT EXISTS idx_files_path ON files(path);",
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

    pub fn query_symbols(
        &self,
        file: Option<&str>,
        kind: Option<&str>,
        grep: Option<&str>,
        scope: Option<&str>,
        visibility: Option<&str>,
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
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_imports_for_file(&self, file_id: i64) -> Result<()> {
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

    /// What does this file import (outgoing deps)?
    pub fn file_dependencies(&self, path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT i.import_path
             FROM imports i JOIN files f ON i.source_file_id = f.id
             WHERE f.path = ?1 ORDER BY i.import_path",
        )?;
        let rows = stmt.query_map(params![path], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying file dependencies")
    }

    /// What files import this file/module (incoming deps)?
    pub fn file_dependents(&self, path: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT f.path
             FROM imports i JOIN files f ON i.source_file_id = f.id
             WHERE i.import_path LIKE ?1 ORDER BY f.path",
        )?;
        let rows = stmt.query_map(params![format!("%{path}%")], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying file dependents")
    }

    /// What does a symbol depend on (via its file's imports)?
    pub fn symbol_dependencies(&self, symbol_name: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT i.import_path
             FROM symbols s
             JOIN imports i ON i.source_file_id = s.file_id
             WHERE s.name = ?1
             ORDER BY i.import_path",
        )?;
        let rows = stmt.query_map(params![symbol_name], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying symbol dependencies")
    }

    /// What references point to this symbol (reverse deps)?
    pub fn symbol_references(&self, symbol_name: &str) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.path, r.line, r.column
             FROM references_ r
             JOIN symbols s ON r.symbol_id = s.id
             JOIN files f ON r.file_id = f.id
             WHERE s.name = ?1
             ORDER BY f.path, r.line",
        )?;
        let rows = stmt.query_map(params![symbol_name], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("querying symbol references")
    }

    // --- Reference operations ---

    pub fn insert_reference(
        &self,
        symbol_id: i64,
        file_id: i64,
        line: i64,
        column: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO references_ (symbol_id, file_id, line, column) VALUES (?1, ?2, ?3, ?4)",
            params![symbol_id, file_id, line, column],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn delete_references_for_file(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM references_ WHERE file_id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    // --- Metadata operations ---

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

        let results = db.query_symbols(None, None, None, None, None).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name, "my_function");

        // Filter by kind
        let results = db
            .query_symbols(None, Some("fn"), None, None, None)
            .unwrap();
        assert_eq!(results.len(), 1);
        let results = db
            .query_symbols(None, Some("struct"), None, None, None)
            .unwrap();
        assert_eq!(results.len(), 0);

        // Filter by grep
        let results = db
            .query_symbols(None, None, Some("my_func"), None, None)
            .unwrap();
        assert_eq!(results.len(), 1);

        // Filter by scope
        let results = db
            .query_symbols(None, None, None, Some("MyStruct"), None)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.name, "my_function");

        // Non-matching scope returns nothing
        let results = db
            .query_symbols(None, None, None, Some("NonExistent"), None)
            .unwrap();
        assert_eq!(results.len(), 0);

        // Filter by visibility
        let results = db
            .query_symbols(None, None, None, None, Some("pub"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.visibility, "pub");

        // Non-matching visibility returns nothing
        let results = db
            .query_symbols(None, None, None, None, Some("private"))
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
        };
        db.insert_import(file_id, &imp).unwrap();

        let imports = db.get_imports_for_file(file_id).unwrap();
        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].import_path, "std::collections::HashMap");
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
}
