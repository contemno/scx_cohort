// Copyright (c) scx_cohort authors.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2.

use std::path::PathBuf;

fn main() {
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let common_dir = crate_dir.parent().unwrap().join("scx_cohort_common");

    // Rust is the source of truth for shared types: generate the C header
    // the BPF component includes. Written next to main.bpf.c (gitignored)
    // so its relative #include resolves without custom cflags.
    let config = cbindgen::Config::from_file(common_dir.join("cbindgen.toml"))
        .expect("read cbindgen.toml");
    cbindgen::Builder::new()
        .with_crate(&common_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen generation failed")
        .write_to_file(crate_dir.join("src/bpf/intf.h"));

    println!("cargo:rerun-if-changed=../scx_cohort_common/src/lib.rs");
    println!("cargo:rerun-if-changed=../scx_cohort_common/cbindgen.toml");

    scx_cargo::BpfBuilder::new()
        .unwrap()
        .enable_skel("src/bpf/main.bpf.c", "bpf")
        .build()
        .unwrap();
}
