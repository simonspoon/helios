use std::path::PathBuf;
use std::process::Command;

/// Helper: initialize a test project and return (temp_dir, helios_binary_path)
fn setup_indexed_project() -> (tempfile::TempDir, PathBuf) {
    let dir = create_test_project();
    let bin = helios_bin();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed during setup");
    (dir, bin)
}

fn helios_bin() -> PathBuf {
    // Cargo builds the binary before the test binary runs and passes its path
    // in. Shelling out to `cargo build` here instead raced: every test called
    // this, so dozens of builds ran at once and one replacing `target/debug/
    // helios` made another test's spawn fail with NotFound.
    PathBuf::from(env!("CARGO_BIN_EXE_helios"))
}

/// Number of raw reference rows linked to symbols named `name`.
///
/// The indexer writes one row per candidate definition, so this counts the
/// candidate links — which `deps` deliberately collapses to unique usage sites
/// (task 837). Tests about resolution must read the rows, not the deps output.
fn reference_rows(project: &std::path::Path, name: &str) -> i64 {
    let conn = rusqlite::Connection::open(project.join(".helios").join("index.db"))
        .expect("opening index.db");
    conn.query_row(
        "SELECT COUNT(*) FROM references_ r JOIN symbols s ON r.symbol_id = s.id
         WHERE s.name = ?1",
        [name],
        |row| row.get(0),
    )
    .expect("counting reference rows")
}

fn create_test_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("creating temp dir");

    // Create some sample files
    std::fs::write(
        dir.path().join("main.rs"),
        r#"
use std::collections::HashMap;

pub fn main() {
    let map = HashMap::new();
    helper();
}

fn helper() -> i32 {
    42
}

pub struct Config {
    pub name: String,
    pub value: i32,
}

pub trait Processor {
    fn process(&self) -> bool;
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("lib.py"),
        r#"
import os
from pathlib import Path

MAX_SIZE = 100

class FileHandler:
    def __init__(self, path):
        self.path = path

    def read(self):
        return Path(self.path).read_text()

def process_files():
    handler = FileHandler("test.txt")
    return handler.read()
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("server.go"),
        r#"
package main

import (
    "fmt"
    "net/http"
)

type Server struct {
    Port int
}

func NewServer(port int) *Server {
    return &Server{Port: port}
}

func (s *Server) Start() error {
    fmt.Println("Starting server")
    return nil
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("app.ts"),
        r#"
import { useState } from 'react';

export interface AppConfig {
    title: string;
    debug: boolean;
}

export function createApp(config: AppConfig): void {
    console.log(config.title);
}

class AppState {
    private ready: boolean = false;

    init(): void {
        this.ready = true;
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("Models.cs"),
        r#"
using System;
using System.Collections.Generic;

namespace MyApp.Models {
    public class Person {
        public string Name { get; set; }
        public int Age { get; set; }

        public Person(string name, int age) {
            Name = name;
            Age = age;
        }

        public void Greet() {
            Console.WriteLine("Hello " + Name);
        }
    }

    public interface IRepository<T> {
        T GetById(int id);
        void Delete(int id);
    }

    public enum Status {
        Active,
        Inactive
    }

    public record Point(int X, int Y);

    public struct Vector {
        public double X;
        public double Y;
    }
}
"#,
    )
    .unwrap();

    dir
}

#[test]
fn test_init_creates_database() {
    let dir = create_test_project();
    let bin = helios_bin();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("running helios init");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "helios init failed:\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );

    // Database should exist
    assert!(dir.path().join(".helios/index.db").exists());

    // Should have indexed files
    assert!(stdout.contains("Indexed"));
    assert!(stdout.contains("files"));
}

#[test]
fn test_init_json_output() {
    let dir = create_test_project();
    let bin = helios_bin();

    let output = Command::new(&bin)
        .args(["--json", "init"])
        .current_dir(dir.path())
        .output()
        .expect("running helios init --json");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON output");

    assert!(json["files_indexed"].as_u64().unwrap() >= 4);
    assert!(json["total_symbols"].as_u64().unwrap() > 0);
}

#[test]
fn test_symbols_query() {
    let dir = create_test_project();
    let bin = helios_bin();

    // Init first
    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    // Query all symbols
    let output = Command::new(&bin)
        .args(["symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main"));
    assert!(stdout.contains("Config"));

    // Query by kind
    let output = Command::new(&bin)
        .args(["symbols", "--kind", "fn"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --kind fn");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("fn"));
    assert!(!stdout.contains("struct"));
    assert!(!stdout.contains("class"));

    // Query by file
    let output = Command::new(&bin)
        .args(["symbols", "--file", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --file");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main.rs"));
    assert!(!stdout.contains("lib.py"));

    // Query by grep
    let output = Command::new(&bin)
        .args(["symbols", "--grep", "Config"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Config"));
}

#[test]
fn test_symbols_json() {
    let dir = create_test_project();
    let bin = helios_bin();

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    let output = Command::new(&bin)
        .args(["--json", "symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");
    assert!(!json.as_array().unwrap().is_empty());
}

#[test]
fn test_deps_command() {
    let dir = create_test_project();
    let bin = helios_bin();

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    // Test file deps
    let output = Command::new(&bin)
        .args(["deps", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("deps");

    assert!(output.status.success());

    // Test symbol deps
    let output = Command::new(&bin)
        .args(["deps", "main"])
        .current_dir(dir.path())
        .output()
        .expect("deps symbol");

    assert!(output.status.success());
}

/// `deps <file path>` answers "who imports this file" from the file's own path,
/// collecting importers that spelled the specifier differently, and traverses
/// transitively because the graph edge is now file -> file.
#[test]
fn test_deps_file_dependents_by_path() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::create_dir_all(dir.path().join("src/util")).unwrap();
    std::fs::create_dir_all(dir.path().join("src/domain")).unwrap();
    std::fs::write(
        dir.path().join("src/util/money.ts"),
        "export function money() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/domain/cart.ts"),
        "import { money } from '../util/money';\nexport function cart() { money(); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/app.ts"),
        "import { money } from './util/money';\nimport { cart } from './domain/cart';\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");
    assert!(output.status.success());

    let output = Command::new(&bin)
        .args(["--json", "deps", "src/util/money.ts"])
        .current_dir(dir.path())
        .output()
        .expect("deps");
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("parsing deps JSON");
    let dependents: Vec<&str> = json["dependents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        dependents,
        vec!["src/app.ts", "src/domain/cart.ts"],
        "both spellings of the specifier resolve to the same file"
    );

    // Transitive: app -> cart -> money, reachable only via resolved edges.
    let output = Command::new(&bin)
        .args(["--json", "deps", "src/util/money.ts", "--depth", "2"])
        .current_dir(dir.path())
        .output()
        .expect("deps depth");
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("parsing deps JSON");
    let deep: Vec<(&str, u64)> = json["dependents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["path"].as_str().unwrap(), d["depth"].as_u64().unwrap()))
        .collect();
    assert!(
        deep.contains(&("src/app.ts", 1)) && deep.contains(&("src/domain/cart.ts", 1)),
        "direct importers at depth 1, got {deep:?}"
    );

    let output = Command::new(&bin)
        .args(["--json", "deps", "src/app.ts"])
        .current_dir(dir.path())
        .output()
        .expect("deps app");
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("parsing deps JSON");
    let deps: Vec<&str> = json["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        deps,
        vec!["src/domain/cart.ts", "src/util/money.ts"],
        "outgoing edges report the resolved file path"
    );
}

/// `implementors_of`'s raw-name fallback matches purely on `super_name` text,
/// with no language or file scoping (accepted by design — dropping an
/// unresolved edge would defeat the point of the feature). So a TypeScript
/// `Sub` whose supertype `Base` never resolved WILL surface when the user
/// asks `deps Base` about an unrelated Python `class Base` in the same repo.
/// The mitigation is disclosure: every such row is marked `external`, shows
/// its own declaring file:line, and the answer ends with a provenance line
/// naming which languages actually contributed an edge — so the reader can
/// see the hit came from a `.ts` file, not the Python one they asked about.
///
/// Seeds the unresolved row directly rather than indexing a real Python
/// `class Sub(Base)` alongside it: constructing a genuine cross-language
/// collision would couple this test to two parsers' behaviour (does Python's
/// leg resolve `Base` first and rob TypeScript's row of the case we want?)
/// for no added value — the thing under test is `deps`'s handling of an
/// unresolved row, not how one gets produced. The seeded shape matches what
/// a tree-sitter leg emits for an unrecognized base type: `super_symbol_id`
/// NULL, `super_name` raw.
#[test]
fn test_deps_type_edge_external_cross_language_shows_declaring_file() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::write(dir.path().join("base.py"), "class Base:\n    pass\n").unwrap();
    std::fs::write(dir.path().join("sub.ts"), "export class Sub {}\n").unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");
    assert!(output.status.success());

    {
        let conn = rusqlite::Connection::open(dir.path().join(".helios").join("index.db"))
            .expect("opening index.db");
        let sub_file_id: i64 = conn
            .query_row(
                "SELECT id FROM files WHERE path = 'sub.ts'",
                [],
                |row| row.get(0),
            )
            .expect("finding sub.ts file row");
        let sub_symbol_id: i64 = conn
            .query_row(
                "SELECT id FROM symbols WHERE name = 'Sub'",
                [],
                |row| row.get(0),
            )
            .expect("finding Sub symbol row");
        conn.execute(
            "INSERT INTO type_relations (sub_symbol_id, sub_name, super_symbol_id, super_name, kind, file_id)
             VALUES (?1, 'Sub', NULL, 'Base', 'implements', ?2)",
            rusqlite::params![sub_symbol_id, sub_file_id],
        )
        .expect("seeding unresolved type relation");
    }

    let output = Command::new(&bin)
        .args(["--json", "deps", "Base", "--file", "base.py"])
        .current_dir(dir.path())
        .output()
        .expect("deps Base");
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&output.stdout)).expect("parsing deps JSON");
    let implementors = json["implementors"].as_array().unwrap();
    assert_eq!(implementors.len(), 1);
    assert_eq!(implementors[0]["sub_name"], "Sub");
    assert_eq!(implementors[0]["file"], "sub.ts");
    assert_eq!(implementors[0]["language"], "typescript");
    assert_eq!(implementors[0]["external"], true);
    assert_eq!(json["edge_languages"], serde_json::json!(["typescript"]));

    let output = Command::new(&bin)
        .args(["deps", "Base", "--file", "base.py"])
        .current_dir(dir.path())
        .output()
        .expect("deps Base human");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("sub.ts:1 Sub -> Base (implements, external)"),
        "expected the declaring file:line on the external row, got:\n{stdout}"
    );
    assert!(
        stdout.contains("Type edges from: typescript"),
        "expected the provenance line to name only typescript, got:\n{stdout}"
    );
}

#[test]
fn test_summary_command() {
    let dir = create_test_project();
    let bin = helios_bin();

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    let output = Command::new(&bin)
        .args(["summary"])
        .current_dir(dir.path())
        .output()
        .expect("summary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Project Summary"));
    assert!(stdout.contains("Files:"));
    assert!(stdout.contains("Symbols:"));
}

#[test]
fn test_export_command() {
    let dir = create_test_project();
    let bin = helios_bin();

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    let output = Command::new(&bin)
        .args(["export"])
        .current_dir(dir.path())
        .output()
        .expect("export");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Code Index"));
    assert!(stdout.contains("main.rs"));
}

#[test]
fn test_no_index_error() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // Running commands without init should fail gracefully
    let output = Command::new(&bin)
        .args(["symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No index found") || stderr.contains("helios init"));
}

#[test]
fn test_incremental_update() {
    let dir = create_test_project();
    let bin = helios_bin();

    // Init
    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    // Add a new file
    std::fs::write(
        dir.path().join("new_module.rs"),
        r#"
pub fn new_function() -> String {
    "hello".to_string()
}
"#,
    )
    .unwrap();

    // Update (will do full re-index since no git)
    let output = Command::new(&bin)
        .arg("update")
        .current_dir(dir.path())
        .output()
        .expect("update");

    assert!(output.status.success());

    // Verify new symbol exists
    let output = Command::new(&bin)
        .args(["symbols", "--grep", "new_function"])
        .current_dir(dir.path())
        .output()
        .expect("symbols");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("new_function"),
        "new_function should be in index after update"
    );
}

#[test]
fn test_multi_language_index() {
    let dir = create_test_project();
    let bin = helios_bin();

    let output = Command::new(&bin)
        .args(["--json", "init"])
        .current_dir(dir.path())
        .output()
        .expect("init");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");

    // Should index all 5 files (rs, py, go, ts, cs)
    assert!(
        json["files_indexed"].as_u64().unwrap() >= 5,
        "Should index at least 5 files, got: {}",
        json["files_indexed"]
    );

    // Check symbols from each language exist
    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let output = Command::new(&bin)
        .args(["--json", "symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // Check we have symbols from different files
    let files: std::collections::HashSet<String> = symbols
        .iter()
        .map(|s| s["file"].as_str().unwrap().to_string())
        .collect();

    assert!(files.contains("main.rs"), "should have Rust symbols");
    assert!(files.contains("lib.py"), "should have Python symbols");
    assert!(files.contains("server.go"), "should have Go symbols");
    assert!(files.contains("app.ts"), "should have TypeScript symbols");
    assert!(files.contains("Models.cs"), "should have C# symbols");
}

#[test]
fn test_csharp_indexing() {
    let dir = create_test_project();
    let bin = helios_bin();

    // Init the project
    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    // Query C# symbols by file
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--file", "Models.cs"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --file Models.cs");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // Collect symbol names and kinds
    let sym_info: Vec<(String, String)> = symbols
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_string(),
                s["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();

    // Verify key C# symbols were extracted
    assert!(
        sym_info.iter().any(|(n, k)| n == "Person" && k == "class"),
        "Should find Person class, got: {:?}",
        sym_info
    );
    assert!(
        sym_info
            .iter()
            .any(|(n, k)| n == "IRepository" && k == "interface"),
        "Should find IRepository interface, got: {:?}",
        sym_info
    );
    assert!(
        sym_info.iter().any(|(n, k)| n == "Status" && k == "enum"),
        "Should find Status enum, got: {:?}",
        sym_info
    );
    assert!(
        sym_info.iter().any(|(n, k)| n == "Vector" && k == "struct"),
        "Should find Vector struct, got: {:?}",
        sym_info
    );
    assert!(
        sym_info.iter().any(|(n, k)| n == "Greet" && k == "fn"),
        "Should find Greet method, got: {:?}",
        sym_info
    );
    assert!(
        sym_info.iter().any(|(n, k)| n == "Name" && k == "fn"),
        "Should find Name property, got: {:?}",
        sym_info
    );

    // Verify namespace was captured
    assert!(
        sym_info
            .iter()
            .any(|(n, k)| n == "MyApp.Models" && k == "mod"),
        "Should find MyApp.Models namespace, got: {:?}",
        sym_info
    );

    // Verify the file is recognized as csharp
    let output = Command::new(&bin)
        .args(["--json", "summary"])
        .current_dir(dir.path())
        .output()
        .expect("summary");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Models.cs"),
        "Summary should include the C# file"
    );
}

/// The default (non-JSON) symbols output must qualify names with their scope,
/// so same-named symbols in different classes are distinguishable.
#[test]
fn test_symbols_text_output_includes_scope() {
    let dir = create_test_project();
    let bin = helios_bin();

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    let output = Command::new(&bin)
        .args(["symbols", "--file", "Models.cs", "--grep", "Greet"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep Greet");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("fn pub Person.Greet"),
        "Text output should qualify Greet with its Person scope, got: {}",
        stdout
    );
}

/// An ambiguous C# method name (declared in 2+ classes) must link a usage to
/// ALL candidate definitions rather than silently picking one. Unambiguous
/// names must still resolve to their single definition (no regression).
#[test]
fn test_csharp_ambiguous_reference_links_all_candidates() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    std::fs::write(
        dir.path().join("Ambiguous.cs"),
        r#"
namespace App {
    public class Alpha {
        public void Compute() { }
    }

    public class Beta {
        public void Compute() { }
    }

    public class Solo {
        public void OnlyHere() { }
    }

    public class Runner {
        public void Run() {
            Compute();
            OnlyHere();
        }
    }
}
"#,
    )
    .unwrap();

    let bin = helios_bin();
    let init = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");
    assert!(init.status.success(), "helios init failed");

    // "Compute" is declared in both Alpha and Beta. The single call site must be
    // linked to BOTH candidate definitions (2 reference rows), not one arbitrary
    // pick (which would yield 1) and not dropped (which would yield 0).
    assert_eq!(
        reference_rows(dir.path(), "Compute"),
        2,
        "ambiguous 'Compute' usage should link to both definitions"
    );

    // "OnlyHere" is declared once — no regression: exactly one linked usage.
    assert_eq!(
        reference_rows(dir.path(), "OnlyHere"),
        1,
        "unambiguous 'OnlyHere' usage should link to its single definition"
    );
}

/// Scope-aware C# resolution: a call to a method defined in the caller's OWN
/// class must resolve to that class's definition, not a same-named method in a
/// sibling class. When the name is ambiguous but no candidate shares the caller's
/// scope, resolution still links to all candidates (story 174 behavior preserved).
#[test]
fn test_csharp_scope_aware_reference_resolution() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    std::fs::write(
        dir.path().join("Scoped.cs"),
        r#"
namespace App {
    public class Alpha {
        public void Compute() { }

        public void Run() {
            // In-scope call: Alpha also declares Compute, so this must resolve
            // to Alpha.Compute ONLY, not Beta.Compute.
            Compute();
        }
    }

    public class Beta {
        public void Compute() { }
    }

    public class Gamma {
        public void Shared() { }
    }

    public class Delta {
        public void Shared() { }
    }

    public class Runner {
        public void Go() {
            // Runner declares no Shared method — no scope match, so this stays
            // ambiguous and links to BOTH Gamma.Shared and Delta.Shared.
            Shared();
        }
    }
}
"#,
    )
    .unwrap();

    let bin = helios_bin();
    let init = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");
    assert!(init.status.success(), "helios init failed");

    // "Compute" is declared in Alpha and Beta. The single call site lives inside
    // Alpha, so scope-aware resolution narrows it to Alpha.Compute alone — exactly
    // ONE linked usage. Under the pre-scope (story 174) behavior this would have
    // linked to both definitions, yielding 2.
    assert_eq!(
        reference_rows(dir.path(), "Compute"),
        1,
        "in-scope 'Compute' call should resolve to its own class only"
    );

    // "Shared" is declared in Gamma and Delta; the call site (Runner) matches
    // neither scope, so it stays ambiguous and links to BOTH definitions.
    assert_eq!(
        reference_rows(dir.path(), "Shared"),
        2,
        "unresolvable-by-scope 'Shared' call should link to all candidates"
    );
}

#[test]
fn test_compact_symbols_json() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["--json", "--compact", "symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json --compact");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    // Compact output must be a single line
    assert_eq!(
        trimmed.lines().count(),
        1,
        "compact JSON should be a single line, got:\n{}",
        trimmed
    );

    // Must be valid JSON (array of symbols)
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("compact output must be valid JSON");
    assert!(parsed.is_array(), "symbols output should be a JSON array");
    assert!(
        !parsed.as_array().unwrap().is_empty(),
        "symbols array should not be empty"
    );
}

#[test]
fn test_compact_export_json() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["--json", "--compact", "export"])
        .current_dir(dir.path())
        .output()
        .expect("export --json --compact");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    // Compact output must be a single line
    assert_eq!(
        trimmed.lines().count(),
        1,
        "compact JSON should be a single line, got:\n{}",
        trimmed
    );

    // Must be valid JSON with expected fields
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("compact output must be valid JSON");
    assert!(
        parsed["files"].is_array(),
        "export should contain 'files' array"
    );
    assert!(
        parsed["total_files"].as_u64().unwrap() >= 4,
        "export should report at least 4 files"
    );
}

#[test]
fn test_compact_summary_json() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["--json", "--compact", "summary"])
        .current_dir(dir.path())
        .output()
        .expect("summary --json --compact");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    // Compact output must be a single line
    assert_eq!(
        trimmed.lines().count(),
        1,
        "compact JSON should be a single line, got:\n{}",
        trimmed
    );

    // Must be valid JSON with expected fields
    let parsed: serde_json::Value =
        serde_json::from_str(trimmed).expect("compact output must be valid JSON");
    assert!(
        parsed["total_symbols"].as_u64().unwrap() > 0,
        "summary should report symbols"
    );
    assert!(
        parsed["directories"].is_object(),
        "summary should contain 'directories' object"
    );
}

#[test]
fn test_compact_vs_pretty_difference() {
    let (dir, bin) = setup_indexed_project();

    // Get pretty output
    let pretty_output = Command::new(&bin)
        .args(["--json", "symbols", "--kind", "fn"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json (pretty)");

    // Get compact output
    let compact_output = Command::new(&bin)
        .args(["--json", "--compact", "symbols", "--kind", "fn"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json --compact");

    let pretty = String::from_utf8_lossy(&pretty_output.stdout);
    let compact = String::from_utf8_lossy(&compact_output.stdout);

    // Pretty should have multiple lines, compact should have one
    assert!(
        pretty.trim().lines().count() > 1,
        "pretty output should span multiple lines"
    );
    assert_eq!(
        compact.trim().lines().count(),
        1,
        "compact output should be a single line"
    );

    // Both should parse to the same JSON value
    let pretty_val: serde_json::Value = serde_json::from_str(pretty.trim()).expect("pretty JSON");
    let compact_val: serde_json::Value =
        serde_json::from_str(compact.trim()).expect("compact JSON");
    assert_eq!(
        pretty_val, compact_val,
        "pretty and compact should produce identical data"
    );
}

#[test]
fn test_symbols_body_text_mode() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["symbols", "--body", "--file", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --body --file main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain header lines with file:line-end_line format
    assert!(
        stdout.contains("--- main.rs:"),
        "body output should contain header lines, got:\n{}",
        stdout
    );

    // Should contain actual function body content
    assert!(
        stdout.contains("pub fn main()"),
        "body should contain main function definition, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("helper()"),
        "body should contain helper call inside main, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("fn helper() -> i32"),
        "body should contain helper function definition, got:\n{}",
        stdout
    );
}

#[test]
fn test_symbols_body_json_mode() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["--json", "symbols", "--body", "--file", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --body --json --file main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // Every symbol should have a "body" field
    for sym in &symbols {
        assert!(
            sym.get("body").is_some(),
            "symbol {} should have a body field, got: {:?}",
            sym["name"],
            sym
        );
    }

    // Find the main function and verify its body content
    let main_sym = symbols
        .iter()
        .find(|s| s["name"] == "main" && s["kind"] == "fn")
        .expect("should find main function");

    let body = main_sym["body"].as_str().expect("body should be a string");
    assert!(
        body.contains("pub fn main()"),
        "main body should contain function signature, got: {}",
        body
    );
    assert!(
        body.contains("HashMap::new()"),
        "main body should contain HashMap::new() call, got: {}",
        body
    );

    // Find Config struct and verify its body
    let config_sym = symbols
        .iter()
        .find(|s| s["name"] == "Config" && s["kind"] == "struct")
        .expect("should find Config struct");

    let body = config_sym["body"]
        .as_str()
        .expect("body should be a string");
    assert!(
        body.contains("pub struct Config"),
        "Config body should contain struct definition, got: {}",
        body
    );
    assert!(
        body.contains("pub name: String"),
        "Config body should contain name field, got: {}",
        body
    );
    assert!(
        body.contains("pub value: i32"),
        "Config body should contain value field, got: {}",
        body
    );
}

#[test]
fn test_symbols_body_kind_filter() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["symbols", "--body", "--kind", "struct", "--file", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --body --kind struct --file main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain struct body
    assert!(
        stdout.contains("pub struct Config"),
        "should show Config struct body, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("pub name: String"),
        "should contain struct fields, got:\n{}",
        stdout
    );

    // Should NOT contain function bodies (filtered to structs only)
    assert!(
        !stdout.contains("fn main()"),
        "should not contain fn main when filtered to structs, got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("fn helper()"),
        "should not contain fn helper when filtered to structs, got:\n{}",
        stdout
    );
}

#[test]
fn test_symbols_body_matches_source() {
    let (dir, bin) = setup_indexed_project();

    // Read the actual source file
    let source = std::fs::read_to_string(dir.path().join("main.rs")).expect("reading main.rs");

    // Get symbols with body in JSON
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--body", "--file", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --body --json --file main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // For each symbol, verify its body is a substring of the actual source
    for sym in &symbols {
        if let Some(body) = sym["body"].as_str() {
            assert!(
                source.contains(body),
                "body for {} should be found in source file.\nbody: {:?}\nsource excerpt around line {}: {:?}",
                sym["name"],
                body,
                sym["line"],
                source
                    .lines()
                    .skip((sym["line"].as_i64().unwrap() as usize).saturating_sub(1))
                    .take(5)
                    .collect::<Vec<_>>()
            );
        }
    }

    // Verify end_line is always >= line
    for sym in &symbols {
        let line = sym["line"].as_i64().unwrap();
        let end_line = sym["end_line"].as_i64().unwrap();
        assert!(
            end_line >= line,
            "end_line ({}) should be >= line ({}) for symbol {}",
            end_line,
            line,
            sym["name"]
        );
    }
}

/// Helper: create a project with impl blocks for scope testing
fn create_scoped_test_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("creating temp dir");

    std::fs::write(
        dir.path().join("scoped.rs"),
        r#"
pub struct Parser {
    input: String,
}

impl Parser {
    pub fn new(input: String) -> Self {
        Parser { input }
    }

    pub fn parse(&self) -> bool {
        !self.input.is_empty()
    }
}

pub struct Lexer {
    source: String,
}

impl Lexer {
    pub fn tokenize(&self) -> Vec<String> {
        vec![]
    }
}

pub fn standalone() -> i32 {
    42
}
"#,
    )
    .unwrap();

    dir
}

/// Helper: set up a scoped project with helios init
fn setup_scoped_project() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = create_scoped_test_project();
    let bin = helios_bin();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(
        output.status.success(),
        "helios init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (dir, bin)
}

#[test]
fn test_scope_filter() {
    let (dir, bin) = setup_scoped_project();

    // --scope Parser should return only Parser's methods
    let output = Command::new(&bin)
        .args(["symbols", "--scope", "Parser"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --scope Parser");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should contain Parser's methods
    assert!(
        stdout.contains("new"),
        "should find 'new' method in Parser scope, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("parse"),
        "should find 'parse' method in Parser scope, got:\n{}",
        stdout
    );

    // Should NOT contain Lexer methods or standalone functions
    assert!(
        !stdout.contains("tokenize"),
        "should not contain Lexer's tokenize, got:\n{}",
        stdout
    );
    assert!(
        !stdout.contains("standalone"),
        "should not contain standalone function, got:\n{}",
        stdout
    );

    // Should NOT contain the struct definitions themselves (they have no scope)
    assert!(
        !stdout.contains("struct"),
        "should not contain struct symbols (they have scope=None), got:\n{}",
        stdout
    );
}

#[test]
fn test_scope_filter_json() {
    let (dir, bin) = setup_scoped_project();

    let output = Command::new(&bin)
        .args(["--json", "symbols", "--scope", "Parser"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json --scope Parser");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // All returned symbols should have scope == "Parser"
    for sym in &symbols {
        assert_eq!(
            sym["scope"].as_str(),
            Some("Parser"),
            "every symbol should be scoped to Parser, got: {:?}",
            sym
        );
    }

    // Should have the expected methods
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"new"),
        "should contain 'new', got: {:?}",
        names
    );
    assert!(
        names.contains(&"parse"),
        "should contain 'parse', got: {:?}",
        names
    );
    assert_eq!(
        symbols.len(),
        2,
        "Parser scope should have exactly 2 methods, got: {:?}",
        names
    );
}

#[test]
fn test_scope_with_kind_filter() {
    let (dir, bin) = setup_scoped_project();

    // Combine --scope and --kind to verify composability
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--scope", "Lexer", "--kind", "fn"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --scope Lexer --kind fn");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // Should return only Lexer's fn-kind symbols
    assert_eq!(
        symbols.len(),
        1,
        "Lexer should have exactly 1 fn, got: {:?}",
        symbols
    );
    assert_eq!(symbols[0]["name"].as_str(), Some("tokenize"));
    assert_eq!(symbols[0]["scope"].as_str(), Some("Lexer"));
    assert_eq!(symbols[0]["kind"].as_str(), Some("fn"));

    // Non-matching scope+kind combo should return empty
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--scope", "Parser", "--kind", "struct"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --scope Parser --kind struct");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");
    assert!(
        symbols.is_empty(),
        "Parser scope should have no structs, got: {:?}",
        symbols
    );
}

#[test]
fn test_visibility_filter_pub() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args([
            "--json",
            "symbols",
            "--visibility",
            "pub",
            "--file",
            "main.rs",
        ])
        .current_dir(dir.path())
        .output()
        .expect("symbols --visibility pub --file main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // All returned symbols should have visibility == "pub"
    assert!(!symbols.is_empty(), "should find pub symbols in main.rs");
    for sym in &symbols {
        assert_eq!(
            sym["visibility"].as_str(),
            Some("pub"),
            "every symbol should be pub, got: {:?}",
            sym
        );
    }

    // Should contain known pub symbols
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"main"),
        "should contain pub fn main, got: {:?}",
        names
    );
    assert!(
        names.contains(&"Config"),
        "should contain pub struct Config, got: {:?}",
        names
    );

    // Should NOT contain the private helper function
    assert!(
        !names.contains(&"helper"),
        "should not contain private fn helper, got: {:?}",
        names
    );
}

#[test]
fn test_visibility_filter_private() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args([
            "--json",
            "symbols",
            "--visibility",
            "private",
            "--file",
            "main.rs",
        ])
        .current_dir(dir.path())
        .output()
        .expect("symbols --visibility private --file main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // All returned symbols should have visibility == "private"
    for sym in &symbols {
        assert_eq!(
            sym["visibility"].as_str(),
            Some("private"),
            "every symbol should be private, got: {:?}",
            sym
        );
    }

    // Should contain the private helper function
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"helper"),
        "should contain private fn helper, got: {:?}",
        names
    );

    // Should NOT contain pub symbols
    assert!(
        !names.contains(&"main"),
        "should not contain pub fn main, got: {:?}",
        names
    );
    assert!(
        !names.contains(&"Config"),
        "should not contain pub struct Config, got: {:?}",
        names
    );
}

#[test]
fn test_visibility_with_kind() {
    let (dir, bin) = setup_indexed_project();

    // Combine --visibility pub with --kind fn
    let output = Command::new(&bin)
        .args([
            "--json",
            "symbols",
            "--visibility",
            "pub",
            "--kind",
            "fn",
            "--file",
            "main.rs",
        ])
        .current_dir(dir.path())
        .output()
        .expect("symbols --visibility pub --kind fn --file main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // All should be pub AND fn
    for sym in &symbols {
        assert_eq!(
            sym["visibility"].as_str(),
            Some("pub"),
            "every symbol should be pub, got: {:?}",
            sym
        );
        assert_eq!(
            sym["kind"].as_str(),
            Some("fn"),
            "every symbol should be fn, got: {:?}",
            sym
        );
    }

    // Should contain pub fn main but not pub struct Config or private fn helper
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"main"),
        "should contain pub fn main, got: {:?}",
        names
    );
    assert!(
        !names.contains(&"Config"),
        "should not contain Config (it's a struct), got: {:?}",
        names
    );
    assert!(
        !names.contains(&"helper"),
        "should not contain helper (it's private), got: {:?}",
        names
    );
}

#[test]
fn test_symbols_param_and_returns_filters() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn greet(name: &str) {
    println!("{}", name);
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(
        output.status.success(),
        "helios init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // --param narrows to symbols with a matching parameter substring.
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--param", "i32"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --param i32");
    assert!(output.status.success());
    let symbols: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("parsing JSON");
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["add"],
        "expected only add to have an i32 param: {:?}",
        symbols
    );
    assert_eq!(
        symbols[0]["params"],
        serde_json::json!(["a: i32", "b: i32"]),
        "params should be the source spelling as a JSON array: {:?}",
        symbols[0]
    );

    // --returns narrows to symbols with a matching return-type substring.
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--returns", "i32"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --returns i32");
    assert!(output.status.success());
    let symbols: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("parsing JSON");
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["add"],
        "expected only add to return i32: {:?}",
        symbols
    );
    assert_eq!(symbols[0]["returns"], serde_json::json!("i32"));

    // A different --param substring selects a different symbol.
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--param", "str"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --param str");
    assert!(output.status.success());
    let symbols: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).expect("parsing JSON");
    let names: Vec<&str> = symbols
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["greet"],
        "expected only greet to have a str param: {:?}",
        symbols
    );
}

// --- Status command tests ---

#[test]
fn test_status_with_index() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["status"])
        .current_dir(dir.path())
        .output()
        .expect("helios status");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Should show the index path
    assert!(
        stdout.contains("Index: .helios/index.db"),
        "should show index path, got:\n{}",
        stdout
    );
    // Should show file and symbol counts
    assert!(
        stdout.contains("Files:"),
        "should show file count, got:\n{}",
        stdout
    );
    assert!(
        stdout.contains("Symbols:"),
        "should show symbol count, got:\n{}",
        stdout
    );
    // Should show languages
    assert!(
        stdout.contains("Languages:"),
        "should show languages, got:\n{}",
        stdout
    );
}

#[test]
fn test_status_without_index() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    let output = Command::new(&bin)
        .args(["status"])
        .current_dir(dir.path())
        .output()
        .expect("helios status");

    // Status without index should succeed (exit 0), not error
    assert!(
        output.status.success(),
        "status without index should exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No index found"),
        "should say no index found, got:\n{}",
        stdout
    );
}

#[test]
fn test_status_without_index_json() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    let output = Command::new(&bin)
        .args(["--json", "status"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json status");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");

    assert_eq!(
        json["indexed"],
        serde_json::json!(false),
        "should report indexed: false, got: {:?}",
        json
    );
}

#[test]
fn test_status_json() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["--json", "status"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json status");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");

    // Core fields
    assert_eq!(json["indexed"], serde_json::json!(true));
    assert!(
        json["files"].as_i64().unwrap() >= 4,
        "should have at least 4 files, got: {}",
        json["files"]
    );
    assert!(
        json["symbols"].as_i64().unwrap() > 0,
        "should have symbols, got: {}",
        json["symbols"]
    );
    assert!(
        json["imports"].is_number(),
        "should have imports count, got: {:?}",
        json["imports"]
    );
    assert!(
        json["languages"].is_array(),
        "should have languages array, got: {:?}",
        json["languages"]
    );
    assert_eq!(json["db_path"], serde_json::json!(".helios/index.db"));
}

#[test]
fn test_status_compact_json() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["--json", "--compact", "status"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json --compact status");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let trimmed = stdout.trim();

    // Compact output must be a single line
    assert_eq!(
        trimmed.lines().count(),
        1,
        "compact JSON should be a single line, got:\n{}",
        trimmed
    );

    // Must be valid JSON
    let json: serde_json::Value =
        serde_json::from_str(trimmed).expect("compact output must be valid JSON");
    assert_eq!(json["indexed"], serde_json::json!(true));
}

// --- Diff command tests ---

/// Helper: create a git-backed test project, index it, and return (temp_dir, binary_path)
fn setup_git_indexed_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // Init git repo
    let output = Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .expect("git init");
    assert!(output.status.success(), "git init failed");

    // Configure git user for commits
    Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir.path())
        .output()
        .expect("git config email");
    Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir.path())
        .output()
        .expect("git config name");

    // Create a source file
    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn hello() {
    println!("hello");
}

pub struct Config {
    pub name: String,
}

fn helper() -> i32 {
    42
}
"#,
    )
    .unwrap();

    // Commit
    Command::new("git")
        .args(["add", "."])
        .current_dir(dir.path())
        .output()
        .expect("git add");
    Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(dir.path())
        .output()
        .expect("git commit");

    // Index with helios
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(
        output.status.success(),
        "helios init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    (dir, bin)
}

#[test]
fn test_diff_no_changes() {
    let (dir, bin) = setup_git_indexed_project();

    let output = Command::new(&bin)
        .arg("diff")
        .current_dir(dir.path())
        .output()
        .expect("helios diff");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No symbol changes"),
        "expected no changes, got: {}",
        stdout
    );
}

#[test]
fn test_diff_after_modification() {
    let (dir, bin) = setup_git_indexed_project();

    // Modify the file: add a function, remove helper, shift Config
    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn hello() {
    println!("hello world");
}

pub fn new_function() {
    println!("new");
}

pub struct Config {
    pub name: String,
    pub value: i32,
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("diff")
        .current_dir(dir.path())
        .output()
        .expect("helios diff");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // new_function should be added
    assert!(
        stdout.contains("+ fn new_function"),
        "expected new_function added, got: {}",
        stdout
    );

    // helper was removed
    assert!(
        stdout.contains("- fn helper"),
        "expected helper removed, got: {}",
        stdout
    );

    // Config moved lines
    assert!(
        stdout.contains("~ struct Config"),
        "expected Config modified, got: {}",
        stdout
    );
}

#[test]
fn test_diff_json_output() {
    let (dir, bin) = setup_git_indexed_project();

    // Modify: add a new function
    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn hello() {
    println!("hello");
}

pub fn brand_new() -> bool {
    true
}

pub struct Config {
    pub name: String,
}

fn helper() -> i32 {
    42
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .args(["--json", "diff"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json diff");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON output");

    // Verify structure has added/removed/modified arrays
    assert!(json["added"].is_array(), "expected added array");
    assert!(json["removed"].is_array(), "expected removed array");
    assert!(json["modified"].is_array(), "expected modified array");

    // brand_new should be in added
    let added = json["added"].as_array().unwrap();
    assert!(
        added.iter().any(|s| s["name"] == "brand_new"),
        "expected brand_new in added: {:?}",
        added
    );

    // Each added entry should have file, name, kind, line
    for entry in added {
        assert!(entry["file"].is_string(), "added entry missing file");
        assert!(entry["name"].is_string(), "added entry missing name");
        assert!(entry["kind"].is_string(), "added entry missing kind");
        assert!(entry["line"].is_number(), "added entry missing line");
    }
}

#[test]
fn test_diff_deleted_file() {
    let (dir, bin) = setup_git_indexed_project();

    // Stage the deletion so git diff sees it
    std::fs::remove_file(dir.path().join("main.rs")).unwrap();
    Command::new("git")
        .args(["add", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("git add deleted file");

    let output = Command::new(&bin)
        .arg("diff")
        .current_dir(dir.path())
        .output()
        .expect("helios diff");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // All symbols from the deleted file should show as removed
    assert!(
        stdout.contains("- fn hello"),
        "expected hello removed, got: {}",
        stdout
    );
    assert!(
        stdout.contains("- struct Config"),
        "expected Config removed, got: {}",
        stdout
    );
}

#[test]
fn test_diff_no_index() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    let output = Command::new(&bin)
        .arg("diff")
        .current_dir(dir.path())
        .output()
        .expect("helios diff");

    let code = output.status.code().expect("should have exit code");
    assert_eq!(
        code, 2,
        "helios diff without index should exit 2, got {code}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No index found"),
        "expected no index message on stderr, got: {}",
        stderr
    );
}

/// Find a named entry in `diff`'s `modified` JSON array.
fn find_modified<'a>(json: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    json["modified"]
        .as_array()
        .expect("modified array")
        .iter()
        .find(|s| s["name"] == name)
        .unwrap_or_else(|| panic!("expected {name} in modified: {}", json["modified"]))
}

#[test]
fn test_diff_body_only_change_reports_body() {
    let (dir, bin) = setup_git_indexed_project();

    // Add a line inside hello()'s body: line range moves but the signature
    // (kind, visibility, params, returns) is untouched.
    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn hello() {
    println!("hello");
    println!("again");
}

pub struct Config {
    pub name: String,
}

fn helper() -> i32 {
    42
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .args(["--json", "diff"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json diff");
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parsing JSON output");

    let hello = find_modified(&json, "hello");
    assert_eq!(hello["change"], "body", "body-only edit: {}", hello);
}

#[test]
fn test_diff_param_change_reports_signature() {
    let (dir, bin) = setup_git_indexed_project();

    // helper() gains a parameter; keep it on the same line so only the
    // signature, not the line range, changes.
    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn hello() {
    println!("hello");
}

pub struct Config {
    pub name: String,
}

fn helper(x: i32) -> i32 {
    x
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .args(["--json", "diff"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json diff");
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parsing JSON output");

    let helper = find_modified(&json, "helper");
    assert_eq!(helper["change"], "signature", "param change: {}", helper);
}

#[test]
fn test_diff_return_type_change_reports_signature() {
    let (dir, bin) = setup_git_indexed_project();

    // helper()'s return type changes from i32 to i64.
    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn hello() {
    println!("hello");
}

pub struct Config {
    pub name: String,
}

fn helper() -> i64 {
    42
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .args(["--json", "diff"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json diff");
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parsing JSON output");

    let helper = find_modified(&json, "helper");
    assert_eq!(
        helper["change"], "signature",
        "return type change: {}",
        helper
    );
}

#[test]
fn test_diff_legacy_null_signature_is_not_reported_as_change() {
    let (dir, bin) = setup_git_indexed_project();

    // Simulate a legacy index: params/returns were never recorded for `helper`.
    {
        let conn = rusqlite::Connection::open(dir.path().join(".helios").join("index.db"))
            .expect("opening index.db");
        conn.execute(
            "UPDATE symbols SET params = NULL, returns = NULL WHERE name = 'helper'",
            [],
        )
        .expect("nulling out legacy signature columns");
    }

    // Move helper's line without touching its signature.
    std::fs::write(
        dir.path().join("main.rs"),
        r#"pub fn hello() {
    println!("hello");
}

pub struct Config {
    pub name: String,
}


fn helper() -> i32 {
    42
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .args(["--json", "diff"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json diff");
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parsing JSON output");

    let helper = find_modified(&json, "helper");
    assert_eq!(
        helper["change"], "body",
        "a stored NULL signature vs. a freshly parsed one must not read as a signature change: {}",
        helper
    );
}

// --- Pagination tests ---

#[test]
fn test_pagination_limit() {
    let (dir, bin) = setup_indexed_project();

    // Get total count first (no limit)
    let output = Command::new(&bin)
        .args(["symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols");
    let all_stdout = String::from_utf8_lossy(&output.stdout);
    let total_lines: Vec<&str> = all_stdout.lines().collect();
    assert!(
        total_lines.len() > 3,
        "need at least 4 symbols for pagination test, got {}",
        total_lines.len()
    );

    // Now query with --limit 3
    let output = Command::new(&bin)
        .args(["symbols", "--limit", "3"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --limit 3");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Text output: 3 symbol lines + 1 pagination footer
    let lines: Vec<&str> = stdout.lines().collect();
    let symbol_lines: Vec<&&str> = lines.iter().filter(|l| l.contains(":")).collect();
    assert_eq!(
        symbol_lines.len(),
        3,
        "expected exactly 3 symbol lines, got: {:?}",
        symbol_lines
    );
    assert!(
        stdout.contains("Showing 1-3 of"),
        "expected pagination footer, got: {}",
        stdout
    );
}

#[test]
fn test_pagination_offset() {
    let (dir, bin) = setup_indexed_project();

    // Get all symbols first
    let output = Command::new(&bin)
        .args(["--json", "symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let all_symbols: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");
    let all_arr = all_symbols.as_array().unwrap();
    assert!(all_arr.len() > 3, "need at least 4 symbols");

    // Get first 2
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--limit", "2"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --limit 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let page1: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON page1");
    let page1_syms = page1["symbols"].as_array().unwrap();
    assert_eq!(page1_syms.len(), 2);

    // Get next 2 with offset
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--limit", "2", "--offset", "2"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --limit 2 --offset 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let page2: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON page2");
    let page2_syms = page2["symbols"].as_array().unwrap();
    assert_eq!(page2_syms.len(), 2);

    // Verify offset actually skipped: page2 first symbol should equal all_arr[2]
    assert_eq!(
        page2_syms[0]["name"], all_arr[2]["name"],
        "offset should skip first 2 symbols"
    );
}

#[test]
fn test_pagination_json_total_count() {
    let (dir, bin) = setup_indexed_project();

    // Without pagination: plain array (backward compat)
    let output = Command::new(&bin)
        .args(["--json", "symbols"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let no_page: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");
    assert!(
        no_page.is_array(),
        "without pagination, output should be a plain array"
    );
    let total = no_page.as_array().unwrap().len();

    // With pagination: wrapped object with total_count
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--limit", "2"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --json --limit 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paginated: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");
    assert!(
        paginated.is_object(),
        "with pagination, output should be an object"
    );
    assert_eq!(
        paginated["total_count"].as_i64().unwrap() as usize,
        total,
        "total_count should match full symbol count"
    );
    assert_eq!(paginated["limit"].as_i64().unwrap(), 2);
    assert_eq!(paginated["offset"].as_i64().unwrap(), 0);
    assert_eq!(paginated["symbols"].as_array().unwrap().len(), 2);
}

#[test]
fn test_pagination_export() {
    let (dir, bin) = setup_indexed_project();

    // Get total symbol count from unpaginated export
    let output = Command::new(&bin)
        .args(["--json", "export"])
        .current_dir(dir.path())
        .output()
        .expect("export --json");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let full_export: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");
    let total_symbols = full_export["total_symbols"].as_i64().unwrap();
    assert!(total_symbols > 3, "need symbols for pagination test");
    // Without pagination, no total_count key
    assert!(
        full_export.get("total_count").is_none(),
        "unpaginated export should not have total_count"
    );

    // With limit
    let output = Command::new(&bin)
        .args(["--json", "export", "--limit", "3"])
        .current_dir(dir.path())
        .output()
        .expect("export --json --limit 3");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let paginated: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");
    assert_eq!(paginated["total_symbols"].as_i64().unwrap(), 3);
    assert!(
        paginated["total_count"].is_number(),
        "paginated export should have total_count"
    );
    assert_eq!(
        paginated["total_count"].as_i64().unwrap(),
        total_symbols,
        "total_count should match full count"
    );
    assert_eq!(paginated["limit"].as_i64().unwrap(), 3);
    assert_eq!(paginated["offset"].as_i64().unwrap(), 0);

    // With limit + offset
    let output = Command::new(&bin)
        .args(["--json", "export", "--limit", "2", "--offset", "2"])
        .current_dir(dir.path())
        .output()
        .expect("export --json --limit 2 --offset 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let page2: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");
    assert_eq!(page2["total_symbols"].as_i64().unwrap(), 2);
    assert_eq!(page2["offset"].as_i64().unwrap(), 2);
}

// --- Regex grep tests ---

#[test]
fn test_grep_regex_anchor() {
    let (dir, bin) = setup_indexed_project();

    // ^main$ should match exactly "main", not "maintain" or "domain"
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--grep", "^main$"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep ^main$");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    assert_eq!(symbols.len(), 1, "^main$ should match exactly one symbol");
    assert_eq!(symbols[0]["name"].as_str().unwrap(), "main");
}

#[test]
fn test_grep_regex_pattern() {
    let (dir, bin) = setup_indexed_project();

    // process.* should match names starting with "process"
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--grep", "^process"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep ^process");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // The test project has process_files in lib.py
    assert!(
        !symbols.is_empty(),
        "^process should match process_files from lib.py"
    );
    for sym in &symbols {
        let name = sym["name"].as_str().unwrap();
        assert!(
            name.starts_with("process"),
            "all matches should start with 'process', got: {name}"
        );
    }
}

#[test]
fn test_grep_regex_invalid() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["symbols", "--grep", "[invalid"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep [invalid");

    assert!(
        !output.status.success(),
        "invalid regex should fail with non-zero exit"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("invalid regex") || stderr.contains("regex parse error"),
        "error should mention regex: {stderr}"
    );
}

#[test]
fn test_grep_backward_compat() {
    let (dir, bin) = setup_indexed_project();

    // Simple substring "Config" should still work (backward compatible)
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--grep", "Config"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep Config");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // The test project has Config in main.rs and AppConfig in app.ts
    assert!(
        symbols.len() >= 2,
        "substring 'Config' should match Config and AppConfig, got: {}",
        symbols.len()
    );
    for sym in &symbols {
        let name = sym["name"].as_str().unwrap();
        assert!(
            name.contains("Config"),
            "all matches should contain 'Config', got: {name}"
        );
    }
}

#[test]
fn test_grep_regex_end_anchor() {
    let (dir, bin) = setup_indexed_project();

    // .*Server$ should match names ending with "Server"
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--grep", "Server$"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep Server$");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let symbols: Vec<serde_json::Value> = serde_json::from_str(&stdout).expect("parsing JSON");

    // The test project has Server and NewServer in server.go
    assert!(!symbols.is_empty(), "Server$ should match at least Server");
    for sym in &symbols {
        let name = sym["name"].as_str().unwrap();
        assert!(
            name.ends_with("Server"),
            "all matches should end with 'Server', got: {name}"
        );
    }
}

#[test]
fn test_grep_regex_with_pagination() {
    let (dir, bin) = setup_indexed_project();

    // Regex with pagination: total_count should reflect regex-filtered count, not LIKE count
    let output = Command::new(&bin)
        .args(["--json", "symbols", "--grep", "^main$", "--limit", "10"])
        .current_dir(dir.path())
        .output()
        .expect("symbols --grep ^main$ --limit 10");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");

    let total = result["total_count"].as_i64().unwrap();
    let symbols = result["symbols"].as_array().unwrap();

    assert_eq!(total, 1, "total_count should be 1 for ^main$ regex");
    assert_eq!(symbols.len(), 1, "should return exactly 1 symbol");
    assert_eq!(symbols[0]["name"].as_str().unwrap(), "main");
}

/// Create a test project with a chain of TypeScript imports for transitive dep testing:
/// chain_base.ts -> chain_mid.ts -> chain_leaf.ts
fn create_chain_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("creating temp dir");

    // chain_leaf.ts: no imports from project files
    std::fs::write(
        dir.path().join("chain_leaf.ts"),
        r#"
export function leaf(): string {
    return "leaf";
}
"#,
    )
    .unwrap();

    // chain_mid.ts: imports from chain_leaf (use .ts extension so LIKE match works)
    std::fs::write(
        dir.path().join("chain_mid.ts"),
        r#"
import { leaf } from './chain_leaf.ts';

export function mid(): string {
    return leaf() + "_mid";
}
"#,
    )
    .unwrap();

    // chain_base.ts: imports from chain_mid (use .ts extension so LIKE match works)
    std::fs::write(
        dir.path().join("chain_base.ts"),
        r#"
import { mid } from './chain_mid.ts';

export function base(): string {
    return mid() + "_base";
}
"#,
    )
    .unwrap();

    dir
}

fn setup_chain_project() -> (tempfile::TempDir, PathBuf) {
    let dir = create_chain_project();
    let bin = helios_bin();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(
        output.status.success(),
        "helios init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (dir, bin)
}

#[test]
fn test_deps_depth_default() {
    // Default depth=1 behavior should be unchanged from before --depth was added
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["deps", "main.rs"])
        .current_dir(dir.path())
        .output()
        .expect("deps main.rs");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should show direct dependencies only
    assert!(
        stdout.contains("Dependencies"),
        "should show dependencies section"
    );
}

#[test]
fn test_deps_depth_flag_accepted() {
    // Verify --depth flag is accepted by the CLI
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["deps", "main.rs", "--depth", "2"])
        .current_dir(dir.path())
        .output()
        .expect("deps --depth 2");

    assert!(
        output.status.success(),
        "deps --depth 2 should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_deps_depth_2_dependents() {
    // chain_leaf.ts is imported by chain_mid.ts, which is imported by chain_base.ts.
    // With --depth 2, dependents of chain_leaf should include chain_base transitively.
    let (dir, bin) = setup_chain_project();

    let output = Command::new(&bin)
        .args(["deps", "chain_leaf.ts", "--depth", "2"])
        .current_dir(dir.path())
        .output()
        .expect("deps chain_leaf.ts --depth 2");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    // chain_mid.ts imports chain_leaf -> should appear at depth 1
    assert!(
        stdout.contains("chain_mid.ts"),
        "should find chain_mid.ts as dependent, got:\n{stdout}"
    );
    // chain_base.ts imports chain_mid -> should appear at depth 2
    assert!(
        stdout.contains("chain_base.ts"),
        "should find chain_base.ts as transitive dependent at depth 2, got:\n{stdout}"
    );
}

#[test]
fn test_deps_depth_1_no_transitive() {
    // With --depth 1, dependents of chain_leaf should NOT include chain_base.
    let (dir, bin) = setup_chain_project();

    let output = Command::new(&bin)
        .args(["deps", "chain_leaf.ts", "--depth", "1"])
        .current_dir(dir.path())
        .output()
        .expect("deps chain_leaf.ts --depth 1");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        stdout.contains("chain_mid.ts"),
        "should find chain_mid.ts at depth 1, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("chain_base.ts"),
        "should NOT find chain_base.ts at depth 1, got:\n{stdout}"
    );
}

#[test]
fn test_deps_depth_json() {
    // JSON output should include depth info per dependency
    let (dir, bin) = setup_chain_project();

    let output = Command::new(&bin)
        .args(["--json", "deps", "chain_leaf.ts", "--depth", "2"])
        .current_dir(dir.path())
        .output()
        .expect("deps --json --depth 2");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value = serde_json::from_str(&stdout).expect("parsing JSON");

    // Check structure
    assert_eq!(result["target"].as_str().unwrap(), "chain_leaf.ts");
    assert_eq!(result["depth"].as_u64().unwrap(), 2);

    // Dependents should have depth field
    let dependents = result["dependents"].as_array().expect("dependents array");
    assert!(
        !dependents.is_empty(),
        "should have dependents, got: {result}"
    );

    // Find chain_mid at depth 1
    let mid_entry = dependents
        .iter()
        .find(|e| e["path"].as_str().is_some_and(|p| p.contains("chain_mid")))
        .expect("should find chain_mid in dependents");
    assert_eq!(
        mid_entry["depth"].as_u64().unwrap(),
        1,
        "chain_mid should be at depth 1"
    );

    // Find chain_base at depth 2
    let base_entry = dependents
        .iter()
        .find(|e| e["path"].as_str().is_some_and(|p| p.contains("chain_base")))
        .expect("should find chain_base in dependents");
    assert_eq!(
        base_entry["depth"].as_u64().unwrap(),
        2,
        "chain_base should be at depth 2"
    );
}

#[test]
fn test_deps_depth_symbol_ignores_depth() {
    // Symbol targets should work with --depth flag without error,
    // but depth > 1 has no special effect for symbols
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["deps", "main", "--depth", "3"])
        .current_dir(dir.path())
        .output()
        .expect("deps symbol --depth 3");

    assert!(
        output.status.success(),
        "symbol deps with --depth should succeed, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_deps_depth_cycle_detection() {
    // Create files with circular imports to verify no infinite loop
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // cycle_a.ts imports cycle_b, cycle_b imports cycle_a
    std::fs::write(
        dir.path().join("cycle_a.ts"),
        r#"
import { b } from './cycle_b';
export function a(): string { return b(); }
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("cycle_b.ts"),
        r#"
import { a } from './cycle_a';
export function b(): string { return a(); }
"#,
    )
    .unwrap();

    let init = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");
    assert!(init.status.success());

    // This should complete without hanging (cycle detection via HashSet)
    let output = Command::new(&bin)
        .args(["deps", "cycle_a.ts", "--depth", "10"])
        .current_dir(dir.path())
        .output()
        .expect("deps with cycle --depth 10");

    assert!(
        output.status.success(),
        "should handle cycles gracefully, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// --- Files command tests ---

#[test]
fn test_files_command() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .arg("files")
        .current_dir(dir.path())
        .output()
        .expect("helios files");

    assert!(
        output.status.success(),
        "files command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Header row
    assert!(stdout.contains("PATH"), "should have PATH header");
    assert!(stdout.contains("LANGUAGE"), "should have LANGUAGE header");
    assert!(stdout.contains("SYMBOLS"), "should have SYMBOLS header");
    assert!(stdout.contains("IMPORTS"), "should have IMPORTS header");

    // Should list the test files
    assert!(stdout.contains("main.rs"), "should list main.rs");
    assert!(stdout.contains("lib.py"), "should list lib.py");
}

#[test]
fn test_files_language_filter() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["files", "--language", "rust"])
        .current_dir(dir.path())
        .output()
        .expect("helios files --language rust");

    assert!(
        output.status.success(),
        "files --language rust failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("main.rs"), "should list rust file");
    assert!(
        !stdout.contains("lib.py"),
        "should not list python file when filtering for rust"
    );
    assert!(
        !stdout.contains("server.go"),
        "should not list go file when filtering for rust"
    );
}

#[test]
fn test_files_json() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["files", "--json"])
        .current_dir(dir.path())
        .output()
        .expect("helios files --json");

    assert!(
        output.status.success(),
        "files --json failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON output");

    let arr = parsed.as_array().expect("should be an array");
    assert!(!arr.is_empty(), "should have at least one file");

    // Check that each entry has expected fields
    for entry in arr {
        assert!(entry.get("path").is_some(), "entry should have path");
        assert!(
            entry.get("language").is_some(),
            "entry should have language"
        );
        assert!(entry.get("symbols").is_some(), "entry should have symbols");
        assert!(entry.get("imports").is_some(), "entry should have imports");
        assert!(
            entry.get("last_indexed_at").is_some(),
            "entry should have last_indexed_at"
        );
    }
}

// ---- Quiet mode tests ----

#[test]
fn test_quiet_init() {
    let dir = create_test_project();
    let bin = helios_bin();

    let output = Command::new(&bin)
        .args(["--quiet", "init"])
        .current_dir(dir.path())
        .output()
        .expect("helios --quiet init");

    assert!(output.status.success(), "exit code should be 0");
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty with --quiet, got: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_quiet_update() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .args(["--quiet", "update"])
        .current_dir(dir.path())
        .output()
        .expect("helios --quiet update");

    assert!(output.status.success(), "exit code should be 0");
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty with --quiet, got: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_quiet_error_stderr() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // Run update without init — should fail with error on stderr
    let output = Command::new(&bin)
        .args(["--quiet", "update"])
        .current_dir(dir.path())
        .output()
        .expect("helios --quiet update (no index)");

    assert!(!output.status.success(), "should fail without index");
    assert!(
        output.stdout.is_empty(),
        "stdout should be empty even on error with --quiet, got: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.is_empty(), "error should still appear on stderr");
    assert!(
        stderr.contains("No index found"),
        "stderr should contain error message, got: {stderr}"
    );
}

// --- Exit code tests ---

#[test]
fn test_exit_code_no_index() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // All commands that require an index should exit 2 when none exists.
    for subcommand in &[
        "symbols",
        "deps dummy",
        "export",
        "summary",
        "diff",
        "files",
        "update",
    ] {
        let args: Vec<&str> = subcommand.split_whitespace().collect();
        let output = Command::new(&bin)
            .args(&args)
            .current_dir(dir.path())
            .output()
            .unwrap_or_else(|_| panic!("helios {subcommand}"));

        let code = output.status.code().expect("should have exit code");
        assert_eq!(
            code, 2,
            "helios {subcommand} without index should exit 2, got {code}"
        );

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("No index found"),
            "helios {subcommand} stderr should mention missing index, got: {stderr}"
        );
    }
}

#[test]
fn test_exit_code_no_index_json() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // JSON mode should also exit 2 for no-index errors.
    let output = Command::new(&bin)
        .args(["--json", "symbols"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json symbols");

    let code = output.status.code().expect("should have exit code");
    assert_eq!(
        code, 2,
        "helios --json symbols without index should exit 2, got {code}"
    );
}

#[test]
fn test_exit_code_success() {
    let (dir, bin) = setup_indexed_project();

    let output = Command::new(&bin)
        .arg("symbols")
        .current_dir(dir.path())
        .output()
        .expect("helios symbols");

    let code = output.status.code().expect("should have exit code");
    assert_eq!(
        code, 0,
        "helios symbols with index should exit 0, got {code}"
    );
}

#[test]
fn test_exit_code_status_no_index() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // status gracefully reports no index and exits 0.
    let output = Command::new(&bin)
        .arg("status")
        .current_dir(dir.path())
        .output()
        .expect("helios status");

    let code = output.status.code().expect("should have exit code");
    assert_eq!(
        code, 0,
        "helios status without index should exit 0 (graceful), got {code}"
    );
}

#[test]
fn test_exit_code_general_error() {
    let (dir, bin) = setup_indexed_project();

    // Provide an invalid regex to symbols --grep to trigger a general error (exit 1).
    let output = Command::new(&bin)
        .args(["symbols", "--grep", "[invalid"])
        .current_dir(dir.path())
        .output()
        .expect("helios symbols --grep invalid");

    let code = output.status.code().expect("should have exit code");
    assert_eq!(
        code, 1,
        "helios symbols with bad regex should exit 1, got {code}"
    );
}

#[test]
fn test_help_shows_exit_codes() {
    let bin = helios_bin();

    let output = Command::new(&bin)
        .arg("--help")
        .output()
        .expect("helios --help");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("EXIT CODES:"),
        "help should contain EXIT CODES section, got: {stdout}"
    );
    assert!(
        stdout.contains("0  Success"),
        "help should document exit code 0"
    );
    assert!(
        stdout.contains("1  General error"),
        "help should document exit code 1"
    );
    assert!(
        stdout.contains("2  No index found"),
        "help should document exit code 2"
    );
}

// --- Roslyn sidecar degradation ladder (story 181: P3-M1, P3-M2, P3-S1) ---
//
// These tests must not require dotnet: with `HELIOS_ROSLYN` set but broken,
// the ladder degrades identically whether dotnet is absent (spawn fails) or
// present (ping exits non-zero) — exit 0, exactly one warning, tree-sitter
// references, usable index.

fn write_csharp_files(dir: &std::path::Path) {
    std::fs::write(
        dir.join("Person.cs"),
        r#"
namespace App {
    public class Person {
        public void Greet() { }
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("Program.cs"),
        r#"
namespace App {
    public class Runner {
        public void Go() {
            Greet();
        }
    }
}
"#,
    )
    .unwrap();
}

fn create_csharp_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("creating temp dir");
    write_csharp_files(dir.path());
    dir
}

/// Open the index database `helios init` produced in `dir`.
fn index_db(dir: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(dir.join(".helios").join("index.db")).expect("open index.db")
}

/// Read a `metadata` row from the index in `dir`.
fn metadata_value(dir: &std::path::Path, key: &str) -> Option<String> {
    index_db(dir)
        .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |r| {
            r.get(0)
        })
        .ok()
}

/// Init with a broken/missing sidecar must exit 0, emit exactly one
/// `warning:` line, and still produce a usable index with `.cs` references
/// resolved via the tree-sitter path.
fn assert_sidecar_degrades(dir: &tempfile::TempDir, helios_roslyn: &str) {
    let bin = helios_bin();
    let output = Command::new(&bin)
        .arg("init")
        .env("HELIOS_ROSLYN", helios_roslyn)
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    // Never hard-fail: exit 0.
    assert!(
        output.status.success(),
        "init must exit 0 despite sidecar failure, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Exactly one warning line on stderr.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one warning line, got stderr: {stderr:?}"
    );
    assert!(
        lines[0].starts_with("warning:"),
        "line must use the warning channel, got: {}",
        lines[0]
    );

    // The fallback is visible in the init summary itself, not only in the
    // warning line above (which scrolls past) or in a later `status` call.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("C# resolver: treesitter"),
        "init summary must name the resolver, got stdout: {stdout:?}"
    );

    // Index usable: symbols present.
    let symbols = Command::new(&bin)
        .args(["--json", "symbols", "--file", "Person.cs"])
        .current_dir(dir.path())
        .output()
        .expect("symbols");
    assert!(symbols.status.success(), "symbols query failed");
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&symbols.stdout)).expect("symbols JSON");
    let names: Vec<&str> = value
        .as_array()
        .expect("symbols array")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(names.contains(&"Greet"), "index missing Greet: {names:?}");

    // `.cs` references resolved via the tree-sitter path: the single Greet()
    // call site links to the single Greet definition.
    let deps = Command::new(&bin)
        .args(["--json", "deps", "Greet"])
        .current_dir(dir.path())
        .output()
        .expect("deps Greet");
    assert!(
        deps.status.success(),
        "deps failed: {}",
        String::from_utf8_lossy(&deps.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&deps.stdout)).expect("deps JSON");
    let dependents = value["dependents"].as_array().expect("dependents array");
    assert_eq!(
        dependents.len(),
        1,
        "Greet() usage should resolve via tree-sitter, got: {dependents:?}"
    );

    // Provenance records the fallback resolver (P3-M7, leg B).
    assert_eq!(
        metadata_value(dir.path(), "csharp_resolver").as_deref(),
        Some("treesitter"),
        "fallback leg must record csharp_resolver=treesitter"
    );
}

#[test]
fn test_sidecar_helper_missing_degrades_to_treesitter() {
    let dir = create_csharp_project();
    assert_sidecar_degrades(&dir, "/nonexistent/helios-roslyn.dll");
}

#[test]
fn test_sidecar_helper_broken_degrades_to_treesitter() {
    let dir = create_csharp_project();
    // A file that exists but is not a runnable helper: ping fails whether
    // dotnet is installed (non-zero exit) or not (spawn failure).
    let broken = dir.path().join("broken.dll");
    std::fs::write(&broken, "not a dotnet assembly").unwrap();
    assert_sidecar_degrades(&dir, broken.to_str().unwrap());
}

// --- End-to-end: semantic leg + mixed-language invariance (story 183: P3-M7/M8/M9) ---
//
// Leg B (fallback) runs unconditionally — the two degradation tests above.
// Leg A (semantic) is gated per spec A4: it needs a dotnet runtime and a
// dev-built helper DLL (CI builds `helios-roslyn` before `cargo test`); when
// either is missing the test skips with a note instead of failing.

/// Leg A prerequisites: dotnet on PATH and a built helper DLL — from
/// `HELIOS_ROSLYN` if set, else the dev-build path (`dotnet build helios-roslyn`).
/// `None` = skip (spec A4); leg B coverage is unaffected.
fn built_roslyn_dll() -> Option<PathBuf> {
    let dotnet_ok = Command::new("dotnet")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !dotnet_ok {
        eprintln!("skipping leg A e2e: dotnet not available");
        return None;
    }
    let dll = match std::env::var("HELIOS_ROSLYN") {
        Ok(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("helios-roslyn/bin/Debug/net8.0/helios-roslyn.dll"),
    };
    if !dll.is_file() {
        eprintln!(
            "skipping leg A e2e: helper DLL not built at {}",
            dll.display()
        );
        return None;
    }
    Some(dll)
}

/// Run `helios init` in `dir` with `HELIOS_ROSLYN` pointing at `dll`; assert
/// exit 0 and the expected provenance row.
fn init_with_sidecar(dir: &std::path::Path, dll: &std::path::Path, expected_resolver: &str) {
    let bin = helios_bin();
    let output = Command::new(&bin)
        .arg("init")
        .env("HELIOS_ROSLYN", dll)
        .current_dir(dir)
        .output()
        .expect("helios init");
    assert!(
        output.status.success(),
        "init must exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        metadata_value(dir, "csharp_resolver").as_deref(),
        Some(expected_resolver),
        "csharp_resolver provenance mismatch (P3-M7), stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// 1-based line of the first fixture line containing `needle`.
fn line_of(content: &str, needle: &str) -> i64 {
    content
        .lines()
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("fixture missing {needle:?}")) as i64
        + 1
}

const AMBIGUOUS_MODELS_CS: &str = r#"
namespace App {
    public class Alpha {
        public void Save() { }
    }
    public class Beta {
        public void Save() { }
    }
}
"#;

const AMBIGUOUS_PROGRAM_CS: &str = r#"
namespace App {
    public class Runner {
        public void Go(Alpha a, Beta b) {
            a.Save();
            b.Save();
        }
    }
}
"#;

/// Leg A (P3-M9): with dotnet + built helper, `helios init` resolves the
/// ambiguous-name case tree-sitter cannot — each `Save()` call site links to
/// exactly the right class's method, DocId-exact — and records
/// `csharp_resolver=roslyn`.
#[test]
fn test_e2e_semantic_leg_docid_exact_references_and_roslyn_provenance() {
    let Some(dll) = built_roslyn_dll() else {
        return;
    };
    let dir = tempfile::tempdir().expect("creating temp dir");
    std::fs::write(dir.path().join("Models.cs"), AMBIGUOUS_MODELS_CS).unwrap();
    std::fs::write(dir.path().join("Program.cs"), AMBIGUOUS_PROGRAM_CS).unwrap();

    init_with_sidecar(dir.path(), &dll, "roslyn");

    // Symbols carry stamped DocIds (P3-M3).
    let conn = index_db(dir.path());
    let stamped: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM symbols WHERE docid IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(stamped > 0, "no symbols carry a stamped docid");

    // Each Save() usage in Program.cs resolves to exactly one symbol, and the
    // symbol's docid is the intended definition's DocId (P3-M4/P3-M9 leg A).
    let mut stmt = conn
        .prepare(
            "SELECT r.line, s.docid FROM references_ r
             JOIN symbols s ON s.id = r.symbol_id
             JOIN files f ON f.id = r.file_id
             WHERE f.path = 'Program.cs' AND s.name = 'Save'",
        )
        .unwrap();
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    let alpha_line = line_of(AMBIGUOUS_PROGRAM_CS, "a.Save()");
    let beta_line = line_of(AMBIGUOUS_PROGRAM_CS, "b.Save()");
    let at = |line: i64| -> Vec<&str> {
        rows.iter()
            .filter(|(l, _)| *l == line)
            .map(|(_, d)| d.as_str())
            .collect()
    };
    assert_eq!(
        at(alpha_line),
        vec!["M:App.Alpha.Save"],
        "a.Save() must resolve to exactly Alpha.Save, got rows: {rows:?}"
    );
    assert_eq!(
        at(beta_line),
        vec!["M:App.Beta.Save"],
        "b.Save() must resolve to exactly Beta.Save, got rows: {rows:?}"
    );

    // Both call sites sit inside Runner.Go — the real Roslyn helper emits
    // container_docid on the wire, and it must resolve to that method's
    // symbol id.
    let mut stmt = conn
        .prepare(
            "SELECT c.name FROM references_ r
             JOIN symbols s ON s.id = r.symbol_id
             JOIN files f ON f.id = r.file_id
             LEFT JOIN symbols c ON c.id = r.container_symbol_id
             WHERE f.path = 'Program.cs' AND s.name = 'Save'",
        )
        .unwrap();
    let containers: Vec<Option<String>> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        containers,
        vec![Some("Go".to_string()), Some("Go".to_string())],
        "both Save() call sites are inside Runner.Go, got: {containers:?}"
    );
}

fn write_mixed_fixture(dir: &std::path::Path) {
    write_csharp_files(dir);
    // Rust file: an intra-file reference (helper) plus a call sharing the C#
    // symbol's name (Greet) — a cross-language name-match row sourced from a
    // non-.cs file, which the semantic `.cs` reference reset must not touch.
    std::fs::write(
        dir.join("main.rs"),
        r#"
pub fn build() -> i32 {
    Greet();
    helper()
}

fn helper() -> i32 {
    7
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("lib.py"),
        r#"
def fetch(path):
    return path


def run():
    return fetch("data.txt")
"#,
    )
    .unwrap();
}

/// Canonical dump of all non-C# symbol and reference rows, independent of
/// rowids and walk order, for byte-identical comparison across legs (P3-M8).
fn dump_non_cs_rows(conn: &rusqlite::Connection) -> String {
    let mut out = String::new();
    let mut stmt = conn
        .prepare(
            "SELECT f.path, s.name, s.kind, s.line, s.\"column\", s.end_line, s.visibility,
                    COALESCE(s.scope, ''), COALESCE(s.docid, '')
             FROM symbols s JOIN files f ON f.id = s.file_id
             WHERE f.language <> 'csharp'
             ORDER BY f.path, s.line, s.\"column\", s.name, s.kind",
        )
        .unwrap();
    let symbols = stmt
        .query_map([], |r| {
            Ok(format!(
                "symbol|{}|{}|{}|{}|{}|{}|{}|{}|{}\n",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        })
        .unwrap();
    for row in symbols {
        out.push_str(&row.unwrap());
    }
    // References *sourced from* non-C# files, keyed by target symbol identity
    // (file/name/line) rather than rowid.
    let mut stmt = conn
        .prepare(
            "SELECT sf.path, r.line, r.\"column\", df.path, s.name, s.line
             FROM references_ r
             JOIN files sf ON sf.id = r.file_id
             JOIN symbols s ON s.id = r.symbol_id
             JOIN files df ON df.id = s.file_id
             WHERE sf.language <> 'csharp'
             ORDER BY sf.path, r.line, r.\"column\", df.path, s.name, s.line",
        )
        .unwrap();
    let refs = stmt
        .query_map([], |r| {
            Ok(format!(
                "reference|{}|{}|{}|{}|{}|{}\n",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })
        .unwrap();
    for row in refs {
        out.push_str(&row.unwrap());
    }
    out
}

/// P3-M8: on a mixed-language fixture, non-C# symbol and reference rows are
/// byte-identical with the sidecar (leg A) and without it (leg B). Gated with
/// leg A since it needs a real semantic run to compare against.
#[test]
fn test_e2e_mixed_language_non_cs_rows_invariant_across_legs() {
    let Some(dll) = built_roslyn_dll() else {
        return;
    };

    let with_sidecar = tempfile::tempdir().expect("creating temp dir");
    write_mixed_fixture(with_sidecar.path());
    init_with_sidecar(with_sidecar.path(), &dll, "roslyn");

    let without_sidecar = tempfile::tempdir().expect("creating temp dir");
    write_mixed_fixture(without_sidecar.path());
    init_with_sidecar(
        without_sidecar.path(),
        std::path::Path::new("/nonexistent/helios-roslyn.dll"),
        "treesitter",
    );

    let semantic_rows = dump_non_cs_rows(&index_db(with_sidecar.path()));
    let syntactic_rows = dump_non_cs_rows(&index_db(without_sidecar.path()));
    assert!(
        !semantic_rows.is_empty(),
        "mixed fixture produced no non-C# rows — invariance check is vacuous"
    );
    assert!(
        semantic_rows.contains("reference|"),
        "mixed fixture produced no non-C# reference rows — invariance check is vacuous"
    );
    assert_eq!(
        semantic_rows, syntactic_rows,
        "non-C# rows must be byte-identical with and without the sidecar (P3-M8)"
    );
}

// --- update staleness warning under a semantic index (task 184) ---
//
// `update` never runs the Roslyn sidecar (measured: even a project-scoped
// analyze costs seconds of MSBuild load vs a sub-second tree-sitter update).
// Instead it must surface the fidelity trade: changed `.cs` files under a
// roslyn-provenance index warn that references degrade until the next init.

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("running git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_repo_with_commit(dir: &std::path::Path) {
    std::fs::write(dir.join(".gitignore"), ".helios/\n").unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "test@test"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "initial"]);
}

/// Run `helios update` in `dir`; assert exit 0 and return stderr.
fn update_stderr(dir: &std::path::Path) -> String {
    let output = Command::new(helios_bin())
        .arg("update")
        .current_dir(dir)
        .output()
        .expect("helios update");
    assert!(
        output.status.success(),
        "update must exit 0, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const STALE_HINT: &str = "run 'helios init' to refresh";

/// Leg A (gated on dotnet + helper DLL): a `.cs` change under a roslyn index
/// warns once on update, and provenance stays "roslyn" (it reflects the last
/// init, per spec Q1).
#[test]
fn test_update_warns_on_cs_change_under_semantic_index() {
    let Some(dll) = built_roslyn_dll() else {
        return;
    };
    let dir = tempfile::tempdir().expect("creating temp dir");
    write_csharp_files(dir.path());
    git_repo_with_commit(dir.path());
    init_with_sidecar(dir.path(), &dll, "roslyn");

    std::fs::write(
        dir.path().join("Person.cs"),
        "namespace App { public class Person { public void Greet() { } public void Wave() { } } }\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "change"]);

    let stderr = update_stderr(dir.path());
    assert!(
        stderr.contains(STALE_HINT),
        "update must warn about stale semantic references, stderr: {stderr}"
    );
    assert!(
        stderr.contains("1 C#/XAML file(s) changed"),
        "warning must count the changed .cs files, stderr: {stderr}"
    );
    assert_eq!(
        metadata_value(dir.path(), "csharp_resolver").as_deref(),
        Some("roslyn")
    );

    // A no-op update (nothing changed since) must not repeat the warning.
    let stderr = update_stderr(dir.path());
    assert!(
        !stderr.contains(STALE_HINT),
        "up-to-date update must not warn, stderr: {stderr}"
    );
}

/// Under a tree-sitter index the same `.cs` change warns nothing — the
/// warning is about semantic fidelity, not about C# changes per se.
#[test]
fn test_update_does_not_warn_under_treesitter_index() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    write_csharp_files(dir.path());
    git_repo_with_commit(dir.path());
    init_with_sidecar(
        dir.path(),
        std::path::Path::new("/nonexistent/helios-roslyn.dll"),
        "treesitter",
    );

    std::fs::write(
        dir.path().join("Person.cs"),
        "namespace App { public class Person { public void Greet() { } public void Wave() { } } }\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "change"]);

    let stderr = update_stderr(dir.path());
    assert!(
        !stderr.contains(STALE_HINT),
        "tree-sitter index must not warn about semantic staleness, stderr: {stderr}"
    );
}

/// `helios status --json` stale_files for the project in `dir`.
fn stale_count(dir: &std::path::Path) -> i64 {
    let output = Command::new(helios_bin())
        .args(["--json", "status"])
        .current_dir(dir)
        .output()
        .expect("helios status");
    assert!(output.status.success(), "status must exit 0");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("status JSON");
    json["stale_files"].as_i64().expect("stale_files present")
}

/// Staleness is what the index would have to re-read, not what git diffed
/// (task 840): a changed file helios has no parser for never counts, and an
/// uncommitted edit stops counting once `update` has indexed it.
#[test]
fn test_stale_count_excludes_unindexable_and_clears_after_update() {
    let dir = create_test_project();
    let bin = helios_bin();
    git_repo_with_commit(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");
    assert_eq!(stale_count(dir.path()), 0, "a fresh index is not stale");

    // A file with no parser is not stale however git reports it.
    std::fs::write(dir.path().join("README.md"), "# hello\n").unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "readme"]);
    std::fs::write(dir.path().join("README.md"), "# hello again\n").unwrap();
    assert_eq!(stale_count(dir.path()), 0, "README.md is never indexed");

    // An uncommitted source edit is stale until indexed, then clears — it is
    // in the diff against HEAD either way.
    std::fs::write(
        dir.path().join("main.rs"),
        "pub fn main() {}\npub fn added() {}\n",
    )
    .unwrap();
    assert_eq!(stale_count(dir.path()), 1, "the edited main.rs is stale");

    Command::new(&bin)
        .arg("update")
        .current_dir(dir.path())
        .output()
        .expect("update");
    assert_eq!(
        stale_count(dir.path()),
        0,
        "an indexed edit is no longer stale"
    );

    // ...and the next update has no work left to redo.
    let output = Command::new(&bin)
        .args(["--json", "update"])
        .current_dir(dir.path())
        .output()
        .expect("update");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("update JSON");
    assert_eq!(
        json["files_indexed"], 0,
        "already-indexed content must not be re-indexed: {json}"
    );
}

/// A rename is a delete plus an add to the index, not one edit to a path
/// spelled "old<TAB>new" — otherwise `update` keeps the old file's symbols
/// forever and never learns the new path, while `status` reports 0 stale.
#[test]
fn test_update_follows_renames() {
    let dir = create_test_project();
    let bin = helios_bin();
    git_repo_with_commit(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("init");

    git(dir.path(), &["mv", "main.rs", "entry.rs"]);
    git(dir.path(), &["commit", "-qm", "rename"]);
    assert_eq!(
        stale_count(dir.path()),
        2,
        "the rename is a delete and an add"
    );

    Command::new(&bin)
        .arg("update")
        .current_dir(dir.path())
        .output()
        .expect("update");

    let output = Command::new(&bin)
        .args(["--json", "files"])
        .current_dir(dir.path())
        .output()
        .expect("files");
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        listing.contains("entry.rs"),
        "the new path must be indexed: {listing}"
    );
    assert!(
        !listing.contains("main.rs"),
        "the old path must be dropped: {listing}"
    );
    assert_eq!(stale_count(dir.path()), 0, "the rename is fully absorbed");
}

/// An index rooted below the repo root stores paths relative to itself, while
/// git reports them relative to the repo root (task 849). Without rebasing,
/// `status`/`update`/`diff` look up `sub/sub/main.rs`, find nothing, and call a
/// genuinely stale index up to date. Changes outside the index root stay out.
#[test]
fn test_staleness_for_index_rooted_below_repo_root() {
    let repo = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    let sub = repo.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("main.rs"), "pub fn main() {}\n").unwrap();
    std::fs::write(repo.path().join("outside.rs"), "pub fn outside() {}\n").unwrap();
    git_repo_with_commit(repo.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(&sub)
        .output()
        .expect("init");
    assert_eq!(stale_count(&sub), 0, "a fresh index is not stale");

    // Only the file inside the index root counts.
    std::fs::write(repo.path().join("outside.rs"), "pub fn moved() {}\n").unwrap();
    assert_eq!(stale_count(&sub), 0, "a change above the index root is not");

    std::fs::write(sub.join("main.rs"), "pub fn main() {}\npub fn added() {}\n").unwrap();
    assert_eq!(stale_count(&sub), 1, "the edited sub/main.rs is stale");

    // `diff` reads the same change, spelled as the index spells it.
    let output = Command::new(&bin)
        .args(["--json", "diff"])
        .current_dir(&sub)
        .output()
        .expect("diff");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("diff JSON");
    assert_eq!(
        json["added"][0]["name"], "added",
        "diff must see the new symbol: {json}"
    );
    assert_eq!(
        json["added"][0]["file"], "main.rs",
        "diff paths are index-relative: {json}"
    );

    // A user's `diff.relative = true` must not change the answer: it makes git
    // pre-strip the prefix helios then expects to strip itself.
    git(repo.path(), &["config", "diff.relative", "true"]);
    assert_eq!(stale_count(&sub), 1, "diff.relative must not hide the edit");

    let output = Command::new(&bin)
        .args(["--json", "update"])
        .current_dir(&sub)
        .output()
        .expect("update");
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("update JSON");
    assert_eq!(json["files_indexed"], 1, "update must re-index it: {json}");
    assert_eq!(stale_count(&sub), 0, "and the index is then current");
}

/// An ambiguous symbol name is stored with one reference row per candidate
/// definition; `deps` must still report each usage site once (task 837).
#[test]
fn test_deps_symbol_references_are_deduplicated() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("money.ts"),
        "export function formatMoney(n: number): string { return `$${n}`; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("legacy.ts"),
        "export function formatMoney(n: number): string { return n + ' USD'; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("cart.ts"),
        "import { formatMoney } from './money';\nexport function total(a: number, b: number): string {\n    return formatMoney(a) + formatMoney(b);\n}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let output = Command::new(&bin)
        .args(["deps", "formatMoney"])
        .current_dir(dir.path())
        .output()
        .expect("deps formatMoney");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let refs: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("(reference)"))
        .collect();
    let mut unique = refs.clone();
    unique.sort_unstable();
    unique.dedup();
    assert!(!refs.is_empty(), "expected references, got: {stdout}");
    assert_eq!(
        refs.len(),
        unique.len(),
        "deps must not repeat a usage site, got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["--json", "deps", "formatMoney"])
        .current_dir(dir.path())
        .output()
        .expect("deps --json formatMoney");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let dependents = value["dependents"].as_array().expect("dependents array");
    let mut seen: Vec<String> = dependents.iter().map(|d| d.to_string()).collect();
    let total = seen.len();
    seen.sort();
    seen.dedup();
    assert!(total > 0, "expected dependents, got: {stdout}");
    assert_eq!(
        total,
        seen.len(),
        "JSON dependents must be unique: {stdout}"
    );
}

/// `helios deps <fn>` names the calling function in its References output,
/// both in the human-readable form and in `--json`.
#[test]
fn test_deps_references_name_the_calling_function() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("wallet.ts"),
        "export function helper(): number { return 1; }\nexport class Wallet {\n    format(): number {\n        return helper();\n    }\n}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let output = Command::new(&bin)
        .args(["deps", "helper"])
        .current_dir(dir.path())
        .output()
        .expect("deps helper");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("in format ->"),
        "expected the caller's function name in the References output, got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["--json", "deps", "helper"])
        .current_dir(dir.path())
        .output()
        .expect("deps --json helper");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let dependents = value["dependents"].as_array().expect("dependents array");
    assert_eq!(dependents.len(), 1, "expected one dependent, got: {stdout}");
    assert_eq!(dependents[0]["container"].as_str(), Some("format"));
}

/// A project with one call reference (`helper()` called from `Wallet.format`)
/// — the same fixture `test_deps_references_name_the_calling_function` uses
/// — for the `deps --reads`/`--writes` filtering tests below.
fn setup_single_reference_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("wallet.ts"),
        "export function helper(): number { return 1; }\nexport class Wallet {\n    format(): number {\n        return helper();\n    }\n}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");
    (dir, bin)
}

/// `--json` dependents entries carry a `usage_kind`, and for an ordinary
/// tree-sitter call reference — the only kind today's parsers emit — it is
/// `"read"`.
#[test]
fn test_deps_json_dependent_carries_usage_kind_read() {
    let (dir, bin) = setup_single_reference_project();

    let value = deps_json(&bin, dir.path(), &["helper"]);
    let dependents = value["dependents"].as_array().expect("dependents array");
    assert_eq!(dependents.len(), 1, "expected one dependent, got: {value}");
    assert_eq!(
        dependents[0]["usage_kind"].as_str(),
        Some("read"),
        "an ordinary call reference must be classified read: {value}"
    );
}

/// `--reads` on a symbol whose only references are calls keeps them: asking
/// for reads must not filter away a read.
#[test]
fn test_deps_reads_flag_keeps_read_references() {
    let (dir, bin) = setup_single_reference_project();

    let unfiltered = deps_json(&bin, dir.path(), &["helper"]);
    let reads = deps_json(&bin, dir.path(), &["helper", "--reads"]);
    assert!(
        !reads["dependents"].as_array().unwrap().is_empty(),
        "expected at least one dependent, got: {reads}"
    );
    assert_eq!(
        reads["dependents"], unfiltered["dependents"],
        "--reads must return the same set of read references unfiltered"
    );
}

/// Passing both `--reads` and `--writes` applies no filter — same result as
/// passing neither.
#[test]
fn test_deps_reads_and_writes_together_means_no_filter() {
    let (dir, bin) = setup_single_reference_project();

    let neither = deps_json(&bin, dir.path(), &["helper"]);
    let both = deps_json(&bin, dir.path(), &["helper", "--reads", "--writes"]);
    assert_eq!(
        both["dependents"], neither["dependents"],
        "both flags together must mean no filtering, the same as neither"
    );
}

/// Default text output for a plain read reference is unchanged by
/// `usage_kind`: no `[write]`/`[readwrite]` suffix, nothing appended to the
/// line format everyone already reads.
#[test]
fn test_deps_text_output_unchanged_for_read_reference() {
    let (dir, bin) = setup_single_reference_project();

    let (path, line, col): (String, i64, i64) = index_db(dir.path())
        .query_row(
            "SELECT f.path, r.line, r.column FROM references_ r JOIN files f ON f.id = r.file_id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("finding the seeded reference row");

    let output = Command::new(&bin)
        .args(["deps", "helper"])
        .current_dir(dir.path())
        .output()
        .expect("deps helper");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let expected_line = format!("  {}:{}:{} in format -> helper (reference)", path, line, col);
    assert!(
        stdout.lines().any(|l| l == expected_line),
        "expected the pre-existing reference line format exactly, got: {stdout}"
    );
    assert!(
        !stdout.contains("[write]") && !stdout.contains("[readwrite]"),
        "a read reference must never carry a write/readwrite suffix, got: {stdout}"
    );
}

/// `--writes` on a symbol that is only ever called (never assigned) returns
/// no reference lines.
#[test]
fn test_deps_writes_flag_excludes_call_only_symbol() {
    let (dir, bin) = setup_single_reference_project();

    let value = deps_json(&bin, dir.path(), &["helper", "--writes"]);
    assert_eq!(
        value["dependents"].as_array().unwrap().len(),
        0,
        "a call-only symbol has no writes: {value}"
    );

    let output = Command::new(&bin)
        .args(["deps", "helper", "--writes"])
        .current_dir(dir.path())
        .output()
        .expect("deps helper --writes");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("(reference)"),
        "expected no reference lines, got: {stdout}"
    );
}

/// The central claim of this feature: an `unknown`-kind reference is
/// excluded by both `--reads` and `--writes`, while still appearing with
/// neither flag — an unclassified usage is not evidence of a read or of a
/// write.
///
/// No tree-sitter parser emits `unknown` today (every capture reads its
/// target — see WP1/WP2), and the Roslyn sidecar path needs a real `dotnet`
/// build (`built_roslyn_dll()`, gated and skipped when unavailable) to
/// exercise end to end. So this seeds an `unknown` row directly into
/// `references_` after a real `helios init`, the same seeding style
/// `test_deps_type_edge_external_cross_language_shows_declaring_file`
/// already uses for `type_relations`, and then drives the filtering purely
/// through the CLI (`helios deps --reads`/`--writes`) against that real
/// index — this is a CLI-level test of the filtering guarantee, not a
/// restatement of the `Database::symbol_references` storage-layer unit test
/// in src/db.rs.
#[test]
fn test_deps_unknown_kind_reference_excluded_by_both_flags() {
    let (dir, bin) = setup_single_reference_project();

    let (symbol_id, file_id): (i64, i64) = index_db(dir.path())
        .query_row(
            "SELECT r.symbol_id, r.file_id FROM references_ r LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("finding the seeded reference row");
    index_db(dir.path())
        .execute(
            "INSERT INTO references_ (symbol_id, file_id, line, column, qualified, usage_kind)
             VALUES (?1, ?2, 999, 0, 0, 'unknown')",
            rusqlite::params![symbol_id, file_id],
        )
        .expect("seeding an unknown-kind reference");

    let lines_of = |value: &serde_json::Value| -> Vec<i64> {
        value["dependents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["line"].as_i64().unwrap())
            .collect()
    };

    let unfiltered = deps_json(&bin, dir.path(), &["helper"]);
    assert!(
        lines_of(&unfiltered).contains(&999),
        "the unknown-kind row must appear with no filter: {unfiltered}"
    );

    let reads = deps_json(&bin, dir.path(), &["helper", "--reads"]);
    assert!(
        !lines_of(&reads).contains(&999),
        "an unknown-kind row is not evidence of a read: {reads}"
    );

    let writes = deps_json(&bin, dir.path(), &["helper", "--writes"]);
    assert_eq!(
        writes["dependents"].as_array().unwrap().len(),
        0,
        "an unknown-kind row is not evidence of a write, and the natural \
         reference is a read, so --writes must return nothing: {writes}"
    );
}

/// `--reads`/`--writes` sort `write` and `readwrite` rows correctly at the
/// CLI level, the same way `test_deps_unknown_kind_reference_excluded_by_both_flags`
/// covers `unknown`.
///
/// No tree-sitter parser can attach a correct `write`/`readwrite` reference
/// to a real definition today: none of the six languages index struct/class
/// fields as symbols, so a member write has no correct symbol to attach to
/// and would land on whatever unrelated same-named symbol happens to exist
/// instead — a confident false claim, which is why the tree-sitter write
/// captures were pulled rather than shipped with that defect. The `write`/
/// `readwrite` path is real end-to-end only via the Roslyn leg (C#
/// properties, which are indexed symbols with exact docids), and exercising
/// that here would need a live `dotnet` build (`built_roslyn_dll()`, gated
/// and skipped when unavailable). So — exactly the seeding technique
/// `test_deps_unknown_kind_reference_excluded_by_both_flags` uses for
/// `unknown` — this seeds `write` and `readwrite` rows directly into a real
/// index's `references_` table, then drives the filtering purely through
/// the CLI. This keeps `--writes`/`--reads` covered at the CLI level for
/// every kind the enum has, independent of which producer (Roslyn today,
/// tree-sitter once fields are indexed) supplies the row.
#[test]
fn test_deps_write_and_readwrite_kinds_sorted_by_flags_at_cli_level() {
    let (dir, bin) = setup_single_reference_project();

    let (symbol_id, file_id): (i64, i64) = index_db(dir.path())
        .query_row(
            "SELECT r.symbol_id, r.file_id FROM references_ r LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("finding the seeded reference row");
    {
        let conn = index_db(dir.path());
        conn.execute(
            "INSERT INTO references_ (symbol_id, file_id, line, column, qualified, usage_kind)
             VALUES (?1, ?2, 111, 0, 0, 'write')",
            rusqlite::params![symbol_id, file_id],
        )
        .expect("seeding a write-kind reference");
        conn.execute(
            "INSERT INTO references_ (symbol_id, file_id, line, column, qualified, usage_kind)
             VALUES (?1, ?2, 222, 0, 0, 'readwrite')",
            rusqlite::params![symbol_id, file_id],
        )
        .expect("seeding a readwrite-kind reference");
    }
    // The fixture's own natural reference is a read (line unknown to this
    // test, so identify the two seeded rows by their distinctive lines
    // instead of asserting the natural row's absence/presence by exclusion).

    let lines_of = |value: &serde_json::Value| -> Vec<i64> {
        value["dependents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["line"].as_i64().unwrap())
            .collect()
    };

    let unfiltered = deps_json(&bin, dir.path(), &["helper"]);
    let unfiltered_lines = lines_of(&unfiltered);
    assert!(
        unfiltered_lines.contains(&111) && unfiltered_lines.contains(&222),
        "both seeded rows must appear with no filter: {unfiltered}"
    );

    let writes = deps_json(&bin, dir.path(), &["helper", "--writes"]);
    let write_lines = lines_of(&writes);
    assert!(
        write_lines.contains(&111) && write_lines.contains(&222),
        "--writes must keep both the write and the readwrite row: {writes}"
    );

    let reads = deps_json(&bin, dir.path(), &["helper", "--reads"]);
    let read_lines = lines_of(&reads);
    assert!(
        !read_lines.contains(&111),
        "--reads must exclude the pure-write row: {reads}"
    );
    assert!(
        read_lines.contains(&222),
        "--reads must keep the readwrite row: {reads}"
    );
}

/// A project with two `formatMoney` definitions whose defining files import
/// different things, so a deps target that selects one is visibly different
/// from one that selects the other.
fn setup_decoy_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::create_dir_all(dir.path().join("src/util")).unwrap();
    std::fs::create_dir_all(dir.path().join("src/legacy")).unwrap();

    std::fs::write(
        dir.path().join("src/util/round.ts"),
        "export function round(n: number): number { return Math.round(n); }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/legacy/pad.ts"),
        "export function pad(s: string): string { return ' ' + s; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/util/money.ts"),
        "import { round } from './round';\nexport function formatMoney(n: number): string { return `$${round(n)}`; }\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/legacy/money.ts"),
        "import { pad } from './pad';\nexport function formatMoney(n: number): string { return pad(n + ' USD'); }\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed during setup");
    (dir, bin)
}

fn deps_json(bin: &PathBuf, dir: &std::path::Path, args: &[&str]) -> serde_json::Value {
    let mut full = vec!["--json", "deps"];
    full.extend_from_slice(args);
    let output = Command::new(bin)
        .args(&full)
        .current_dir(dir)
        .output()
        .expect("deps --json");
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("parsing deps JSON ({e}): {stdout}"))
}

fn definition_paths(value: &serde_json::Value) -> Vec<String> {
    value["definitions"]
        .as_array()
        .expect("definitions array")
        .iter()
        .map(|d| d["path"].as_str().expect("definition path").to_string())
        .collect()
}

#[test]
fn test_deps_file_flag_selects_one_definition() {
    let (dir, bin) = setup_decoy_project();

    let both = deps_json(&bin, dir.path(), &["formatMoney"]);
    assert_eq!(
        definition_paths(&both),
        vec!["src/legacy/money.ts", "src/util/money.ts"],
        "an unnarrowed target still covers every definition"
    );

    let util = deps_json(&bin, dir.path(), &["formatMoney", "--file", "src/util"]);
    assert_eq!(definition_paths(&util), vec!["src/util/money.ts"]);
    assert_eq!(
        util["dependencies"],
        serde_json::json!(["./round"]),
        "only the selected definition's file imports count: {util}"
    );

    let legacy = deps_json(&bin, dir.path(), &["formatMoney", "--file", "src/legacy"]);
    assert_eq!(definition_paths(&legacy), vec!["src/legacy/money.ts"]);
    assert_eq!(
        legacy["dependencies"],
        serde_json::json!(["./pad"]),
        "the decoy definition's deps must not leak in: {legacy}"
    );
}

#[test]
fn test_deps_file_qualified_target_selects_one_definition() {
    let (dir, bin) = setup_decoy_project();

    let value = deps_json(&bin, dir.path(), &["src/util/money.ts:formatMoney"]);
    assert_eq!(definition_paths(&value), vec!["src/util/money.ts"]);
    assert_eq!(value["dependencies"], serde_json::json!(["./round"]));
}

#[test]
fn test_deps_scope_flag_and_qualified_name_select_one_definition() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::write(
        dir.path().join("Promo.cs"),
        "namespace App {\n  class PromoPricing { public int Compute(int x) { return x * 2; } }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("Legacy.cs"),
        "namespace App {\n  class LegacyPricing { public int Compute(int x) { return x; } }\n}\n",
    )
    .unwrap();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    // The bug this replaces: a qualified name was read as a file path, so it
    // matched nothing at all.
    let qualified = deps_json(&bin, dir.path(), &["PromoPricing.Compute"]);
    let defs = qualified["definitions"].as_array().expect("definitions");
    assert_eq!(defs.len(), 1, "expected one definition: {qualified}");
    assert_eq!(defs[0]["path"], "Promo.cs");
    assert_eq!(defs[0]["scope"], "PromoPricing");

    let scoped = deps_json(&bin, dir.path(), &["Compute", "--scope", "LegacyPricing"]);
    let defs = scoped["definitions"].as_array().expect("definitions");
    assert_eq!(defs.len(), 1, "expected one definition: {scoped}");
    assert_eq!(defs[0]["path"], "Legacy.cs");

    // Unnarrowed, the name is still ambiguous.
    let bare = deps_json(&bin, dir.path(), &["Compute"]);
    assert_eq!(
        bare["definitions"].as_array().expect("definitions").len(),
        2
    );
}

/// A dotted target that names no definition is still a module path.
#[test]
fn test_deps_dotted_target_falls_back_to_file_mode() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::create_dir_all(dir.path().join("pkg")).unwrap();
    std::fs::write(dir.path().join("pkg/__init__.py"), "").unwrap();
    std::fs::write(
        dir.path().join("pkg/money.py"),
        "def fmt(n):\n    return n\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("app.py"),
        "import pkg.money\n\ndef go():\n    return pkg.money.fmt(1)\n",
    )
    .unwrap();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let value = deps_json(&bin, dir.path(), &["pkg.money"]);
    assert!(
        value.get("definitions").is_none(),
        "expected file-mode output for a module path: {value}"
    );
    let dependents: Vec<&str> = value["dependents"]
        .as_array()
        .expect("dependents array")
        .iter()
        .map(|d| d["path"].as_str().expect("path"))
        .collect();
    assert!(
        dependents.contains(&"app.py"),
        "expected app.py to import pkg.money: {value}"
    );
}

// --- flow subcommand ---

/// A project whose Rust sources give `flow` something to walk: a branching
/// method inside an impl block, a free function whose name is deliberately
/// duplicated across two files, and a Python function for the
/// unsupported-language path.
fn setup_flow_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();

    std::fs::write(
        dir.path().join("src/pricing.rs"),
        r#"pub struct Pricing {
    pub rate: i32,
}

impl Pricing {
    pub fn compute(&self, units: i32) -> i32 {
        if units > 10 {
            discount(units)
        } else {
            units * self.rate
        }
    }
}

pub fn discount(units: i32) -> i32 {
    units / 2
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("src/legacy.rs"),
        "pub fn discount(units: i32) -> i32 {\n    units\n}\n",
    )
    .unwrap();

    std::fs::write(
        dir.path().join("lib.py"),
        "def summarize(rows):\n    if rows:\n        return len(rows)\n    return 0\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed during setup");
    (dir, bin)
}

fn flow_output(bin: &PathBuf, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let mut full = vec!["flow"];
    full.extend_from_slice(args);
    Command::new(bin)
        .args(&full)
        .current_dir(dir)
        .output()
        .expect("helios flow")
}

fn flow_stdout(bin: &PathBuf, dir: &std::path::Path, args: &[&str]) -> String {
    let output = flow_output(bin, dir, args);
    assert!(
        output.status.success(),
        "helios flow {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn test_flow_tree_output() {
    let (dir, bin) = setup_flow_project();

    let stdout = flow_stdout(&bin, dir.path(), &["Pricing.compute"]);

    assert!(
        stdout.starts_with("src/pricing.rs:6-12 compute\n"),
        "tree should be headed by the function's location: {stdout}"
    );
    assert!(
        stdout.contains("entry Pricing.compute(&self, units: i32) -> i32"),
        "tree should open at the entry node: {stdout}"
    );
    assert!(
        stdout.contains("branch units > 10"),
        "the if condition should appear as a branch: {stdout}"
    );
    assert!(
        stdout.contains("[true]") && stdout.contains("[false]"),
        "both branch arms should be labelled: {stdout}"
    );
    assert!(
        stdout.contains("call discount(…)"),
        "the call in the true arm should appear: {stdout}"
    );
    assert!(
        stdout.contains("exit end"),
        "the tree should reach the exit node: {stdout}"
    );
}

#[test]
fn test_flow_json_output() {
    let (dir, bin) = setup_flow_project();

    let output = Command::new(&bin)
        .args(["--json", "flow", "Pricing.compute"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json flow");
    assert!(output.status.success(), "helios --json flow failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("parsing flow JSON ({e}): {stdout}"));

    assert_eq!(value["function"]["name"], "compute");
    assert_eq!(value["function"]["scope"], "Pricing");
    assert_eq!(value["function"]["file"], "src/pricing.rs");
    assert_eq!(value["function"]["language"], "rust");

    let kinds: Vec<&str> = value["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|n| n["kind"].as_str().expect("node kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["entry", "exit", "branch", "call", "return", "return"],
        "unexpected node kinds: {stdout}"
    );

    let edges = value["edges"].as_array().expect("edges array");
    let labels: Vec<&str> = edges.iter().filter_map(|e| e["label"].as_str()).collect();
    assert_eq!(
        labels,
        vec!["true", "false"],
        "unexpected edge labels: {stdout}"
    );
    assert!(
        edges
            .iter()
            .all(|e| e["from"].is_number() && e["to"].is_number()),
        "every edge needs numeric endpoints: {stdout}"
    );
}

#[test]
fn test_flow_mermaid_output() {
    let (dir, bin) = setup_flow_project();

    let stdout = flow_stdout(&bin, dir.path(), &["Pricing.compute", "--mermaid"]);

    assert!(
        stdout.starts_with("flowchart TD\n"),
        "mermaid output should open with the flowchart header: {stdout}"
    );
    assert!(
        stdout.contains("n2{\"units > 10\"}"),
        "a branch node should use the decision shape: {stdout}"
    );
    assert!(
        stdout.contains("n0 --> n2"),
        "unlabelled edges should be plain arrows: {stdout}"
    );
    assert!(
        stdout.contains("-->|\"true\"|") && stdout.contains("-->|\"false\"|"),
        "branch edges should carry their labels: {stdout}"
    );
}

#[test]
fn test_flow_target_spellings_resolve() {
    let (dir, bin) = setup_flow_project();

    // A bare unique name, and the same definition reached via Scope.Method.
    let bare = flow_stdout(&bin, dir.path(), &["compute"]);
    let scoped_name = flow_stdout(&bin, dir.path(), &["Pricing.compute"]);
    assert_eq!(bare, scoped_name);
    assert!(bare.starts_with("src/pricing.rs:6-12 compute\n"), "{bare}");

    // --scope reaches the same definition as the dotted spelling.
    let scope_flag = flow_stdout(&bin, dir.path(), &["compute", "--scope", "Pricing"]);
    assert_eq!(scope_flag, scoped_name);

    // The duplicated name needs narrowing: path:name and --file both work, and
    // they select genuinely different bodies.
    let qualified = flow_stdout(&bin, dir.path(), &["src/pricing.rs:discount"]);
    assert!(
        qualified.starts_with("src/pricing.rs:15-17 discount\n") && qualified.contains("units / 2"),
        "path-qualified target should select the pricing.rs body: {qualified}"
    );

    let by_file = flow_stdout(&bin, dir.path(), &["discount", "--file", "src/legacy.rs"]);
    assert!(
        by_file.starts_with("src/legacy.rs:1-3 discount\n") && !by_file.contains("units / 2"),
        "--file should select the legacy.rs body: {by_file}"
    );
}

#[test]
fn test_flow_ambiguous_target_errors() {
    let (dir, bin) = setup_flow_project();

    let output = flow_output(&bin, dir.path(), &["discount"]);

    let code = output.status.code().expect("should have exit code");
    assert_eq!(
        code, 1,
        "an ambiguous flow target should exit 1, got {code}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("matches 2 definitions"),
        "stderr should report the ambiguity, got: {stderr}"
    );
    assert!(
        stderr.contains("narrow with --file, --scope or --line"),
        "stderr should suggest narrowing, got: {stderr}"
    );
    assert!(
        stderr.contains("src/pricing.rs:15") && stderr.contains("src/legacy.rs:1"),
        "stderr should list both candidates, got: {stderr}"
    );
}

#[test]
fn test_flow_unknown_target_errors() {
    let (dir, bin) = setup_flow_project();

    let output = flow_output(&bin, dir.path(), &["nosuchfunction"]);

    let code = output.status.code().expect("should have exit code");
    assert_eq!(code, 1, "an unknown flow target should exit 1, got {code}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no function named nosuchfunction"),
        "stderr should name the missing function, got: {stderr}"
    );
}

#[test]
fn test_flow_no_index_exits_2() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    let output = flow_output(&bin, dir.path(), &["anything"]);

    let code = output.status.code().expect("should have exit code");
    assert_eq!(
        code, 2,
        "helios flow without index should exit 2, got {code}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No index found"),
        "stderr should mention the missing index, got: {stderr}"
    );
}

#[test]
fn test_flow_unsupported_language_errors() {
    let (dir, bin) = setup_flow_project();

    let output = flow_output(&bin, dir.path(), &["summarize"]);

    let code = output.status.code().expect("should have exit code");
    assert_eq!(code, 1, "a non-Rust flow target should exit 1, got {code}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("flow does not support python yet"),
        "stderr should name the unsupported language, got: {stderr}"
    );
    assert!(
        stderr.contains("supported: rust"),
        "stderr should say what is supported, got: {stderr}"
    );
}

/// A bare name that happens to spell a file extension is still a function.
/// Target parsing used to read `go` as a path and reject it outright with
/// "flow needs a function or method, not a file".
#[test]
fn test_flow_extension_like_name_resolves() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::write(
        dir.path().join("ext.rs"),
        "pub fn go() -> i32 {\n    rs()\n}\n\npub fn rs() -> i32 {\n    1\n}\n",
    )
    .unwrap();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let go = flow_stdout(&bin, dir.path(), &["go"]);
    assert!(
        go.starts_with("ext.rs:1-3 go\n"),
        "`go` should resolve to the function, not be read as a file: {go}"
    );
    assert!(
        go.contains("entry go() -> i32") && go.contains("call rs(…)"),
        "the graph should be `go`'s own body: {go}"
    );

    let rs = flow_stdout(&bin, dir.path(), &["rs"]);
    assert!(
        rs.starts_with("ext.rs:5-7 rs\n") && rs.contains("return 1"),
        "`rs` should resolve to the function too: {rs}"
    );
}

/// `--scope` must be checked against the source, not just printed as a label.
/// The index stores a line number, so once the file shifts under a stale index
/// the recorded line lands in the wrong impl block — the scope is what tells
/// the two `go` methods apart.
#[test]
fn test_flow_scope_survives_a_stale_index() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    let source = dir.path().join("pair.rs");
    std::fs::write(
        &source,
        "pub struct A;\npub struct B;\n\nimpl A {\n    pub fn go(&self) -> i32 {\n        11\n    }\n}\n\nimpl B {\n    pub fn go(&self) -> i32 {\n        22\n    }\n}\n",
    )
    .unwrap();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let fresh = flow_stdout(&bin, dir.path(), &["go", "--scope", "B"]);
    assert!(
        fresh.contains("entry B.go(&self) -> i32") && fresh.contains("return 22"),
        "--scope B should select B's body: {fresh}"
    );
    assert!(
        !fresh.contains("return 11"),
        "A's body must not appear: {fresh}"
    );

    // Shift every definition down three lines, leaving the index stale: the
    // recorded line for B.go now points inside `impl A`.
    let shifted = format!(
        "// a comment added after indexing\n// another comment\n// and a third\n{}",
        std::fs::read_to_string(&source).expect("reading fixture")
    );
    std::fs::write(&source, shifted).unwrap();

    let stale = flow_stdout(&bin, dir.path(), &["go", "--scope", "B"]);
    assert!(
        stale.contains("entry B.go(&self) -> i32") && stale.contains("return 22"),
        "--scope B must still select B's body under a stale index: {stale}"
    );
    assert!(
        !stale.contains("return 11"),
        "A's body must not be shown labelled as B.go: {stale}"
    );
}

/// A C# project whose methods exercise the constructs `flow` maps: an if/else,
/// a foreach with `continue` and `break`, a switch, a try/catch and a throw in
/// `Total`; a switch with no default and both `yield` forms in `Pending`.
/// `Rate` is an arrow-bodied property, the other shape of C# body.
fn setup_csharp_flow_project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("Orders.cs"),
        r#"using System;

namespace Shop {
    public class Orders {
        public int Total(int units) {
            if (units > 10) {
                Log("bulk");
            } else {
                Log("retail");
            }

            foreach (var item in Fetch()) {
                if (Skip(item)) {
                    continue;
                }
                if (Done(item)) {
                    break;
                }
                Handle(item);
            }

            switch (units) {
                case 0:
                    Log("empty");
                    break;
                default:
                    Log("some");
                    break;
            }

            try {
                Commit();
            } catch (InvalidOperationException e) {
                Rollback(e);
            }

            if (units < 0) {
                throw new ArgumentException("negative");
            }
            return units * 2;
        }

        public IEnumerable<int> Pending(int mode) {
            switch (mode) {
                case 0:
                    Log("none");
                    break;
            }
            yield return Next();
            yield break;
        }

        public int Rate => Lookup();
    }
}
"#,
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed during setup");
    (dir, bin)
}

#[test]
fn test_flow_csharp_tree_output() {
    let (dir, bin) = setup_csharp_flow_project();

    let stdout = flow_stdout(&bin, dir.path(), &["Orders.Total"]);

    assert!(
        stdout.starts_with("Orders.cs:5-41 Total\n"),
        "tree should be headed by the method's location: {stdout}"
    );
    assert!(
        stdout.contains("entry Orders.Total(int units) -> int"),
        "tree should open at the entry node: {stdout}"
    );
    assert!(
        stdout.contains("branch units > 10") && stdout.contains("[true]"),
        "the if condition should appear as a branch: {stdout}"
    );
    assert!(
        stdout.contains("loop foreach (var item in Fetch())"),
        "the foreach header should appear as a loop: {stdout}"
    );
    for marker in ["[body]", "[repeat]", "[done]"] {
        assert!(
            stdout.contains(marker),
            "the loop should carry {marker}: {stdout}"
        );
    }
    assert!(
        stdout.contains("continue continue;") && stdout.contains("break break;"),
        "continue and break should be nodes: {stdout}"
    );
    assert!(
        stdout.contains("match switch units") && stdout.contains("[default]"),
        "the switch and its default label should appear: {stdout}"
    );
    assert!(
        stdout.contains("branch try") && stdout.contains("[catch (InvalidOperationException e)]"),
        "the try and its catch edge should appear: {stdout}"
    );
    assert!(
        stdout.contains("throw throw new ArgumentException(\"negative\");"),
        "the throw should be its own node: {stdout}"
    );
    assert!(
        stdout.contains("call new ArgumentException(…)"),
        "object creation should be a call node: {stdout}"
    );
    assert!(
        stdout.contains("return return units * 2;") && stdout.contains("exit end"),
        "the tree should reach the exit node: {stdout}"
    );
}

#[test]
fn test_flow_csharp_json_output() {
    let (dir, bin) = setup_csharp_flow_project();

    let output = Command::new(&bin)
        .args(["--json", "flow", "Orders.Rate"])
        .current_dir(dir.path())
        .output()
        .expect("helios --json flow");
    assert!(output.status.success(), "helios --json flow failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("parsing flow JSON ({e}): {stdout}"));

    assert_eq!(value["function"]["name"], "Rate");
    assert_eq!(value["function"]["scope"], "Orders");
    assert_eq!(value["function"]["file"], "Orders.cs");
    assert_eq!(value["function"]["language"], "csharp");
    assert_eq!(value["function"]["returns"], "int");

    let kinds: Vec<&str> = value["nodes"]
        .as_array()
        .expect("nodes array")
        .iter()
        .map(|n| n["kind"].as_str().expect("node kind"))
        .collect();
    assert_eq!(
        kinds,
        vec!["entry", "exit", "call", "return"],
        "an arrow body is a call and a return: {stdout}"
    );

    let edges = value["edges"].as_array().expect("edges array");
    assert!(
        edges
            .iter()
            .all(|e| e["from"].is_number() && e["to"].is_number()),
        "every edge needs numeric endpoints: {stdout}"
    );
}

#[test]
fn test_flow_csharp_mermaid_output() {
    let (dir, bin) = setup_csharp_flow_project();

    let stdout = flow_stdout(&bin, dir.path(), &["Orders.Total", "--mermaid"]);

    assert!(
        stdout.starts_with("flowchart TD\n"),
        "mermaid output should open with the flowchart header: {stdout}"
    );
    assert!(
        stdout.contains("n0([\"Orders.Total(int units) -> int\"])"),
        "the entry node should carry the signature: {stdout}"
    );
    assert!(
        stdout.contains("{\"units > 10\"}"),
        "a branch node should use the decision shape: {stdout}"
    );
    assert!(
        stdout.contains("{{\"foreach (var item in Fetch())\"}}"),
        "a loop header should use the loop shape: {stdout}"
    );
    assert!(
        stdout.contains("-->|\"true\"|") && stdout.contains("-->|\"default\"|"),
        "branch and switch edges should carry their labels: {stdout}"
    );
}

#[test]
fn test_flow_csharp_no_match_edge_and_yield_nodes() {
    let (dir, bin) = setup_csharp_flow_project();

    let stdout = flow_stdout(&bin, dir.path(), &["Orders.Pending"]);

    assert!(
        stdout.contains("match switch mode"),
        "the switch should appear: {stdout}"
    );
    assert!(
        stdout.contains("[no match]"),
        "a switch with no default can match nothing: {stdout}"
    );
    assert!(
        stdout.contains("yield yield return Next();"),
        "`yield return` should be its own kind: {stdout}"
    );
    assert!(
        stdout.contains("return yield break;"),
        "`yield break` should end the iterator: {stdout}"
    );
    assert!(
        stdout.contains("exit end"),
        "the tree should reach the exit node: {stdout}"
    );
}

/// Two overloads share a name, a scope and a file, so neither `--scope` nor
/// `--file` can tell them apart; `--line` is what picks one.
#[test]
fn test_flow_csharp_overloads_need_line() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::write(
        dir.path().join("Math.cs"),
        r#"public class Calc {
    public int Add(int a) {
        return One(a);
    }

    public int Add(int a, int b) {
        return Two(a, b);
    }
}
"#,
    )
    .unwrap();
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    // Ambiguous on its own, and the advice names the flag that can fix it.
    let output = flow_output(&bin, dir.path(), &["Calc.Add"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("matches 2 definitions"),
        "stderr should report the ambiguity: {stderr}"
    );
    assert!(
        stderr.contains("narrow with --file, --scope or --line"),
        "stderr should suggest --line: {stderr}"
    );
    assert!(
        stderr.contains("Math.cs:2") && stderr.contains("Math.cs:6"),
        "the candidate lines are what --line takes: {stderr}"
    );

    let first = flow_stdout(&bin, dir.path(), &["Calc.Add", "--line", "2"]);
    assert!(
        first.contains("entry Calc.Add(int a)") && first.contains("call One(…)"),
        "--line 2 should select the one-argument overload: {first}"
    );
    assert!(!first.contains("Two(…)"), "{first}");

    let second = flow_stdout(&bin, dir.path(), &["Calc.Add", "--line", "6"]);
    assert!(
        second.contains("entry Calc.Add(int a, int b)") && second.contains("call Two(…)"),
        "--line 6 should select the two-argument overload: {second}"
    );
    assert!(!second.contains("One(…)"), "{second}");

    // A line no definition starts on is an error, not a silent fallback.
    let output = flow_output(&bin, dir.path(), &["Calc.Add", "--line", "99"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("declared on line 99"),
        "stderr should name the line that matched nothing: {stderr}"
    );
}

// --- XAML data bindings (leg A, semantic mode only) ---

const MAUI_VIEWMODEL_CS: &str = r#"
namespace App {
    public abstract class BaseViewModel {
        public bool IsBusy { get; set; }
    }
    public class MainViewModel : BaseViewModel {
        public string Query { get; set; }
    }
}
"#;

const MAUI_PAGE_XAML: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<ContentPage xmlns="http://schemas.microsoft.com/dotnet/2021/maui"
             xmlns:x="http://schemas.microsoft.com/winfx/2009/xaml"
             xmlns:vm="clr-namespace:App"
             x:DataType="vm:MainViewModel">
    <Entry Text="{Binding Query}" />
    <ActivityIndicator IsRunning="{Binding IsBusy}" />
</ContentPage>
"#;

/// `helios init` indexes `.xaml` and the sidecar attributes its `{Binding}`
/// paths to the ViewModel members named by `x:DataType` — including `IsBusy`,
/// declared on the base class, which a name match against the markup alone
/// could not reach.
#[test]
fn test_e2e_xaml_bindings_resolve_to_viewmodel_members() {
    let Some(dll) = built_roslyn_dll() else {
        return;
    };
    let dir = tempfile::tempdir().expect("creating temp dir");
    std::fs::write(dir.path().join("MainViewModel.cs"), MAUI_VIEWMODEL_CS).unwrap();
    std::fs::write(dir.path().join("MainPage.xaml"), MAUI_PAGE_XAML).unwrap();

    init_with_sidecar(dir.path(), &dll, "roslyn");

    let conn = index_db(dir.path());
    let mut stmt = conn
        .prepare(
            "SELECT r.line, s.docid FROM references_ r
             JOIN symbols s ON s.id = r.symbol_id
             JOIN files f ON f.id = r.file_id
             WHERE f.path = 'MainPage.xaml' ORDER BY r.line",
        )
        .unwrap();
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    assert_eq!(
        rows,
        vec![
            (
                line_of(MAUI_PAGE_XAML, "{Binding Query}"),
                "P:App.MainViewModel.Query".to_string()
            ),
            (
                line_of(MAUI_PAGE_XAML, "{Binding IsBusy}"),
                "P:App.BaseViewModel.IsBusy".to_string()
            ),
        ]
    );
}

/// Without the helper the same repo still indexes: `.xaml` files are file rows
/// with no symbols and no bindings, and nothing errors.
#[test]
fn test_xaml_without_sidecar_indexes_without_bindings() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    std::fs::write(dir.path().join("MainViewModel.cs"), MAUI_VIEWMODEL_CS).unwrap();
    std::fs::write(dir.path().join("MainPage.xaml"), MAUI_PAGE_XAML).unwrap();

    let output = Command::new(helios_bin())
        .arg("init")
        .env("HELIOS_ROSLYN", "")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success());

    let conn = index_db(dir.path());
    let indexed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM files WHERE path = 'MainPage.xaml' AND language = 'xaml'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(indexed, 1, "the .xaml file is indexed either way");

    let bindings: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM references_ r JOIN files f ON f.id = r.file_id
             WHERE f.path = 'MainPage.xaml'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bindings, 0, "no parser, so no references from markup");
}

// --- Type relations (extends/implements) tests ---
//
// `type_relations` records inheritance/implements edges and surfaces them
// through `helios deps` as Supertypes/Implementors/Overrides sections. See
// src/db.rs (`type_relations`, `supertypes_of`, `implementors_of`,
// `overrides_of`, `TypeEdge`) and src/parsers/typescript.rs for the
// TypeScript leg exercised here.

/// `class Wallet extends Base implements Payable` records both edges: `deps
/// Base` lists Wallet as an implementor, and `deps Wallet` lists both
/// supertypes with the right kinds. Covers human output and `--json`.
#[test]
fn test_deps_reports_supertypes_and_implementors() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("wallet.ts"),
        "export class Base {}\nexport class Payable {}\nexport class Wallet extends Base implements Payable {}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    // deps Base -> Wallet shows up as an implementor.
    let output = Command::new(&bin)
        .args(["deps", "Base"])
        .current_dir(dir.path())
        .output()
        .expect("deps Base");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Implementors (what extends/implements Base):"),
        "expected an Implementors section, got: {stdout}"
    );
    assert!(
        stdout.contains("Wallet -> Base (extends)"),
        "expected Wallet -> Base (extends), got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["--json", "deps", "Base"])
        .current_dir(dir.path())
        .output()
        .expect("deps --json Base");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let implementors = value["implementors"].as_array().expect("implementors array");
    assert_eq!(
        implementors.len(),
        1,
        "expected one implementor, got: {stdout}"
    );
    assert_eq!(implementors[0]["sub_name"].as_str(), Some("Wallet"));
    assert_eq!(implementors[0]["kind"].as_str(), Some("extends"));
    assert_eq!(
        value["edge_languages"],
        serde_json::json!(["typescript"]),
        "got: {stdout}"
    );

    // deps Wallet -> both supertypes, right kinds.
    let output = Command::new(&bin)
        .args(["deps", "Wallet"])
        .current_dir(dir.path())
        .output()
        .expect("deps Wallet");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Supertypes (what Wallet extends/implements):"),
        "expected a Supertypes section, got: {stdout}"
    );
    assert!(
        stdout.contains("Wallet -> Base (extends)"),
        "expected Wallet -> Base (extends), got: {stdout}"
    );
    assert!(
        stdout.contains("Wallet -> Payable (implements)"),
        "expected Wallet -> Payable (implements), got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["--json", "deps", "Wallet"])
        .current_dir(dir.path())
        .output()
        .expect("deps --json Wallet");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let supertypes = value["supertypes"].as_array().expect("supertypes array");
    assert_eq!(supertypes.len(), 2, "expected two supertypes, got: {stdout}");
    let kinds: std::collections::BTreeSet<(String, String)> = supertypes
        .iter()
        .map(|e| {
            (
                e["super_name"].as_str().unwrap().to_string(),
                e["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        std::collections::BTreeSet::from([
            ("Base".to_string(), "extends".to_string()),
            ("Payable".to_string(), "implements".to_string()),
        ]),
        "got: {stdout}"
    );
}

/// An unresolvable supertype (`class Ext extends SomeExternalThing {}` where
/// nothing named SomeExternalThing is indexed) still gets a row: `super_name`
/// keeps the raw text, `super_symbol_id` stays NULL, and `deps Ext` marks the
/// edge `external`. This is the "external base types are not silently
/// dropped" requirement.
#[test]
fn test_unresolvable_supertype_recorded_and_marked_external() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("ext.ts"),
        "export class Ext extends SomeExternalThing {}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let conn = index_db(dir.path());
    let mut stmt = conn
        .prepare(
            "SELECT tr.super_name, tr.super_symbol_id, tr.kind FROM type_relations tr
             JOIN symbols s ON s.id = tr.sub_symbol_id
             WHERE s.name = 'Ext'",
        )
        .unwrap();
    let rows: Vec<(String, Option<i64>, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![("SomeExternalThing".to_string(), None, "extends".to_string())],
        "an unresolvable supertype must still get a row, with super_symbol_id NULL, got: {rows:?}"
    );

    let output = Command::new(&bin)
        .args(["deps", "Ext"])
        .current_dir(dir.path())
        .output()
        .expect("deps Ext");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Ext -> SomeExternalThing (extends, external)"),
        "expected the external marker on the unresolved edge, got: {stdout}"
    );
}

/// A type relation survives `helios update`: editing a class's heritage
/// clause and re-indexing removes the old edge and adds the new one, with no
/// duplicate rows left behind.
#[test]
fn test_type_relations_survive_incremental_update() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("payable.ts"),
        "export class Payable {}\nexport class Refundable {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("wallet.ts"),
        "import { Payable } from './payable';\nexport class Wallet implements Payable {}\n",
    )
    .unwrap();

    git_repo_with_commit(dir.path());

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let supers_of_wallet = |dir: &std::path::Path| -> Vec<String> {
        let conn = index_db(dir);
        let mut stmt = conn
            .prepare(
                "SELECT tr.super_name FROM type_relations tr
                 JOIN symbols s ON s.id = tr.sub_symbol_id
                 WHERE s.name = 'Wallet' ORDER BY tr.super_name",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    assert_eq!(
        supers_of_wallet(dir.path()),
        vec!["Payable".to_string()],
        "before the edit, Wallet implements Payable"
    );

    std::fs::write(
        dir.path().join("wallet.ts"),
        "import { Refundable } from './payable';\nexport class Wallet implements Refundable {}\n",
    )
    .unwrap();
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-qm", "swap heritage"]);

    update_stderr(dir.path());

    assert_eq!(
        supers_of_wallet(dir.path()),
        vec!["Refundable".to_string()],
        "after update, the old Payable edge must be gone and the new Refundable edge present, with no duplicate"
    );
}

/// `interface I extends K, L` produces two `extends` edges, not one.
#[test]
fn test_interface_extends_multiple_produces_two_edges() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("iface.ts"),
        "export interface K {}\nexport interface L {}\nexport interface I extends K, L {}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let conn = index_db(dir.path());
    let mut stmt = conn
        .prepare(
            "SELECT tr.super_name, tr.kind FROM type_relations tr
             JOIN symbols s ON s.id = tr.sub_symbol_id
             WHERE s.name = 'I' ORDER BY tr.super_name",
        )
        .unwrap();
    let rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("K".to_string(), "extends".to_string()),
            ("L".to_string(), "extends".to_string()),
        ],
        "expected two extends edges for `interface I extends K, L`, got: {rows:?}"
    );
}

/// A base class and a subclass both define the same method: `deps
/// <method> --scope <BaseClass>` reports the subclass's method as an
/// override.
#[test]
fn test_deps_reports_override_in_subclass() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("shapes.ts"),
        "export class Shape {\n    area(): number {\n        return 0;\n    }\n}\nexport class Circle extends Shape {\n    area(): number {\n        return 3;\n    }\n}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let output = Command::new(&bin)
        .args(["deps", "area", "--scope", "Shape"])
        .current_dir(dir.path())
        .output()
        .expect("deps area --scope Shape");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Overrides (what overrides area):"),
        "expected an Overrides section, got: {stdout}"
    );
    assert!(
        stdout.contains("Circle.area overrides Shape.area"),
        "expected Circle's area to be reported as overriding Shape's, got: {stdout}"
    );
}

/// `deps` names which languages contributed the type edges in its answer,
/// both in human output (`Type edges from: typescript`) and `--json`
/// (`edge_languages`) — the "output states which languages contributed"
/// acceptance requirement.
#[test]
fn test_deps_provenance_names_contributing_languages() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("wallet.ts"),
        "export class Base {}\nexport class Wallet extends Base {}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let output = Command::new(&bin)
        .args(["deps", "Wallet"])
        .current_dir(dir.path())
        .output()
        .expect("deps Wallet");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Type edges from: typescript"),
        "expected the provenance line naming typescript, got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["--json", "deps", "Wallet"])
        .current_dir(dir.path())
        .output()
        .expect("deps --json Wallet");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    assert_eq!(
        value["edge_languages"],
        serde_json::json!(["typescript"]),
        "expected edge_languages to match the provenance line, got: {stdout}"
    );
}

/// A class with no heritage clause produces zero `type_relations` rows.
#[test]
fn test_class_without_heritage_produces_no_type_relations() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("plain.ts"),
        "export class Plain {}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let conn = index_db(dir.path());
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM type_relations tr
             JOIN symbols s ON s.id = tr.sub_symbol_id
             WHERE s.name = 'Plain'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 0,
        "a class with no heritage clause must produce zero type_relations rows"
    );
}

/// `helios init` reuses an existing `.helios/index.db` and skips re-parsing
/// any file whose content hash is unchanged. That is right for a file that
/// really hasn't changed since it was last indexed — but wrong for an index
/// built before a feature (here: `type_relations`) existed, where "unchanged
/// content" doesn't mean "nothing new to extract". Simulating that upgrade
/// path — a stale `index_format_version` and an emptied `type_relations`,
/// with the source file left untouched — pins that a second `init` must
/// still re-parse and backfill the edges, not silently trust the hash.
#[test]
fn test_init_backfills_type_relations_after_format_upgrade() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    std::fs::write(
        dir.path().join("wallet.ts"),
        "export class Base {}\nexport class Wallet extends Base {}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let edge_count = || -> i64 {
        index_db(dir.path())
            .query_row(
                "SELECT COUNT(*) FROM type_relations tr
                 JOIN symbols s ON s.id = tr.sub_symbol_id
                 WHERE s.name = 'Wallet'",
                [],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(edge_count(), 1, "the first init must record Wallet's edge");

    // Simulate a pre-upgrade index: the edges a newer helios would have
    // populated are gone, and the format stamp names an older version — both
    // exactly what an index built before `type_relations` existed would look
    // like once opened by a helios binary that knows about it.
    {
        let conn = index_db(dir.path());
        conn.execute("DELETE FROM type_relations", [])
            .expect("clearing type_relations");
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES ('index_format_version', '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .expect("resetting index_format_version");
    }
    assert_eq!(edge_count(), 0, "setup must have emptied type_relations");

    // Re-run init with the source file untouched: its content hash still
    // matches, so without the format-version check this would short-circuit
    // and leave type_relations empty.
    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init (second run)");
    assert!(output.status.success(), "second helios init failed");

    assert_eq!(
        edge_count(),
        1,
        "an unchanged file must still be re-parsed to backfill type_relations \
         when the index predates the current format"
    );
    assert_eq!(
        metadata_value(dir.path(), "index_format_version").as_deref(),
        Some("4"),
        "a successful full index must stamp the current format version"
    );
}

/// A Rust `impl Trait for Type` whose `Type` is declared in a *different*
/// file must still produce an edge — not be silently dropped. `display.rs`
/// (the impl) sorts before `types.rs` (the struct + trait) in the walk, which
/// is deliberate: it pins the ordering trap `resolve_type_relations` exists
/// for, where the sub's own file hasn't been indexed yet when the impl is
/// parsed. See `resolve_type_relations` in src/indexer.rs and the
/// `sub_symbol_id`/`sub_name` columns in src/db.rs.
#[test]
fn test_cross_file_impl_resolves_sub_via_second_pass() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();

    // Sorts before types.rs, so the impl is parsed before Widget exists in
    // the index — sub_symbol_id must come back NULL at insert time and only
    // get filled in by the resolve_type_relations second pass.
    std::fs::write(
        dir.path().join("display.rs"),
        "use crate::types::{Render, Widget};\n\nimpl Render for Widget {\n    fn render(&self) -> String {\n        String::new()\n    }\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("types.rs"),
        "pub struct Widget {\n    pub id: u32,\n}\n\npub trait Render {\n    fn render(&self) -> String;\n}\n",
    )
    .unwrap();

    let output = Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    assert!(output.status.success(), "helios init failed");

    let conn = index_db(dir.path());
    let mut stmt = conn
        .prepare(
            "SELECT s.name, s.file_id = (SELECT id FROM files WHERE path = 'display.rs')
             FROM type_relations tr
             JOIN symbols s ON s.id = tr.sub_symbol_id
             WHERE tr.super_name = 'Render'",
        )
        .unwrap();
    let rows: Vec<(String, bool)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![("Widget".to_string(), false)],
        "expected the cross-file impl to resolve to Widget's own symbol row \
         (declared in types.rs, not display.rs), got: {rows:?}"
    );

    let output = Command::new(&bin)
        .args(["deps", "Render"])
        .current_dir(dir.path())
        .output()
        .expect("deps Render");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Implementors (what extends/implements Render):"),
        "expected an Implementors section, got: {stdout}"
    );
    assert!(
        stdout.contains("Widget -> Render (implements)"),
        "expected Widget -> Render (implements), got: {stdout}"
    );
    assert!(
        !stdout.contains("No dependencies found"),
        "the cross-file edge must not be silently dropped, got: {stdout}"
    );
}

// --------------------------------------------------------------------------
// Task 914: `deps` gained symbol-level transitive traversal (`--depth` on a
// symbol target, not just a file target), a call-path query (`--to`), and
// dynamic-dispatch traversal (`--follow-impls`). See src/commands/deps.rs
// (`bfs_call_graph`, `run_to_query`, `call_steps`) and src/db.rs
// (`callees_of`, `callers_of`, `override_impl_ids`) for the implementation
// exercised below.

/// A 3-deep call chain (`a` -> `b` -> `c`), plus one unrelated function with
/// no edges to or from the chain — shared by several tests below that need a
/// deterministic call graph with known line numbers.
fn write_call_chain_fixture(dir: &std::path::Path) {
    std::fs::write(
        dir.join("chain.rs"),
        "pub fn a() {\n    b();\n}\n\npub fn b() {\n    c();\n}\n\npub fn c() {\n    println!(\"c\");\n}\n\npub fn unrelated() {\n    println!(\"unrelated\");\n}\n",
    )
    .unwrap();
}

/// Acceptance criterion 1: a symbol target now accepts `--depth`, but only
/// additively — `--depth 1` (the pre-existing default) must still produce
/// exactly today's JSON, with no `calls`/`calls_truncated`/`callers`/
/// `callers_truncated` keys at all. This is the regression guard called out
/// in the task: depth-1 symbol JSON is byte-identical to before the feature.
#[test]
fn test_deps_depth1_symbol_json_has_no_new_keys() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    write_call_chain_fixture(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    // Depth defaults to 1 when omitted, so both spellings must agree.
    for args in [vec!["--json", "deps", "a"], vec!["--json", "deps", "a", "--depth", "1"]] {
        let output = Command::new(&bin)
            .args(&args)
            .current_dir(dir.path())
            .output()
            .expect("deps a");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
        let keys: std::collections::BTreeSet<&str> =
            value.as_object().unwrap().keys().map(String::as_str).collect();
        let expected: std::collections::BTreeSet<&str> = [
            "target",
            "definitions",
            "supertypes",
            "implementors",
            "overrides",
            "edge_languages",
            "dependencies",
            "dependents",
        ]
        .into_iter()
        .collect();
        assert_eq!(
            keys, expected,
            "depth-1 symbol JSON must have exactly the pre-existing keys, got: {stdout}"
        );
    }
}

/// Acceptance criterion 1: `--depth N > 1` on a symbol target walks the call
/// graph transitively (a -> b -> c), reporting each hop's depth. `--depth 1`
/// must not surface `c` at all — it is two hops away from `a` — proving the
/// new traversal is genuinely gated on `depth > 1`, not always-on.
#[test]
fn test_deps_symbol_depth_walks_transitive_calls() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    write_call_chain_fixture(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    // --depth 3 reaches c at depth 2 (a -> b at depth 1 -> c at depth 2).
    let output = Command::new(&bin)
        .args(["--json", "deps", "a", "--depth", "3"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --depth 3");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let calls = value["calls"].as_array().expect("calls array");
    let names_and_depths: Vec<(&str, u64)> = calls
        .iter()
        .map(|c| (c["name"].as_str().unwrap(), c["depth"].as_u64().unwrap()))
        .collect();
    assert!(
        names_and_depths.contains(&("b", 1)),
        "expected b at depth 1, got {names_and_depths:?}"
    );
    assert!(
        names_and_depths.contains(&("c", 2)),
        "expected c at depth 2, got {names_and_depths:?}"
    );
    assert_eq!(
        value["calls_truncated"],
        serde_json::json!(false),
        "the whole chain fit inside depth 3, got: {stdout}"
    );

    // Human output: same depth-indented shape.
    let output = Command::new(&bin)
        .args(["deps", "a", "--depth", "3"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --depth 3 (human)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Calls (what a reaches, transitively):"),
        "expected a Calls section, got: {stdout}"
    );
    assert!(
        stdout.contains("b (depth 1)"),
        "expected b at depth 1, got: {stdout}"
    );
    assert!(
        stdout.contains("c (depth 2)"),
        "expected c at depth 2, got: {stdout}"
    );

    // --depth 1 must NOT reach c — depth-1 output is unchanged from before
    // this feature, so a two-hop-away symbol has no way to appear.
    let output = Command::new(&bin)
        .args(["deps", "a", "--depth", "1"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --depth 1");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("Calls ("),
        "depth 1 must not print a Calls section at all, got: {stdout}"
    );
    assert!(
        !stdout.contains(" c "),
        "depth 1 must not reach c, got: {stdout}"
    );
}

/// Acceptance criterion 2: `--to` returns the actual call chain, not a
/// boolean. Each hop names both the callee's own definition site and the
/// call site that produced the edge.
#[test]
fn test_deps_to_returns_full_call_chain() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    write_call_chain_fixture(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    let output = Command::new(&bin)
        .args(["--json", "deps", "a", "--to", "c"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --to c");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    assert_eq!(value["found"], serde_json::json!(true), "got: {stdout}");
    let path = value["path"].as_array().expect("path array");
    assert_eq!(path.len(), 2, "a -> b -> c is a 2-hop path, got: {stdout}");
    assert_eq!(path[0]["name"].as_str(), Some("b"));
    assert_eq!(path[0]["depth"].as_u64(), Some(1));
    assert_eq!(path[1]["name"].as_str(), Some("c"));
    assert_eq!(path[1]["depth"].as_u64(), Some(2));
    // Each hop names the call site that produced the edge, not just the
    // callee's own definition.
    assert!(path[0]["call_site"].is_object(), "got: {stdout}");
    assert!(path[1]["call_site"].is_object(), "got: {stdout}");
    assert_eq!(path[0]["inferred"], serde_json::json!(false));

    let output = Command::new(&bin)
        .args(["deps", "a", "--to", "c"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --to c (human)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Path from a to c (2 calls, depth limit"),
        "expected a path header, got: {stdout}"
    );
    assert!(
        stdout.contains("-> chain.rs:5 b (call at chain.rs:2)"),
        "expected b's hop naming both its definition and the call site, got: {stdout}"
    );
    assert!(
        stdout.contains("-> chain.rs:9 c (call at chain.rs:6)"),
        "expected c's hop naming both its definition and the call site, got: {stdout}"
    );
}

/// Acceptance criterion 3: cycles terminate. A mutual-recursion pair
/// (`ping` <-> `pong`) and a self-recursive function (`loopy`) must both
/// finish `--depth 5` (and a `--to` query against an unreachable symbol) in
/// bounded time, without the same symbol appearing twice in the output — the
/// `visited` set is what makes this true, not luck about the depth chosen.
#[test]
fn test_deps_call_graph_cycles_terminate() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::write(
        dir.path().join("cycle.rs"),
        "pub fn ping() {\n    pong();\n}\n\npub fn pong() {\n    ping();\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("selfrec.rs"),
        "pub fn loopy() {\n    loopy();\n}\n",
    )
    .unwrap();

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    // Mutual recursion: `pong` must appear exactly once, not once per loop
    // iteration around the cycle.
    let output = Command::new(&bin)
        .args(["--json", "deps", "ping", "--depth", "5"])
        .current_dir(dir.path())
        .output()
        .expect("deps ping --depth 5");
    assert!(output.status.success(), "must terminate and exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let calls = value["calls"].as_array().expect("calls array");
    let pong_count = calls.iter().filter(|c| c["name"] == "pong").count();
    assert_eq!(pong_count, 1, "pong must not be duplicated, got: {stdout}");
    let ping_in_calls = calls.iter().any(|c| c["name"] == "ping");
    assert!(
        !ping_in_calls,
        "ping is a starting symbol, not a discovered hop, got: {stdout}"
    );

    // Self-recursion: loopy calling itself must not create any hop at all —
    // it is pre-seeded into `visited` as a starting symbol.
    let output = Command::new(&bin)
        .args(["--json", "deps", "loopy", "--depth", "5"])
        .current_dir(dir.path())
        .output()
        .expect("deps loopy --depth 5");
    assert!(output.status.success(), "must terminate and exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    assert_eq!(
        value["calls"].as_array().unwrap().len(),
        0,
        "a pure self-call must not surface as a discovered hop, got: {stdout}"
    );

    // A `--to` query against an unreachable symbol over a cyclic graph must
    // also terminate rather than looping forever chasing the cycle.
    let output = Command::new(&bin)
        .args(["--json", "deps", "ping", "--to", "loopy", "--depth", "5"])
        .current_dir(dir.path())
        .output()
        .expect("deps ping --to loopy");
    assert!(output.status.success(), "must terminate and exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    assert_eq!(value["found"], serde_json::json!(false), "got: {stdout}");
}

/// Acceptance criterion 4 (the most important one): a bounded search must
/// never read as a complete one. Two symbols separated by more hops than
/// `--depth` allows ("truncated: true") must be reported differently — in
/// both JSON and human text — from two symbols with no connection at all
/// ("truncated: false"). Conflating the two would let a search that simply
/// gave up masquerade as proof no path exists.
#[test]
fn test_deps_to_distinguishes_truncated_from_no_path() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    write_call_chain_fixture(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    // c is 2 hops from a; --depth 1 cuts the search off before reaching it.
    let output = Command::new(&bin)
        .args(["--json", "deps", "a", "--to", "c", "--depth", "1"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --to c --depth 1");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    assert_eq!(value["found"], serde_json::json!(false), "got: {stdout}");
    assert_eq!(
        value["truncated"],
        serde_json::json!(true),
        "the depth limit, not exhaustion, is why no path was found, got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["deps", "a", "--to", "c", "--depth", "1"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --to c --depth 1 (human)");
    let truncated_message = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        truncated_message.contains("cut off at depth 1"),
        "got: {truncated_message}"
    );
    assert!(
        truncated_message.contains("a longer path may exist"),
        "got: {truncated_message}"
    );

    // `unrelated` has no static edge to or from a/b/c at all — the depth
    // limit (10, well above the chain's own length) is never the reason.
    let output = Command::new(&bin)
        .args(["--json", "deps", "a", "--to", "unrelated", "--depth", "10"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --to unrelated");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    assert_eq!(value["found"], serde_json::json!(false), "got: {stdout}");
    assert_eq!(
        value["truncated"],
        serde_json::json!(false),
        "the reachable set was exhausted, not cut off, got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["deps", "a", "--to", "unrelated", "--depth", "10"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --to unrelated (human)");
    let no_path_message = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        no_path_message.contains("was searched") && no_path_message.contains("no static call path exists"),
        "got: {no_path_message}"
    );
    assert!(
        !no_path_message.contains("cut off"),
        "an exhausted search must not use the truncated search's wording, got: {no_path_message}"
    );

    // The two "not found" explanations must be genuinely different strings
    // — this is the machine-readable criterion 4 made human-readable.
    assert_ne!(
        truncated_message, no_path_message,
        "truncated and exhausted must read differently, not just differ in a shared template"
    );
}

/// Acceptance criterion 4, transitive-traversal form: `--depth N` on a plain
/// symbol target (no `--to`) must also disclose its own truncation via
/// `calls_truncated`/`callers_truncated`, the same honesty requirement as
/// the `--to` path query above.
#[test]
fn test_deps_transitive_calls_reports_own_truncation() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    write_call_chain_fixture(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    // Depth 2 reaches c but never gets to expand it (c sits exactly at the
    // depth limit), so the walk must report itself as truncated.
    let output = Command::new(&bin)
        .args(["--json", "deps", "a", "--depth", "2"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --depth 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    assert_eq!(
        value["calls_truncated"],
        serde_json::json!(true),
        "got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["deps", "a", "--depth", "2"])
        .current_dir(dir.path())
        .output()
        .expect("deps a --depth 2 (human)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Calls truncated at depth 2:") && stdout.contains("still unexplored"),
        "expected a truncation disclosure line, got: {stdout}"
    );
}

/// Acceptance criterion 5: with `--follow-impls`, an edge reached only
/// through dynamic dispatch (calling a base member whose subtype overrides
/// it) is marked `inferred` — in both JSON and human output — and without
/// the flag no such edge is produced at all. `Circle` overrides `Shape`'s
/// `area`, so `deps area --scope Shape --follow-impls` must reach
/// `Circle.area` as an inferred hop that `deps area --scope Shape` (no flag)
/// does not.
#[test]
fn test_deps_follow_impls_marks_inferred_edges() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    std::fs::write(
        dir.path().join("shapes.ts"),
        "export class Shape {\n    area(): number {\n        return 0;\n    }\n}\nexport class Circle extends Shape {\n    area(): number {\n        return 3;\n    }\n}\n",
    )
    .unwrap();

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");

    // Without --follow-impls: no inferred edges at all, even at a depth that
    // would otherwise be deep enough to reach the override.
    let output = Command::new(&bin)
        .args(["--json", "deps", "area", "--scope", "Shape", "--depth", "2"])
        .current_dir(dir.path())
        .output()
        .expect("deps area --scope Shape --depth 2");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let calls = value["calls"].as_array().expect("calls array");
    assert!(
        calls.iter().all(|c| c["inferred"] == serde_json::json!(false)),
        "no edge should be marked inferred without --follow-impls, got: {stdout}"
    );
    assert!(
        !calls.iter().any(|c| c["scope"] == serde_json::json!("Circle")),
        "Circle.area must not be reached without --follow-impls, got: {stdout}"
    );

    // With --follow-impls: Circle.area is reached, marked inferred, with its
    // relation kind ("extends") as edge_kind.
    let output = Command::new(&bin)
        .args([
            "--json", "deps", "area", "--scope", "Shape", "--depth", "2", "--follow-impls",
        ])
        .current_dir(dir.path())
        .output()
        .expect("deps area --scope Shape --depth 2 --follow-impls");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let calls = value["calls"].as_array().expect("calls array");
    let circle_area = calls
        .iter()
        .find(|c| c["scope"] == serde_json::json!("Circle"))
        .unwrap_or_else(|| panic!("expected Circle.area reached via --follow-impls, got: {stdout}"));
    assert_eq!(circle_area["inferred"], serde_json::json!(true));
    assert_eq!(circle_area["edge_kind"], serde_json::json!("extends"));

    let output = Command::new(&bin)
        .args([
            "deps", "area", "--scope", "Shape", "--depth", "2", "--follow-impls",
        ])
        .current_dir(dir.path())
        .output()
        .expect("deps area --scope Shape --depth 2 --follow-impls (human)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Circle.area (depth 1) [inferred: extends]"),
        "expected the inferred hop to be visually marked, got: {stdout}"
    );
}

// --------------------------------------------------------------------------
// Regression: `--follow-impls` in the Callers direction was applying the
// same "widen to subtypes" rule the Callees direction correctly uses —
// `override_impl_ids(name, scope)` always finds subtypes of `scope`, which
// is right for "what might this call dispatch to" but wrong for "what calls
// this": a sibling override is not a caller of the base member it overrides.
// The fix branches `call_steps` on direction: Callers now bridges to the
// *supertype*'s member (`Database::supertype_member_ids`) instead, so a
// caller of the base member's real callers surface one hop further out, and
// the base's own callers listing never gets polluted with unrelated
// overrides. See src/commands/deps.rs `call_steps` (~182-247) and
// `inferred_label` (~357-376).
//
// The fixture below writes real TypeScript (Shape, and Circle/Square both
// `extends Shape` overriding `foo`) so `type_relations` and the three
// `foo` symbols come from the real parser — but does NOT write any call to
// `.foo()` in source. A textual `.foo()` call would itself be an ambiguous
// name and fan out to a reference row per candidate definition (Shape's,
// Circle's, and Square's `foo` all at once, a documented, unrelated
// behavior — see `bfs_call_graph`'s doc comment), which would make it
// impossible to tell "reached via the real edge" apart from "reached via
// the inferred bridge" in these assertions. Instead, exactly one
// `references_` row is inserted directly (`run` calling `Shape.foo`,
// following the same seed-the-database convention
// `test_deps_type_edge_external_cross_language_shows_declaring_file` uses),
// so the only way from `Circle.foo` to `run` is through the bridge under
// test.
fn write_dispatch_fixture(dir: &std::path::Path) {
    std::fs::write(
        dir.join("shapes3.ts"),
        "export class Shape {\n    foo(): number {\n        return 0;\n    }\n}\nexport class Circle extends Shape {\n    foo(): number {\n        return 1;\n    }\n}\nexport class Square extends Shape {\n    foo(): number {\n        return 2;\n    }\n}\nexport function run(): number {\n    return 0;\n}\n",
    )
    .unwrap();
}

/// Inserts one `references_` row recording `run` as a real (non-inferred)
/// caller of `Shape.foo` — the only edge into the base member that exists
/// anywhere in this fixture's graph.
fn seed_run_calls_shape_foo(conn: &rusqlite::Connection) {
    let file_id: i64 = conn
        .query_row("SELECT id FROM files WHERE path = 'shapes3.ts'", [], |row| row.get(0))
        .expect("finding shapes3.ts file row");
    let shape_foo_id: i64 = conn
        .query_row(
            "SELECT id FROM symbols WHERE name = 'foo' AND scope = 'Shape'",
            [],
            |row| row.get(0),
        )
        .expect("finding Shape.foo symbol row");
    let run_id: i64 = conn
        .query_row("SELECT id FROM symbols WHERE name = 'run'", [], |row| row.get(0))
        .expect("finding run symbol row");
    conn.execute(
        "INSERT INTO references_ (symbol_id, file_id, line, column, qualified, container_symbol_id, usage_kind)
         VALUES (?1, ?2, 17, 4, 0, ?3, 'read')",
        rusqlite::params![shape_foo_id, file_id, run_id],
    )
    .expect("seeding run -> Shape.foo reference");
}

/// Direct regression guard for the bug: querying the BASE member's callers
/// with `--follow-impls` must not surface either subtype's override —
/// before the fix, `Circle.foo` and `Square.foo` would both appear here,
/// mislabeled as callers of `Shape.foo`. Also covers "walking Callers from
/// the base adds no inferred edges at all": the only entry present is the
/// one real caller, and nothing here is `inferred: true`.
#[test]
fn test_deps_follow_impls_callers_from_base_excludes_sibling_overrides() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    write_dispatch_fixture(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    {
        let conn = rusqlite::Connection::open(dir.path().join(".helios").join("index.db"))
            .expect("opening index.db");
        seed_run_calls_shape_foo(&conn);
    }

    let output = Command::new(&bin)
        .args(["--json", "deps", "foo", "--scope", "Shape", "--depth", "2", "--follow-impls"])
        .current_dir(dir.path())
        .output()
        .expect("deps foo --scope Shape --depth 2 --follow-impls");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let callers = value["callers"].as_array().expect("callers array");

    assert!(
        !callers.iter().any(|c| c["scope"] == serde_json::json!("Circle")),
        "regression: Circle.foo (a sibling override) must not be reported as a caller of the base member, got: {stdout}"
    );
    assert!(
        !callers.iter().any(|c| c["scope"] == serde_json::json!("Square")),
        "regression: Square.foo (a sibling override) must not be reported as a caller of the base member, got: {stdout}"
    );
    assert!(
        callers.iter().all(|c| c["inferred"] == serde_json::json!(false)),
        "the base member has no supertype of its own, so --follow-impls must add nothing here, got: {stdout}"
    );
    let names: Vec<&str> = callers.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(
        names, vec!["run"],
        "the only caller present must be the one genuine edge, got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["deps", "foo", "--scope", "Shape", "--depth", "2", "--follow-impls"])
        .current_dir(dir.path())
        .output()
        .expect("deps foo --scope Shape --depth 2 --follow-impls (human)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Scoped to the Callers section specifically: Circle/Square legitimately
    // appear elsewhere in this output (the Overrides section, and the Calls
    // section's own --follow-impls dispatch, which is the correct, unrelated
    // Callees-direction behaviour covered by
    // `test_deps_follow_impls_marks_inferred_edges`).
    let callers_section = stdout
        .split("Callers (what reaches foo, transitively):")
        .nth(1)
        .unwrap_or_else(|| panic!("expected a Callers section, got: {stdout}"));
    assert!(
        !callers_section.contains("Circle") && !callers_section.contains("Square"),
        "regression: neither override's name should appear in the base's Callers section, got: {stdout}"
    );
    assert!(
        callers_section.contains("run (depth 1)"),
        "expected the one genuine caller edge, got: {stdout}"
    );
}

/// The fixed behaviour: from an override (`Circle.foo`), `--follow-impls`
/// bridges inward to the base member (`Shape.foo`, marked `inferred: true`)
/// at depth 1, and the base member's own genuine caller (`run`) surfaces
/// one hop further out, at depth 2 — reachable only through the bridge,
/// since `run` has no edge to `Circle.foo` in this fixture. Also asserts
/// the human label is the new wording ("callers of ... may dispatch here"),
/// not the old bare `[inferred: <kind>]` form the Callees direction still
/// uses (see `test_deps_follow_impls_marks_inferred_edges`), since that
/// wording would misdescribe the relationship in this direction.
#[test]
fn test_deps_follow_impls_callers_bridges_override_to_base_caller() {
    let dir = tempfile::tempdir().expect("creating temp dir");
    let bin = helios_bin();
    write_dispatch_fixture(dir.path());

    Command::new(&bin)
        .arg("init")
        .current_dir(dir.path())
        .output()
        .expect("helios init");
    {
        let conn = rusqlite::Connection::open(dir.path().join(".helios").join("index.db"))
            .expect("opening index.db");
        seed_run_calls_shape_foo(&conn);
    }

    let output = Command::new(&bin)
        .args(["--json", "deps", "foo", "--scope", "Circle", "--depth", "2", "--follow-impls"])
        .current_dir(dir.path())
        .output()
        .expect("deps foo --scope Circle --depth 2 --follow-impls");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("parsing deps JSON");
    let callers = value["callers"].as_array().expect("callers array");

    let bridge = callers
        .iter()
        .find(|c| c["name"] == "foo" && c["scope"] == serde_json::json!("Shape"))
        .unwrap_or_else(|| panic!("expected Shape.foo reached as an inferred bridge, got: {stdout}"));
    assert_eq!(bridge["inferred"], serde_json::json!(true), "got: {stdout}");
    assert_eq!(bridge["depth"], serde_json::json!(1), "got: {stdout}");

    let real_caller = callers
        .iter()
        .find(|c| c["name"] == "run")
        .unwrap_or_else(|| panic!("expected run to surface via the bridge, got: {stdout}"));
    assert_eq!(
        real_caller["inferred"],
        serde_json::json!(false),
        "run is a genuine caller of the base member, not itself inferred, got: {stdout}"
    );
    assert_eq!(
        real_caller["depth"],
        serde_json::json!(2),
        "run should surface one hop past the bridge, not directly at depth 1, got: {stdout}"
    );

    let output = Command::new(&bin)
        .args(["deps", "foo", "--scope", "Circle", "--depth", "2", "--follow-impls"])
        .current_dir(dir.path())
        .output()
        .expect("deps foo --scope Circle --depth 2 --follow-impls (human)");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("[inferred: callers of Shape.foo may dispatch here]"),
        "expected the new inward-direction wording, got: {stdout}"
    );
    assert!(
        !stdout.contains("[inferred: extends]") && !stdout.contains("[inferred: implements]"),
        "a Callers-direction inferred edge must not use the bare Callees-direction label, got: {stdout}"
    );
    assert!(
        stdout.contains("run (depth 2)"),
        "expected run's real caller edge one hop past the bridge, got: {stdout}"
    );
}
