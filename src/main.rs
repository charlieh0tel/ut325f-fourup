use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;

use ut325f_rs::BleTransport;
use ut325f_rs::Meter;
use ut325f_rs::Reading;
use ut325f_rs::Transport;

use std::io::Write;
use std::time::Duration;
use std::time::Instant;

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
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
    #[arg(long, value_name = "SECONDS",
          value_parser = clap::value_parser!(u64).range(1..=3600))]
    scan_time: Option<u64>,

    /// Serial ports to open or, with --ble, meter Bluetooth addresses.
    #[arg(num_args = 0..=4, action = ArgAction::Set, value_name = "PORT|ADDR")]
    ports: Vec<String>,
}

/// Maps each meter's single active input to its position, diagnosing
/// meters with no or several active inputs and position collisions.
fn assemble_positions(readings: &[(String, Reading)]) -> Result<[f32; 4]> {
    let mut positional = [f32::NAN; 4];
    let mut claimed: [Option<&str>; 4] = [None; 4];
    for (source, reading) in readings {
        let active: Vec<(usize, f32)> = reading
            .current_temps_c
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_nan())
            .map(|(position, &value)| (position, value))
            .collect();
        match active[..] {
            [] => bail!("Meter {source}: no active input."),
            [(position, value)] => {
                if let Some(other) = claimed[position] {
                    bail!(
                        "Meters {other} and {source} both report position {}.",
                        position + 1
                    );
                }
                claimed[position] = Some(source);
                positional[position] = value;
            }
            _ => bail!(
                "Meter {source}: {} active inputs, expected exactly one.",
                active.len()
            ),
        }
    }
    if let Some(missing) = claimed.iter().position(|c| c.is_none()) {
        bail!("No meter reported position {}.", missing + 1);
    }
    Ok(positional)
}

/// Rejects repeated sources before any is opened; Bluetooth addresses
/// compare case-insensitively.
fn check_distinct(kind: &str, sources: &[String], ignore_ascii_case: bool) -> Result<()> {
    for (i, a) in sources.iter().enumerate() {
        if sources[i + 1..]
            .iter()
            .any(|b| a == b || (ignore_ascii_case && a.eq_ignore_ascii_case(b)))
        {
            bail!("Duplicate {kind}: {a}.");
        }
    }
    Ok(())
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

async fn discover_four(scan_time: Duration) -> Result<Vec<String>> {
    // Meters without an RSSI are only known from the Bluetooth stack's
    // cache (e.g. paired but powered off); a stale entry must not
    // break the exactly-four requirement.
    let meters: Vec<_> = BleTransport::discover(scan_time)
        .await?
        .into_iter()
        .filter(|m| m.rssi.is_some())
        .collect();
    if meters.len() != 4 {
        bail!(
            "Expected to see exactly four meters, saw {}:{}",
            meters.len(),
            meters
                .iter()
                .map(|m| format!("\n  {}  {}", m.address, m.name))
                .collect::<String>()
        );
    }
    Ok(meters.into_iter().map(|m| m.address).collect())
}

/// Meters send about three frames a second; frames already queued in
/// the transport (startup backlog, slow consumer) return immediately,
/// so a wait well under the frame interval distinguishes stale frames
/// from a fresh one.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

/// Returns the meter's most recent reading, draining any queued
/// backlog so the value reflects now rather than when it was queued.
async fn read_latest<T: Transport>(meter: &mut Meter<T>) -> ut325f_rs::Result<Reading> {
    let mut reading = meter.read().await?;
    while let Ok(next) = tokio::time::timeout(DRAIN_TIMEOUT, meter.read()).await {
        reading = next?;
    }
    Ok(reading)
}

const MAX_TIMESTAMP_SKEW: Duration = Duration::from_secs(1);
const MAX_CONSECUTIVE_SKEWED_ROWS: u32 = 5;

/// Opens one meter per source (in parallel), pairing each with its
/// source so later errors can say which meter failed.
async fn open_all<T, F, Fut>(sources: &[String], open: F) -> Result<Vec<(String, Meter<T>)>>
where
    T: Transport,
    F: Fn(String) -> Fut,
    Fut: Future<Output = ut325f_rs::Result<Meter<T>>>,
{
    let maybe_meters =
        futures::future::join_all(sources.iter().map(|source| open(source.clone()))).await;
    sources
        .iter()
        .zip(maybe_meters)
        .map(|(source, meter)| Ok((source.clone(), meter.with_context(|| source.clone())?)))
        .collect()
}

async fn run<T: Transport>(
    mut meters: Vec<(String, Meter<T>)>,
    relative_timestamps: bool,
) -> Result<()> {
    let mut relative_start: Option<Instant> = None;
    let mut consecutive_skewed_rows: u32 = 0;
    let mut stdout = std::io::stdout().lock();

    loop {
        let maybe_readings =
            futures::future::join_all(meters.iter_mut().map(|(source, meter)| async move {
                read_latest(meter)
                    .await
                    .with_context(|| source.clone())
                    .map(|reading| (source.clone(), reading))
            }))
            .await;
        let readings: Vec<(String, Reading)> = maybe_readings.into_iter().collect::<Result<_>>()?;
        let positional_readings = assemble_positions(&readings)?;

        let min_timestamp = readings
            .iter()
            .map(|(_, r)| r.timestamp)
            .min()
            .with_context(|| "no min timestamp??")?;
        let max_timestamp = readings
            .iter()
            .map(|(_, r)| r.timestamp)
            .max()
            .with_context(|| "no max timestamp??")?;
        let skew = max_timestamp.duration_since(min_timestamp)?;
        if skew >= MAX_TIMESTAMP_SKEW {
            consecutive_skewed_rows += 1;
            if consecutive_skewed_rows >= MAX_CONSECUTIVE_SKEWED_ROWS {
                bail!(
                    "Readings misaligned by {skew:?} for {consecutive_skewed_rows} consecutive rows."
                );
            }
            continue;
        }
        consecutive_skewed_rows = 0;

        // Relative time comes from a monotonic clock so a stepped
        // system clock (NTP, manual change) can't make it jump.
        let timestamp = if relative_timestamps {
            relative_start
                .get_or_insert_with(Instant::now)
                .elapsed()
                .as_secs_f64()
        } else {
            system_time_to_unix_seconds(min_timestamp)?
        };
        if !write_line(
            &mut stdout,
            format_args!(
                "{:.3},{:.3},{:.3},{:.3},{:.3}",
                timestamp,
                positional_readings[0],
                positional_readings[1],
                positional_readings[2],
                positional_readings[3]
            ),
        )? {
            return Ok(());
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.scan_time.is_some() && !args.ble && !args.discover {
        bail!("--scan-time only applies to --discover and --ble.");
    }
    let scan_time = Duration::from_secs(args.scan_time.unwrap_or(8));

    if args.discover {
        if !args.ports.is_empty() {
            bail!("--discover takes no ports or addresses.");
        }
        return discover(scan_time).await;
    }

    if args.ble {
        let addresses = match args.ports.len() {
            0 => discover_four(scan_time).await?,
            4 => args.ports.clone(),
            n => bail!("--ble takes four addresses or none to discover, got {n}."),
        };
        check_distinct("address", &addresses, true)?;
        let meters = open_all(&addresses, async |address: String| {
            Meter::open_ble(&address).await
        })
        .await?;
        return run(meters, args.relative_timestamps).await;
    }

    if args.ports.len() != 4 {
        bail!("Four ports not specified.");
    }
    check_distinct("port", &args.ports, false)?;
    let meters = open_all(&args.ports, async |port: String| {
        Meter::open_serial(&port).await
    })
    .await?;
    run(meters, args.relative_timestamps).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use ut325f_rs::HoldType;

    const N: f32 = f32::NAN;

    fn readings(temps: &[[f32; 4]]) -> Vec<(String, Reading)> {
        temps
            .iter()
            .enumerate()
            .map(|(i, &current_temps_c)| {
                (
                    format!("meter{}", i + 1),
                    Reading {
                        timestamp: SystemTime::now(),
                        current_temps_c,
                        held_temps_c: [N; 4],
                        hold_type: HoldType::Current,
                        meter_temp_c: 25.0,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn test_assemble_positions_any_order() {
        let positional = assemble_positions(&readings(&[
            [N, N, 3.0, N],
            [1.0, N, N, N],
            [N, N, N, 4.0],
            [N, 2.0, N, N],
        ]))
        .unwrap();
        assert_eq!(positional, [1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn test_assemble_positions_duplicate_names_both_meters() {
        let err = assemble_positions(&readings(&[[1.0, N, N, N], [10.0, N, N, N]]))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Meters meter1 and meter2 both report position 1.");
    }

    #[test]
    fn test_assemble_positions_no_active_input() {
        let err = assemble_positions(&readings(&[[N, N, N, N]]))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Meter meter1: no active input.");
    }

    #[test]
    fn test_assemble_positions_multiple_active_inputs() {
        let err = assemble_positions(&readings(&[[1.0, N, 3.0, N]]))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Meter meter1: 2 active inputs, expected exactly one.");
    }

    #[test]
    fn test_assemble_positions_missing_position() {
        let err = assemble_positions(&readings(&[[1.0, N, N, N], [N, 2.0, N, N], [N, N, N, 4.0]]))
            .unwrap_err()
            .to_string();
        assert_eq!(err, "No meter reported position 3.");
    }

    #[test]
    fn test_check_distinct() {
        let sources = [
            "/dev/a".to_owned(),
            "/dev/A".to_owned(),
            "/dev/b".to_owned(),
        ];
        assert!(check_distinct("port", &sources, false).is_ok());
        let err = check_distinct("address", &sources, true)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "Duplicate address: /dev/a.");
    }

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
