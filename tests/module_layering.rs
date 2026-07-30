//! The module graph stays acyclic.
//!
//! Workspace crates would get this from cargo, which refuses to build a
//! dependency cycle. myco is one crate, so `mod` boundaries carry no such
//! guarantee — `session` reaching into `harness` compiles exactly as well as
//! the other way round. This test is the substitute: the layering AGENTS.md
//! documents is checked, not just described.
//!
//! It reads `src/` as text rather than parsing Rust, which sets its limits:
//! a `crate::x` inside a string literal counts, and a `#[cfg(test)]` on
//! anything other than the trailing test module cuts the file short. Both fail
//! toward silence, never toward a false alarm — the test can miss an edge, but
//! anything it reports is really written in the source.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Modules whose contents are entirely test scaffolding, so their edges say
/// nothing about how the shipped code is layered.
const TEST_ONLY_MODULES: &[&str] = &["test_support"];

fn src_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Top-level modules: `src/foo/` and `src/foo.rs`, minus `lib.rs` and `bin/`.
fn modules() -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(src_root()).expect("read src/") {
        let entry = entry.expect("dir entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().expect("file type").is_dir();
        let module = match (is_dir, name.strip_suffix(".rs")) {
            (true, _) if name != "bin" => name,
            (false, Some(stem)) if stem != "lib" => stem.to_string(),
            _ => continue,
        };
        if !TEST_ONLY_MODULES.contains(&module.as_str()) {
            out.push(module);
        }
    }
    out.sort();
    out
}

fn rust_files(module: &str) -> Vec<PathBuf> {
    let dir = src_root().join(module);
    if !dir.is_dir() {
        return vec![src_root().join(format!("{module}.rs"))];
    }
    let mut out = Vec::new();
    let mut stack = vec![dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read module dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Files pulled in as `#[cfg(test)] mod name;` — test-only in their entirety,
/// so nothing in them counts as a production edge.
fn whole_file_test_modules() -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for module in modules() {
        for file in rust_files(&module) {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let lines: Vec<_> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if line.trim() != "#[cfg(test)]" {
                    continue;
                }
                let Some(next) = lines.get(i + 1) else {
                    continue;
                };
                let Some(name) = next
                    .trim()
                    .strip_prefix("mod ")
                    .and_then(|rest| rest.strip_suffix(';'))
                else {
                    continue;
                };
                let dir = file.parent().expect("file has a parent");
                for candidate in [
                    dir.join(format!("{name}.rs")),
                    dir.join(name).join("mod.rs"),
                ] {
                    if candidate.is_file() {
                        out.insert(candidate);
                    }
                }
            }
        }
    }
    out
}

/// Which modules each module names in shipped code. Everything from the first
/// `#[cfg(test)]` to the end of a file is dropped: test setup composes real
/// layers on purpose and is exempt by design.
fn production_graph() -> BTreeMap<String, BTreeSet<String>> {
    let known: BTreeSet<String> = modules().into_iter().collect();
    let test_files = whole_file_test_modules();
    let mut graph = BTreeMap::new();

    for module in modules() {
        let mut deps = BTreeSet::new();
        for file in rust_files(&module) {
            if test_files.contains(&file) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            for line in text.lines() {
                let trimmed = line.trim_start();
                if trimmed.starts_with("//") || trimmed.starts_with("#!") {
                    continue;
                }
                if trimmed.starts_with("#[cfg(test)]") {
                    break;
                }
                for dep in referenced_modules(line) {
                    if dep != module && known.contains(&dep) {
                        deps.insert(dep);
                    }
                }
            }
        }
        graph.insert(module, deps);
    }
    graph
}

/// Every `crate::<ident>` on one line.
fn referenced_modules(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (idx, _) in line.match_indices("crate::") {
        let rest = &line[idx + "crate::".len()..];
        let ident: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if !ident.is_empty() {
            out.push(ident);
        }
    }
    out
}

/// Depth-first search for a cycle, returning the path around it.
fn find_cycle(graph: &BTreeMap<String, BTreeSet<String>>) -> Option<Vec<String>> {
    fn walk(
        node: &str,
        graph: &BTreeMap<String, BTreeSet<String>>,
        path: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Option<Vec<String>> {
        if let Some(at) = path.iter().position(|n| n == node) {
            let mut cycle = path[at..].to_vec();
            cycle.push(node.to_string());
            return Some(cycle);
        }
        if done.contains(node) {
            return None;
        }
        path.push(node.to_string());
        for dep in graph.get(node).into_iter().flatten() {
            if let Some(cycle) = walk(dep, graph, path, done) {
                return Some(cycle);
            }
        }
        path.pop();
        done.insert(node.to_string());
        None
    }

    let mut done = BTreeSet::new();
    for node in graph.keys() {
        if let Some(cycle) = walk(node, graph, &mut Vec::new(), &mut done) {
            return Some(cycle);
        }
    }
    None
}

#[test]
fn the_module_graph_is_acyclic() {
    let graph = production_graph();
    if let Some(cycle) = find_cycle(&graph) {
        let rendered: Vec<_> = graph
            .iter()
            .map(|(m, d)| {
                format!(
                    "  {m} -> {}",
                    d.iter().cloned().collect::<Vec<_>>().join(" ")
                )
            })
            .collect();
        panic!(
            "module dependency cycle: {}\n\nwhole graph:\n{}\n\nThe fix is usually that the \
             shared thing belongs lower down, or that the caller wants data instead of \
             rendering. See the layering invariant in AGENTS.md.",
            cycle.join(" -> "),
            rendered.join("\n"),
        );
    }
}

#[test]
fn core_is_the_bottom_layer() {
    let deps = production_graph()
        .remove("core")
        .expect("core module exists");
    assert!(
        deps.is_empty(),
        "core must depend on no other module, but reaches: {deps:?}. Anything core needs is \
         either a third-party crate or belongs in core itself."
    );
}

/// The scanner has to actually see edges, or both tests above pass vacuously.
#[test]
fn the_scanner_finds_the_edges_that_exist() {
    let graph = production_graph();
    assert!(
        graph.len() > 5,
        "expected the src/ modules, found {:?}",
        graph.keys().collect::<Vec<_>>()
    );
    assert!(
        graph["session"].contains("core"),
        "session depends on core in the source; scanner reported {:?}",
        graph["session"],
    );
    assert!(
        !graph["session"].contains("harness"),
        "session must not reach the tool runtime; reported {:?}",
        graph["session"],
    );
}
