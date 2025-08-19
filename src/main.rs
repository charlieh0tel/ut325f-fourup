use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use clap::ArgAction;
use clap::Parser;
use clap_derive::Parser;

use ut325f_rs::Meter;
use ut325f_rs::Reading;

use std::time::Duration;

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Makes timestamps start at zero.
    #[arg(long, short = 'z')]
    relative_timestamps: bool,

    /// Ports to open.
    #[arg(num_args=4, required = true, action = ArgAction::Set)]
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

fn collect_readings(maybe_readings: Vec<Result<Reading, anyhow::Error>>) -> Result<Vec<Reading>> {
    maybe_readings.into_iter().collect()
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    if args.ports.len() != 4 {
        bail!("Four ports not specified.");
    }

    let mut meters: Vec<Meter> = args
        .ports
        .iter()
        .map(|port| Meter::new(port.to_string()))
        .collect();

    futures::future::join_all(meters.iter_mut().map(|meter| meter.open())).await;

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
                        "Multiple meters returned a valuein the same position {}",
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
        if args.relative_timestamps && unix_time_offset == 0. {
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
