use std::{env, path::PathBuf, process::Command};
fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.rc");
    let kits = PathBuf::from(env::var("ProgramFiles(x86)").unwrap()).join("Windows Kits/10/bin");
    let mut versions: Vec<_> = std::fs::read_dir(&kits)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("x64/rc.exe").exists())
        .collect();
    versions.sort();
    let rc = versions
        .last()
        .expect("Windows SDK required")
        .join("x64/rc.exe");
    let res = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app.res");
    assert!(Command::new(rc)
        .args(["/nologo", "/fo"])
        .arg(&res)
        .arg("app.rc")
        .status()
        .unwrap()
        .success());
    println!("cargo:rustc-link-arg={}", res.display());
}
