extern crate cc;

fn main() {
    println!("cargo:rerun-if-changed=/home/folkertdev/rust/nea/roc_app/target/debug/libroc_app.a");
    println!("cargo:rustc-link-lib=static=roc_app");
    println!("cargo:rustc-link-search=/home/folkertdev/rust/nea/roc_app/target/debug/");
}
