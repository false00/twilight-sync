fn main() -> anyhow::Result<()> {
    #[cfg(all(windows, debug_assertions))]
    {
        let backend = twilight_sync::runtime::ui_preview_backend(
            "ready",
            std::time::Duration::from_secs(1800),
        )?;
        twilight_sync::ui::run_smoke(backend, false, "overview", "dark", 1.0)
    }
    #[cfg(not(all(windows, debug_assertions)))]
    anyhow::bail!("The isolated UI preview requires a Windows debug build")
}
