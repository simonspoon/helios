use std::path::PathBuf;
use std::process::Command;

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

    // Should index all 4 files (rs, py, go, ts)
    assert!(
        json["files_indexed"].as_u64().unwrap() >= 4,
        "Should index at least 4 files, got: {}",
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
}
