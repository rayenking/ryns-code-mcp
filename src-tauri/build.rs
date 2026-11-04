fn main() {
    let result = std::panic::catch_unwind(|| {
        tauri_build::build();
    });
    if let Err(e) = result {
        let target = std::env::var("TARGET").unwrap_or_default();
        let host = std::env::var("HOST").unwrap_or_default();
        if target.contains("windows") && !host.contains("windows") {
            println!("cargo:warning=tauri-winres skipped Windows resource compilation on non-Windows host (llvm-rc not present)");
        } else {
            std::panic::resume_unwind(e);
        }
    }
}
