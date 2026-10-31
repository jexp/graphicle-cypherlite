// `parquet_codec`: the Parquet snapshot codec is compiled in (ADR-0105).
// `arrow`/`parquet` are optional and never build on wasm32.
fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(parquet_codec)");
    let feature = std::env::var_os("CARGO_FEATURE_PARQUET").is_some();
    let wasm = std::env::var("CARGO_CFG_TARGET_ARCH").is_ok_and(|arch| arch == "wasm32");
    if feature && !wasm {
        println!("cargo::rustc-cfg=parquet_codec");
    }
}
