extern crate cc;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let manifest_dir = manifest_dir.trim_end_matches("/async-server");
    let profile = std::env::var("PROFILE").unwrap();
    let target_dir = format!("{manifest_dir}/target/{profile}");

    println!("cargo:rerun-if-changed={target_dir}/libroc_app.a");
    println!("cargo:rustc-link-lib=static=roc_app");
    println!("cargo:rustc-link-search={target_dir}");
}
