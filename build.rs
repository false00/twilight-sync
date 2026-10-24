fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=assets\\twilight-sync.ico");
    if std::env::var("CARGO_CFG_TARGET_OS")? == "windows" {
        let icon = std::path::Path::new("assets").join("twilight-sync.ico");
        winresource::WindowsResource::new()
            .set_icon(icon.to_str().ok_or("Windows icon path is not Unicode")?)
            .set("FileDescription", "Twilight Sync")
            .set("ProductName", "Twilight Sync")
            .set("OriginalFilename", "twilight-sync.exe")
            .compile()?;
    }
    Ok(())
}
