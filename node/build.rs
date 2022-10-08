use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../ionian-client");

    let status = Command::new("go")
        .current_dir("../ionian-client")
        .args(vec!["build", "-o", "../target"])
        .status()
        .unwrap();

    println!("build ionian-client with status {}", status);
    
    println!("cargo:rerun-if-changed=../ionian-kv");

    let status = Command::new("cargo")
        .current_dir("../ionian-kv")
        .args(vec!["build", "--release", "--all-features"])
        .status()
        .unwrap();

    println!("build ionian-kv with status {}", status);
    
    println!("cargo:rerun-if-changed=../ionian-kv/target/release/ionian-kv");
    let status = Command::new("cp")
        .current_dir("../ionian-kv")
        .args(vec!["target/release/ionian_kv", "../target/"])
        .status()
        .unwrap();
    
    println!("copy ionian-kv with status {}", status);
}
