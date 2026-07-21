//! End-to-end: spawn the `peashooter-provider-rust` sidecar and drive it over the protobuf wire.
use ci_core::{CommitResult, EditOp, EditOpts, Granularity, LanguageProvider};
use ci_proto::ProcessProvider;
use std::path::{Path, PathBuf};
use std::process::Command;

fn sidecar(root: &Path) -> ProcessProvider {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_peashooter-provider-rust"));
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

// Full write path over the wire — the rust-analyzer gate runs INSIDE the sidecar. #[ignore]
// (needs `rustup component add rust-analyzer`); run with `cargo test -p lang-rust -- --ignored`.
#[test]
#[ignore]
fn sidecar_apply_edits_over_wire() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"t\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\npub fn run() -> i32 {\n    add(1, 2)\n}\n",
    )
    .unwrap();

    let p = sidecar(root);
    let opts = EditOpts { write: true, dry_run: false, tsconfig: None };
    let res = p
        .apply_edits(&[EditOp::Rename { node_id: "src/lib.rs#add".into(), new_name: "sum".into() }], &opts)
        .unwrap();
    assert!(matches!(res, CommitResult::Ok { .. }), "rename over the wire should commit: {res:?}");
    let after = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(after.contains("pub fn sum"), "definition renamed via sidecar: {after}");
    assert!(after.contains("sum(1, 2)"), "call site renamed by the in-sidecar gate: {after}");
}
