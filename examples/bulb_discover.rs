use onair::bulb;
use std::time::Duration;
use tracing_subscriber::{fmt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,onair=debug")),
        )
        .with_target(false)
        .init();

    println!("Broadcasting WiZ discovery on UDP 38899 (waiting 3s)...\n");
    let bulbs = bulb::discover(Duration::from_secs(3)).await?;
    if bulbs.is_empty() {
        println!("No bulbs found.");
    } else {
        println!("{:<14}  {:<16}  MODULE", "MAC", "IP");
        println!("{}", "-".repeat(60));
        for b in &bulbs {
            println!(
                "{:<14}  {:<16}  {}",
                b.mac,
                b.ip,
                b.module.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(())
}
