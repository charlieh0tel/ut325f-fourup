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
    // The single-member ble_mode group exists for `requires`: clap
    // treats `requires = "ble"` as satisfied by any member of a group
    // ble belongs to (here, --discover via the bluetooth group).
    #[arg(long, short = 'b', conflicts_with = "discover", group = "ble_mode")]
    ble: bool,

    /// Discover meters over Bluetooth LE, print them, and exit.
    #[arg(long, short = 'd')]
    discover: bool,

    /// Disconnect the meters on exit. By default they are left
    /// connected: a connected meter stays awake and the next run
    /// finds it without a scan.
    // conflicts_with makes `--discover --disconnect` report the
    // conflict instead of suggesting the unsatisfiable --ble.
    #[arg(long, requires = "ble_mode", conflicts_with = "discover")]
    disconnect: bool,

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
fn write_line(writer: &mut impl Write, line: &str) -> Result<bool> {
    match writeln!(writer, "{line}") {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Writes a line to stdout on the blocking pool: a consumer that
/// stalls without closing the pipe blocks the write, and it must not
/// wedge the async task that is also polling Ctrl-C.
async fn write_stdout_line(line: String) -> Result<bool> {
    tokio::task::spawn_blocking(move || write_line(&mut std::io::stdout().lock(), &line)).await?
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
    for meter in &meters {
        let status = match (meter.connected, meter.rssi) {
            (true, _) => "connected".to_owned(),
            (false, Some(rssi)) => format!("{rssi} dBm"),
            (false, None) => "cached".to_owned(),
        };
        if !write_stdout_line(format!("{}  {}  [{}]", meter.address, meter.name, status)).await? {
            break;
        }
    }
    Ok(())
}

/// Races `open` against Ctrl-C: the discovery/connect phase can last
/// minutes, and an unhandled SIGINT there kills the process with
/// connections half-established. On interrupt the dropped open's
/// guards spawn best-effort disconnects; a short grace period lets
/// them run before exit.
async fn open_interruptible<T: Transport>(
    open: impl Future<Output = ut325f_fourup::Result<FourUp<T>>>,
) -> Result<FourUp<T>> {
    tokio::select! {
        fourup = open => Ok(fourup?),
        interrupt = tokio::signal::ctrl_c() => {
            interrupt?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            std::process::exit(130);
        }
    }
}

async fn run<T: Transport>(
    mut fourup: FourUp<T>,
    relative_timestamps: bool,
    disconnect: bool,
) -> Result<()> {
    // Ctrl-C must also go through teardown: dying with connections
    // held leaves them dangling in the Bluetooth stack instead of
    // deliberately kept (detach) or released (close).
    let mut interrupted = false;
    let result: Result<()> = tokio::select! {
        result = read_rows(&mut fourup, relative_timestamps) => result,
        interrupt = tokio::signal::ctrl_c() => {
            interrupted = true;
            interrupt.map_err(Into::into)
        }
    };
    // Bounded: a stuck Bluetooth/D-Bus operation must not hang exit.
    const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(10);
    let torn_down: Result<()> = match tokio::time::timeout(TEARDOWN_TIMEOUT, async {
        if disconnect {
            fourup.close().await
        } else {
            fourup.detach().await
        }
    })
    .await
    {
        Ok(torn_down) => torn_down.map_err(Into::into),
        Err(_) => Err(anyhow!("Teardown timed out after {TEARDOWN_TIMEOUT:?}.")),
    };
    if interrupted {
        // Exit directly: runtime drop would wait for a row write
        // still blocked on a stalled consumer.
        if let Err(e) = torn_down {
            eprintln!("Error: {e}");
        }
        std::process::exit(130);
    }
    match (result, torn_down) {
        // The read error is the exit error, but a simultaneous
        // teardown failure (meters possibly still connected) must
        // not go unreported.
        (Err(read), Err(teardown)) => {
            eprintln!("Error: {teardown}");
            Err(read)
        }
        (result, torn_down) => result.and(torn_down),
    }
}

async fn read_rows<T: Transport>(fourup: &mut FourUp<T>, relative_timestamps: bool) -> Result<()> {
    let mut relative_start: Option<Instant> = None;

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
        if !write_stdout_line(format!(
            "{:.3},{:.3},{:.3},{:.3},{:.3}",
            timestamp, row.temps_c[0], row.temps_c[1], row.temps_c[2], row.temps_c[3]
        ))
        .await?
        {
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
            0 => open_interruptible(FourUp::discover_ble(scan_time, Config::default())).await?,
            4 => open_interruptible(FourUp::open_ble(&args.ports, Config::default())).await?,
            n => bail!("--ble takes four addresses or none to discover, got {n}."),
        };
        return run(fourup, args.relative_timestamps, args.disconnect).await;
    }

    if args.ports.len() != 4 {
        bail!("Four ports not specified.");
    }
    let fourup = open_interruptible(FourUp::open_serial(&args.ports, Config::default())).await?;
    run(fourup, args.relative_timestamps, args.disconnect).await
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

    /// clap treats `requires` aimed at a member of an ArgGroup as
    /// satisfied by any member of that group; pin the flag matrix so
    /// the single-member-group workaround doesn't regress.
    #[test]
    fn test_flag_matrix() {
        assert!(Args::try_parse_from(["f", "--ble", "--disconnect"]).is_ok());
        assert!(Args::try_parse_from(["f", "--discover", "--disconnect"]).is_err());
        assert!(Args::try_parse_from(["f", "--disconnect", "a", "b", "c", "d"]).is_err());
        assert!(Args::try_parse_from(["f", "--ble", "--scan-time", "5"]).is_ok());
        assert!(Args::try_parse_from(["f", "--discover", "--scan-time", "5"]).is_ok());
        assert!(Args::try_parse_from(["f", "--scan-time", "5", "a", "b", "c", "d"]).is_err());
        assert!(Args::try_parse_from(["f", "--ble", "--discover"]).is_err());
    }

    #[test]
    fn test_write_line() {
        let mut buf = Vec::new();
        assert!(write_line(&mut buf, "row").unwrap());
        assert_eq!(buf, b"row\n");
        assert!(!write_line(&mut BrokenPipeWriter, "row").unwrap());
    }
}
