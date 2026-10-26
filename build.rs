fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|target| target == "linux") {
        // libduckdb-sys 1.20000.0 links its extension constructor after the
        // DuckDB archive. GNU linkers need the loader selected before that
        // archive is scanned so bundled extensions can register before main.
        println!(
            "cargo:rustc-link-arg-bin=chunkify-analytics=-Wl,--undefined=duckdb_register_static_extensions"
        );
    }
}
