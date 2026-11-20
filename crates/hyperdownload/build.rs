//! The program's icon and version resource (Task Manager, Properties > Details). The version in the .rc is a placeholder: the
//! package's own version is written into a copy of it here.
fn main() {
    println!("cargo:rerun-if-changed=hyperdownload.rc");
    println!("cargo:rerun-if-changed=hyperdownload.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let v = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".into());
    let parts: Vec<&str> = v.split(['.', '-']).take(3).collect();
    let comma = format!("{},{},{},0", parts.first().unwrap_or(&"0"), parts.get(1).unwrap_or(&"0"), parts.get(2).unwrap_or(&"0"));
    let dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let text = std::fs::read_to_string(dir.join("hyperdownload.rc")).expect("read .rc").replace("0,1,0,0", &comma).replace("\"0.1.0\"", &format!("\"{v}\""));
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    std::fs::copy(dir.join("hyperdownload.ico"), out.join("hyperdownload.ico")).expect("copy icon");
    let copy = out.join("hyperdownload.rc");
    std::fs::write(&copy, text).expect("write .rc");
    embed_resource::compile(&copy, embed_resource::NONE).manifest_optional().expect("embed resources");
}
