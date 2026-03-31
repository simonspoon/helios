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
    // Use cargo to find the binary
    let output = Command::new("cargo")
        .args(["build", "--quiet"])
        .output()
        .expect("cargo build failed");
    assert!(output.status.success(), "cargo build failed");

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("target");
    path.push("debug");
    path.push("helios");
    path
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

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No index found"),
        "expected no index message, got: {}",
        stdout
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
