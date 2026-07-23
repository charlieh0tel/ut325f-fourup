//! Read four Uni-T UT325F thermocouple meters in lockstep.
//!
//! Each meter carries one thermocouple, plugged into one of its four
//! inputs, and every meter must use a different input: the input
//! position identifies the meter and selects its column in a [`Row`].
//! (The UT325F's four inputs are not galvanically isolated, hence one
//! thermocouple per meter.)
//!
//! Open the meters with [`FourUp::open_serial`], [`FourUp::open_ble`],
//! or [`FourUp::discover_ble`], then call [`FourUp::read_row`] in a
//! loop. Rows are synchronized: each meter is drained to its freshest
//! frame and sets whose timestamps spread beyond
//! [`Config::max_skew`] are discarded and re-read.

use std::time::Duration;
use std::time::SystemTime;

pub use ut325f_rs::{BleTransport, DiscoveredMeter, Meter, Reading, SerialTransport, Transport};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("Invalid config: {reason}.")]
    InvalidConfig { reason: &'static str },
    #[error("Expected exactly four sources, got {0}.")]
    SourceCount(usize),
    #[error("Duplicate {kind}: {source_id}.")]
    DuplicateSource {
        kind: &'static str,
        source_id: String,
    },
    #[error("{source_id}: {cause}")]
    Open {
        source_id: String,
        cause: ut325f_rs::Error,
    },
    #[error("{source_id}: {cause}")]
    Read {
        source_id: String,
        cause: ut325f_rs::Error,
    },
    #[error("Meter {source_id}: no active input.")]
    NoActiveInput { source_id: String },
    #[error("Meter {source_id}: {count} active inputs, expected exactly one.")]
    MultipleActiveInputs { source_id: String, count: usize },
    #[error("Meters {first} and {second} both report position {position}.")]
    DuplicatePosition {
        first: String,
        second: String,
        position: usize,
    },
    #[error("No meter reported position {position}.")]
    MissingPosition { position: usize },
    #[error("Readings misaligned by {skew:?} for {rows} consecutive rows.")]
    Misaligned { skew: Duration, rows: u32 },
    #[error("Expected to see exactly four meters, saw {}:{}", seen.len(), format_seen(seen))]
    DiscoverCount { seen: Vec<DiscoveredMeter> },
    #[error(transparent)]
    Discover(ut325f_rs::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn format_seen(seen: &[DiscoveredMeter]) -> String {
    seen.iter()
        .map(|m| format!("\n  {}  {}", m.address, m.name))
        .collect()
}

/// Synchronized-read behavior; [`Config::default`] matches the
/// `ut325f-fourup` CLI. Values are validated when a [`FourUp`] is
/// opened.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Config {
    /// Widest allowed spread between the four timestamps of a row.
    /// Must be nonzero.
    pub max_skew: Duration,
    /// Consecutive misaligned sets tolerated before
    /// [`FourUp::read_row`] gives up. Must be nonzero.
    pub max_consecutive_skewed_rows: u32,
    /// How long a meter must stay quiet before its last frame is
    /// considered fresh rather than queued backlog. Must be nonzero
    /// and well under the ~333 ms frame interval; at most 250 ms.
    pub drain_timeout: Duration,
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.max_skew.is_zero() {
            return Err(Error::InvalidConfig {
                reason: "max_skew must be nonzero",
            });
        }
        if self.max_consecutive_skewed_rows == 0 {
            return Err(Error::InvalidConfig {
                reason: "max_consecutive_skewed_rows must be nonzero",
            });
        }
        if self.drain_timeout.is_zero() || self.drain_timeout > Duration::from_millis(250) {
            return Err(Error::InvalidConfig {
                reason: "drain_timeout must be nonzero and at most 250 ms",
            });
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_skew: Duration::from_secs(1),
            max_consecutive_skewed_rows: 5,
            drain_timeout: Duration::from_millis(50),
        }
    }
}

/// One synchronized sample set.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// Earliest timestamp of the four readings.
    pub timestamp: SystemTime,
    /// Temperature per input position, degrees Celsius.
    pub temps_c: [f32; 4],
}

/// Four meters read in lockstep.
pub struct FourUp<T: Transport> {
    meters: Vec<(String, Meter<T>)>,
    config: Config,
    consecutive_skewed_rows: u32,
}

impl FourUp<SerialTransport> {
    /// Opens four meters on USB serial ports (e.g. "/dev/ttyUSB0").
    pub async fn open_serial(ports: &[String], config: Config) -> Result<Self> {
        check_distinct("port", ports, false)?;
        Self::open_with(
            ports,
            async |port: String| Meter::open_serial(&port).await,
            config,
        )
        .await
    }
}

impl FourUp<BleTransport> {
    /// Opens four meters by Bluetooth address (e.g. "E8:26:CF:F1:23:61").
    pub async fn open_ble(addresses: &[String], config: Config) -> Result<Self> {
        check_distinct("address", addresses, true)?;
        Self::open_with(
            addresses,
            async |address: String| Meter::open_ble(&address).await,
            config,
        )
        .await
    }

    /// Scans for `scan_time` and opens the meters seen, requiring that
    /// exactly four were seen. Meters only known from the Bluetooth
    /// stack's cache (e.g. paired but powered off) are ignored.
    pub async fn discover_ble(scan_time: Duration, config: Config) -> Result<Self> {
        let seen: Vec<_> = BleTransport::discover(scan_time)
            .await
            .map_err(Error::Discover)?
            .into_iter()
            .filter(|m| m.rssi.is_some())
            .collect();
        if seen.len() != 4 {
            return Err(Error::DiscoverCount { seen });
        }
        let addresses: Vec<String> = seen.into_iter().map(|m| m.address).collect();
        Self::open_ble(&addresses, config).await
    }
}

impl<T: Transport> FourUp<T> {
    /// Opens one meter per source (in parallel) on any transport,
    /// pairing each with its source so errors can say which meter
    /// failed. The convenience constructors use this; call it directly
    /// for custom transports.
    pub async fn open_with<F, Fut>(sources: &[String], open: F, config: Config) -> Result<Self>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = ut325f_rs::Result<Meter<T>>>,
    {
        config.validate()?;
        if sources.len() != 4 {
            return Err(Error::SourceCount(sources.len()));
        }
        check_distinct("source", sources, false)?;
        let maybe_meters =
            futures::future::join_all(sources.iter().map(|source| open(source.clone()))).await;
        let meters = sources
            .iter()
            .zip(maybe_meters)
            .map(|(source_id, meter)| match meter {
                Ok(meter) => Ok((source_id.clone(), meter)),
                Err(cause) => Err(Error::Open {
                    source_id: source_id.clone(),
                    cause,
                }),
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            meters,
            config,
            consecutive_skewed_rows: 0,
        })
    }

    /// Returns the next synchronized row: each meter's freshest frame,
    /// mapped to columns by input position. Misaligned sets are
    /// discarded and re-read, up to
    /// [`Config::max_consecutive_skewed_rows`].
    pub async fn read_row(&mut self) -> Result<Row> {
        let config = self.config;
        loop {
            let maybe_readings = futures::future::join_all(self.meters.iter_mut().map(
                |(source_id, meter)| async move {
                    match read_latest(meter, config.drain_timeout).await {
                        Ok(reading) => Ok((source_id.clone(), reading)),
                        Err(cause) => Err(Error::Read {
                            source_id: source_id.clone(),
                            cause,
                        }),
                    }
                },
            ))
            .await;
            let readings: Vec<(String, Reading)> =
                maybe_readings.into_iter().collect::<Result<_>>()?;
            let temps_c = assemble_positions(&readings)?;

            let timestamps = || readings.iter().map(|(_, r)| r.timestamp);
            let min_timestamp = timestamps().min().expect("four readings");
            let max_timestamp = timestamps().max().expect("four readings");
            let skew = max_timestamp
                .duration_since(min_timestamp)
                .unwrap_or_default();
            if skew >= self.config.max_skew {
                self.consecutive_skewed_rows += 1;
                if self.consecutive_skewed_rows >= self.config.max_consecutive_skewed_rows {
                    return Err(Error::Misaligned {
                        skew,
                        rows: self.consecutive_skewed_rows,
                    });
                }
                continue;
            }
            self.consecutive_skewed_rows = 0;

            return Ok(Row {
                timestamp: min_timestamp,
                temps_c,
            });
        }
    }
}

/// Upper bound on frames discarded per drain; far above any real
/// backlog, it guarantees the drain terminates even on a transport
/// that is never quiet.
const MAX_DRAIN_FRAMES: usize = 64;

/// Returns the meter's most recent reading, draining any queued
/// backlog so the value reflects now rather than when it was queued.
async fn read_latest<T: Transport>(
    meter: &mut Meter<T>,
    drain_timeout: Duration,
) -> ut325f_rs::Result<Reading> {
    let mut reading = meter.read().await?;
    for _ in 0..MAX_DRAIN_FRAMES {
        match tokio::time::timeout(drain_timeout, meter.read()).await {
            Ok(next) => reading = next?,
            Err(_) => break,
        }
    }
    Ok(reading)
}

/// Rejects repeated sources before any is opened; Bluetooth addresses
/// compare case-insensitively.
fn check_distinct(kind: &'static str, sources: &[String], ignore_ascii_case: bool) -> Result<()> {
    for (i, a) in sources.iter().enumerate() {
        if sources[i + 1..]
            .iter()
            .any(|b| a == b || (ignore_ascii_case && a.eq_ignore_ascii_case(b)))
        {
            return Err(Error::DuplicateSource {
                kind,
                source_id: a.clone(),
            });
        }
    }
    Ok(())
}

/// Maps each meter's single active input to its position, diagnosing
/// meters with no or several active inputs and position collisions.
fn assemble_positions(readings: &[(String, Reading)]) -> Result<[f32; 4]> {
    let mut positional = [f32::NAN; 4];
    let mut claimed: [Option<&str>; 4] = [None; 4];
    for (source_id, reading) in readings {
        let active: Vec<(usize, f32)> = reading
            .current_temps_c
            .iter()
            .enumerate()
            .filter(|(_, v)| !v.is_nan())
            .map(|(position, &value)| (position, value))
            .collect();
        match active[..] {
            [] => {
                return Err(Error::NoActiveInput {
                    source_id: source_id.clone(),
                });
            }
            [(position, value)] => {
                if let Some(other) = claimed[position] {
                    return Err(Error::DuplicatePosition {
                        first: other.to_owned(),
                        second: source_id.clone(),
                        position: position + 1,
                    });
                }
                claimed[position] = Some(source_id);
                positional[position] = value;
            }
            _ => {
                return Err(Error::MultipleActiveInputs {
                    source_id: source_id.clone(),
                    count: active.len(),
                });
            }
        }
    }
    if let Some(missing) = claimed.iter().position(|c| c.is_none()) {
        return Err(Error::MissingPosition {
            position: missing + 1,
        });
    }
    Ok(positional)
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

    async fn no_open(_: String) -> ut325f_rs::Result<Meter<SerialTransport>> {
        unreachable!("open must not be called for an invalid config");
    }

    #[tokio::test]
    async fn test_open_rejects_invalid_config() {
        for (bad, reason) in [
            (
                Config {
                    max_skew: Duration::ZERO,
                    ..Config::default()
                },
                "max_skew must be nonzero",
            ),
            (
                Config {
                    max_consecutive_skewed_rows: 0,
                    ..Config::default()
                },
                "max_consecutive_skewed_rows must be nonzero",
            ),
            (
                Config {
                    drain_timeout: Duration::ZERO,
                    ..Config::default()
                },
                "drain_timeout must be nonzero and at most 250 ms",
            ),
            (
                Config {
                    drain_timeout: Duration::from_millis(300),
                    ..Config::default()
                },
                "drain_timeout must be nonzero and at most 250 ms",
            ),
        ] {
            let Err(err) = FourUp::open_with(&[], no_open, bad).await else {
                panic!("config accepted: {reason}");
            };
            assert_eq!(err.to_string(), format!("Invalid config: {reason}."));
        }
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
}
