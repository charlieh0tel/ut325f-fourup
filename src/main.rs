use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;

use ut325f_fourup::BleTransport;
use ut325f_fourup::Config;
use ut325f_fourup::FourUp;
use ut325f_fourup::Transport;

use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
#[command(group = clap::ArgGroup::new("bluetooth").args(["ble", "discover"]))]
struct Args {
    /// Makes timestamps start at zero.
    #[arg(long, short = 'z')]
    relative_timestamps: bool,

    /// Use Bluetooth LE: give four addresses (e.g. E8:26:CF:F1:23:61),
    /// or none to discover exactly four meters.
    #[arg(long, short = 'b', conflicts_with = "discover")]
    ble: bool,

    /// Discover meters over Bluetooth LE, print them, and exit.
    #[arg(long, short = 'd')]
    discover: bool,

    /// Bluetooth scan duration in seconds, for --discover and --ble
    /// without addresses [default: 8].
    #[arg(long, value_name = "SECONDS", requires = "bluetooth",
          value_parser = clap::value_parser!(u64).range(1..=3600))]
    scan_time: Option<u64>,

    /// Serial ports to open or, with --ble, meter Bluetooth addresses.
    #[arg(num_args = 0..=4, action = ArgAction::Set, value_name = "PORT|ADDR")]
    ports: Vec<String>,
}

/// Writes a line to `writer`; returns Ok(false) when the consumer has
/// gone away (e.g. piped to head), which ends output cleanly.
fn write_line(writer: &mut impl Write, line: std::fmt::Arguments) -> Result<bool> {
    match writer.write_fmt(format_args!("{line}\n")) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e.into()),
    }
}

pub fn system_time_to_unix_seconds(time: SystemTime) -> Result<f64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => {
            let seconds = duration.as_secs() as f64;
            let nanos = duration.subsec_nanos() as f64 / 1_000_000_000.0;
            Ok(seconds + nanos)
        }
        Err(e) => Err(anyhow!("Time went backwards: {:?}", e)),
    }
}

async fn discover(scan_time: Duration) -> Result<()> {
    let meters = BleTransport::discover(scan_time).await?;
    if meters.is_empty() {
        eprintln!("No meters found.");
    }
    let mut stdout = std::io::stdout().lock();
    for meter in &meters {
        let rssi = meter
            .rssi
            .map_or_else(|| "cached".to_owned(), |rssi| format!("{rssi} dBm"));
        if !write_line(
            &mut stdout,
            format_args!("{}  {}  [{}]", meter.address, meter.name, rssi),
        )? {
            break;
        }
    }
    Ok(())
}

async fn run<T: Transport>(mut fourup: FourUp<T>, relative_timestamps: bool) -> Result<()> {
    let result = read_rows(&mut fourup, relative_timestamps).await;
    let closed = fourup.close().await;
    // A read error is the story; a close failure matters only on an
    // otherwise clean exit.
    result.and(closed.map_err(Into::into))
}

async fn read_rows<T: Transport>(fourup: &mut FourUp<T>, relative_timestamps: bool) -> Result<()> {
    let mut relative_start: Option<Instant> = None;
    let mut stdout = std::io::stdout().lock();

    loop {
        let row = fourup.read_row().await?;

        // Relative time comes from a monotonic clock so a stepped
        // system clock (NTP, manual change) can't make it jump.
        let timestamp = if relative_timestamps {
            relative_start
                .get_or_insert_with(Instant::now)
                .elapsed()
                .as_secs_f64()
        } else {
            system_time_to_unix_seconds(row.timestamp)?
        };
        if !write_line(
            &mut stdout,
            format_args!(
                "{:.3},{:.3},{:.3},{:.3},{:.3}",
                timestamp, row.temps_c[0], row.temps_c[1], row.temps_c[2], row.temps_c[3]
            ),
        )? {
            return Ok(());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let scan_time = Duration::from_secs(args.scan_time.unwrap_or(8));

    if args.discover {
        if !args.ports.is_empty() {
            bail!("--discover takes no ports or addresses.");
        }
        return discover(scan_time).await;
    }

    if args.ble {
        let fourup = match args.ports.len() {
            0 => FourUp::discover_ble(scan_time, Config::default()).await?,
            4 => FourUp::open_ble(&args.ports, Config::default()).await?,
            n => bail!("--ble takes four addresses or none to discover, got {n}."),
        };
        return run(fourup, args.relative_timestamps).await;
    }

    if args.ports.len() != 4 {
        bail!("Four ports not specified.");
    }
    let fourup = FourUp::open_serial(&args.ports, Config::default()).await?;
    run(fourup, args.relative_timestamps).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_time_to_unix_seconds() {
        let time = UNIX_EPOCH + Duration::new(1_000, 250_000_000);
        assert_eq!(system_time_to_unix_seconds(time).unwrap(), 1000.25);
        assert!(system_time_to_unix_seconds(UNIX_EPOCH - Duration::from_secs(1)).is_err());
    }

    struct BrokenPipeWriter;

    impl Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_write_line() {
        let mut buf = Vec::new();
        assert!(write_line(&mut buf, format_args!("row")).unwrap());
        assert_eq!(buf, b"row\n");
        assert!(!write_line(&mut BrokenPipeWriter, format_args!("row")).unwrap());
    }
}
