use anyhow::{Context, Result, bail};
use std::sync::atomic::AtomicBool;
use twilight_sync::{protocol::GameId, source_build};

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let game = match arguments.next().as_deref() {
        Some("dusklight") => GameId::Dusklight,
        Some("totk") => GameId::Totk,
        Some("triforce") => GameId::Triforce,
        _ => bail!("Usage: source-check dusklight|totk|triforce APPROVED_COMMIT [JOBS]"),
    };
    let commit = arguments
        .next()
        .context("Explicit source commit approval is required")?;
    let jobs = arguments
        .next()
        .map(|value| value.parse::<u8>())
        .transpose()?;
    anyhow::ensure!(
        jobs.is_none_or(|jobs| (1..=16).contains(&jobs)),
        "Choose 1 to 16 compiler workers"
    );
    anyhow::ensure!(
        arguments.next().is_none(),
        "Unexpected validation arguments"
    );
    println!("{}", source_build::WARNING);
    println!(
        "Validation installs side-by-side. Active selections, saves and play reservations are not changed."
    );
    let built = source_build::stage_configured(
        game,
        &commit,
        jobs.unwrap_or_default(),
        &AtomicBool::new(false),
        &mut |message| {
            println!("{message}");
            Ok(())
        },
    )?;
    println!(
        "SOURCE INSTALL VERIFIED\nGame: {game:?}\nTag: {}\nCommit: {}\nExecutable: {}\nSHA-256: {}\nVerified save before/after: {}\nLog: {}\nActive installation and reservations unchanged.",
        built.tag,
        built.commit,
        built.executable.display(),
        built.sha256,
        built.expected_save.as_deref().unwrap_or("no save"),
        built.log.display()
    );
    Ok(())
}
