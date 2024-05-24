use std::sync::OnceLock;

static EXAMPLE: OnceLock<String> = OnceLock::new();

fn main() {
    let mut it = std::env::args();
    let _ = it.next();
    let implementation = it.next().unwrap();
    let example = it.next().unwrap();

    let path = match implementation.as_str() {
        "go" => format!("benchmarks/go/{}", example),
        "roc-nea" => format!("benchmarks/roc-nea/{}", example),
        "rust-nea" => format!("benchmarks/rust-nea/target/release/{}", example),
        "rust-tokio" => format!("benchmarks/rust-tokio/target/release/{}", example),
        _ => unreachable!(),
    };

    let mut child = std::process::Command::new(path)
        .env("RUST_LOG", "error")
        .spawn()
        .unwrap();

    EXAMPLE.set(example.clone()).unwrap();

    let json = energy_bench::EnergyTool::from_function(|()| {
        let siege = std::process::Command::new("siege")
            .args(&["-j", "-c", "4", "-r", "10000", "-i", "-f", &format!("benchmarks/{}.txt", EXAMPLE.get().unwrap())])
            .output()
            .unwrap();

        if siege.status.code() != Some(0) {
            panic!("siege failed: {:?}", siege);
        }
    }).benchmark().unwrap();

    std::fs::write(format!("benchmarks/{implementation}-{example}.json"), json).unwrap();

    child.kill().unwrap();
}
