//! End-to-end: spawn the `marksman-provider-rust` sidecar and drive it over the protobuf wire.
use ci_core::{Granularity, LanguageProvider};
use ci_proto::ProcessProvider;
use std::path::{Path, PathBuf};
use std::process::Command;

fn sidecar(root: &Path) -> ProcessProvider {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_marksman-provider-rust"));
    cmd.arg("--root").arg(root);
    ProcessProvider::spawn(cmd).expect("spawn sidecar")
}

#[test]
fn sidecar_round_trips_read_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("lib.rs"), "mod a;\n").unwrap();
    std::fs::write(
        root.join("a.rs"),
        "/// Adds two ints.\npub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    )
    .unwrap();

    let p = sidecar(root);

    // granularity travels the wire
    assert!(matches!(p.granularity(), Granularity::Ast));

    // structure: the recursive Node tree (symbol + :doc/:param/:return/:body sub-nodes) survives
    let nodes = p.structure(Path::new("a.rs")).unwrap();
    let add = nodes.iter().find(|n| n.id == "a.rs#add").expect("add via sidecar");
    let child_ids: Vec<&str> = add.children.iter().map(|c| c.id.as_str()).collect();
    assert!(child_ids.contains(&"a.rs#add:doc"), "doc sub-node survived: {child_ids:?}");
    assert!(child_ids.contains(&"a.rs#add:body"), "body sub-node survived: {child_ids:?}");
    assert!(child_ids.contains(&"a.rs#add:return"), "return sub-node survived: {child_ids:?}");

    // import graph: lib.rs -> a.rs
    let g = p.import_graph().unwrap();
    let edges = g.get(&PathBuf::from("lib.rs")).expect("lib.rs edges via sidecar");
    assert!(edges.contains(&PathBuf::from("a.rs")), "mod edge survived: {edges:?}");

    // outline over the wire
    let out = p.outline("pub fn f() {\n    let x = 1;\n    x\n}\n").unwrap();
    assert!(out.contains("{ /* … */ }"), "outline folded body over the wire: {out}");
}
