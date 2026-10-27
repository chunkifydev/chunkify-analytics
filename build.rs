fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if cfg!(feature = "bundled-duckdb") && target == "linux" {
        // libduckdb-sys 1.20000.0 links its extension constructor after the
        // DuckDB archive. GNU linkers need the loader selected before that
        // archive is scanned so bundled extensions can register before main.
        println!(
            "cargo:rustc-link-arg-bin=chunkify-analytics=-Wl,--undefined=duckdb_register_static_extensions"
        );
    } else if !cfg!(feature = "bundled-duckdb") {
        // Repository builds, installed executables, and demo bundles share this layout.
        match target.as_str() {
            "linux" => println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib"),
            "macos" => println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path/../lib"),
            _ => {}
        }
    }
}
