// Usage: cargo run --example bulb_flash -- <ip> <hex_color>
// Example: cargo run --example bulb_flash -- 192.168.29.105 ff0000
use onair::{bulb, models::Rgb};
use std::net::Ipv4Addr;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: bulb_flash <ip> <hex_color>");
        eprintln!("Example: bulb_flash 192.168.29.105 ff0000");
        std::process::exit(1);
    }
    let ip: Ipv4Addr = args[1].parse()?;
    let color = Rgb::from_hex(&args[2]).ok_or_else(|| anyhow::anyhow!("invalid hex color"))?;

    println!("Flashing {} {} for 2s...", ip, color.to_hex());
    bulb::set_pilot_color(ip, color, 100).await?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    bulb::set_pilot_off(ip).await?;
    println!("Done.");
    Ok(())
}
