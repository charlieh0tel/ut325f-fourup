use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;
use clap_derive::Parser;

use ut325f_rs::BleTransport;
use ut325f_rs::Meter;
use ut325f_rs::Reading;
use ut325f_rs::Transport;

use std::time::Duration;

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
    /// without addresses.
    #[arg(long, default_value_t = 8, value_name = "SECONDS")]
    scan_time: u64,

    /// Serial ports to open or, with --ble, meter Bluetooth addresses.
    #[arg(num_args = 0..=4, action = ArgAction::Set, value_name = "PORT|ADDR")]
    ports: Vec<String>,
}

fn find_unique_non_nan_value_and_position(arr: [f32; 4]) -> Option<(f32, usize)> {
    let mut non_nan_values = arr.iter().enumerate().filter(|&(_, &v)| !v.is_nan());

    let first = non_nan_values.next()?;
    if non_nan_values.next().is_none() {
        Some((*first.1, first.0))
    } else {
        None
    }
}

fn collect_readings(maybe_readings: Vec<ut325f_rs::Result<Reading>>) -> Result<Vec<Reading>> {
    Ok(maybe_readings
        .into_iter()
        .collect::<ut325f_rs::Result<_>>()?)
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
        let rssi = meter
            .rssi
            .map_or_else(|| "cached".to_owned(), |rssi| format!("{rssi} dBm"));
        println!("{}  {}  [{}]", meter.address, meter.name, rssi);
    }
    Ok(())
}

async fn discover_four(scan_time: Duration) -> Result<Vec<String>> {
    let meters = BleTransport::discover(scan_time).await?;
    if meters.len() != 4 {
        bail!(
            "Expected to discover exactly four meters, found {}:{}",
            meters.len(),
            meters
                .iter()
                .map(|m| format!("\n  {}  {}", m.address, m.name))
                .collect::<String>()
        );
    }
    Ok(meters.into_iter().map(|m| m.address).collect())
}

async fn run<T: Transport>(mut meters: Vec<Meter<T>>, relative_timestamps: bool) -> Result<()> {
    let mut unix_time_offset: f64 = 0.;

    loop {
        let maybe_readings =
            futures::future::join_all(meters.iter_mut().map(|meter| meter.read())).await;
        let readings = collect_readings(maybe_readings)?;
        let mut positional_readings = [f32::NAN; 4];

        for reading in &readings {
            if let Some((value, index)) =
                find_unique_non_nan_value_and_position(reading.current_temps_c)
            {
                if positional_readings[index].is_nan() {
                    positional_readings[index] = value
                } else {
                    return Err(anyhow!(
                        "Multiple meters returned a value in the same position {}",
                        index + 1
                    ));
                }
            }
        }

        if positional_readings.iter().filter(|v| !v.is_nan()).count() != 4 {
            bail!("Did not receive four readings.");
        }

        let min_timestamp = readings
            .iter()
            .map(|r| r.timestamp)
            .min()
            .with_context(|| "no min timestamp??")?;
        let max_timestamp = readings
            .iter()
            .map(|r| r.timestamp)
            .max()
            .with_context(|| "no max timestamp??")?;
        assert!(max_timestamp.duration_since(min_timestamp)? < Duration::from_secs(1));

        let timestamp = min_timestamp;
        if relative_timestamps && unix_time_offset == 0. {
            unix_time_offset = system_time_to_unix_seconds(timestamp)?;
        }

        let timestamp = system_time_to_unix_seconds(timestamp)? - unix_time_offset;
        println!(
            "{:.3},{:.3},{:.3},{:.3},{:.3}",
            timestamp,
            positional_readings[0],
            positional_readings[1],
            positional_readings[2],
            positional_readings[3]
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let scan_time = Duration::from_secs(args.scan_time);

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
        let maybe_meters =
            futures::future::join_all(addresses.iter().map(|address| Meter::open_ble(address)))
                .await;
        let meters: Vec<Meter<BleTransport>> =
            maybe_meters.into_iter().collect::<ut325f_rs::Result<_>>()?;
        return run(meters, args.relative_timestamps).await;
    }

    if args.ports.len() != 4 {
        bail!("Four ports not specified.");
    }
    let maybe_meters =
        futures::future::join_all(args.ports.iter().map(|port| Meter::open_serial(port))).await;
    let meters: Vec<Meter<ut325f_rs::SerialTransport>> =
        maybe_meters.into_iter().collect::<ut325f_rs::Result<_>>()?;
    run(meters, args.relative_timestamps).await
}
