// Tails the auto-detected Teams log dir and prints presence events.
// Useful for verifying the regex against your live Teams installation.
use onair::{platform, presence::LogWatcher};
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = platform::default_teams_log_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine Teams log dir for this OS"))?;
    println!("Watching {}\n", dir.display());

    let verify = LogWatcher::verify(&dir);
    println!(
        "Verify: dir_exists={} files={} latest={:?}",
        verify.dir_exists, verify.log_files_count, verify.latest_log
    );
    if let Some(sample) = &verify.sample_match {
        println!("Sample matched line in latest log: {}", sample);
    }
    if let Some(err) = &verify.error {
        println!("Verify error: {}", err);
    }
    println!();

    let mut watcher = LogWatcher::new(dir);
    println!("Waiting for presence changes (change your Teams status to test)...\n");
    loop {
        match watcher.poll().await {
            Ok(events) => {
                for ev in events {
                    println!(
                        "{}  presence={:?}  raw={}",
                        chrono::Local::now().format("%H:%M:%S"),
                        ev.presence,
                        ev.raw
                    );
                }
            }
            Err(e) => eprintln!("poll error: {}", e),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
