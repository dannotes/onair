// Usage: cargo run --example bulb_set -- <ip> <hex> [brightness]
// Sets the bulb to the given color and leaves it ON. Exit immediately.
// Example: cargo run --example bulb_set -- 192.168.29.23 00ff00 100
use onair::{bulb, models::Rgb};
use std::net::Ipv4Addr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: bulb_set <ip> <hex> [brightness]");
        eprintln!("Example: bulb_set 192.168.29.23 00ff00 100");
        std::process::exit(1);
    }
    let ip: Ipv4Addr = args[1].parse()?;
    let color = Rgb::from_hex(&args[2]).ok_or_else(|| anyhow::anyhow!("invalid hex color"))?;
    let brightness: u8 = args.get(3).map(|s| s.parse()).transpose()?.unwrap_or(100);

    bulb::set_pilot_color(ip, color, brightness).await?;
    println!(
        "Bulb {} set to {} at {}% — left ON.",
        ip,
        color.to_hex(),
        brightness
    );
    Ok(())
}
