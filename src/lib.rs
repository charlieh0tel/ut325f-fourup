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

#[cfg(feature = "serial")]
pub use ut325f_rs::SerialTransport;
#[cfg(feature = "ble")]
pub use ut325f_rs::{BleTransport, DiscoveredMeter};
pub use ut325f_rs::{Meter, Reading, Transport};

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
        #[source]
        cause: ut325f_rs::Error,
    },
    #[error("{source_id}: {cause}")]
    Read {
        source_id: String,
        #[source]
        cause: ut325f_rs::Error,
    },
    #[error("{source_id}: {cause}")]
    Close {
        source_id: String,
        #[source]
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
    #[cfg(feature = "ble")]
    #[error("Expected to see exactly four meters, saw {}:{}", seen.len(), format_seen(seen))]
    DiscoverCount { seen: Vec<DiscoveredMeter> },
    #[error(transparent)]
    Discover(#[from] ut325f_rs::Error),
    #[error("{}", format_errors(errors))]
    Multiple { errors: Vec<Error> },
}

pub type Result<T> = std::result::Result<T, Error>;

fn format_errors(errors: &[Error]) -> String {
    errors
        .iter()
        .map(Error::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Collects results, reporting every failure rather than just the
/// first (a single failure is returned unwrapped).
fn collect_all<T>(results: Vec<Result<T>>) -> Result<Vec<T>> {
    let mut values = Vec::new();
    let mut errors = Vec::new();
    for result in results {
        match result {
            Ok(value) => values.push(value),
            Err(error) => errors.push(error),
        }
    }
    match errors.len() {
        0 => Ok(values),
        1 => Err(errors.remove(0)),
        _ => Err(Error::Multiple { errors }),
    }
}

#[cfg(feature = "ble")]
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

#[cfg(feature = "serial")]
impl FourUp<SerialTransport> {
    /// Opens four meters on USB serial ports (e.g. "/dev/ttyUSB0").
    pub async fn open_serial(ports: &[String], config: Config) -> Result<Self> {
        check_sources("port", ports, false)?;
        Self::open_with(
            ports,
            async |port: String| Meter::open_serial(&port).await,
            config,
        )
        .await
    }
}

#[cfg(feature = "ble")]
impl FourUp<BleTransport> {
    /// Opens four meters by Bluetooth address (e.g. "E8:26:CF:F1:23:61").
    pub async fn open_ble(addresses: &[String], config: Config) -> Result<Self> {
        check_sources("address", addresses, true)?;
        Self::open_with(
            addresses,
            async |address: String| Meter::open_ble(&address).await,
            config,
        )
        .await
    }

    /// Scans for `scan_time` and opens the meters present, requiring
    /// that exactly four are: seen in the scan, or already connected
    /// to this host (a connected meter stops advertising). Meters only
    /// known from the Bluetooth stack's cache (e.g. paired but powered
    /// off) are ignored.
    pub async fn discover_ble(scan_time: Duration, config: Config) -> Result<Self> {
        let seen: Vec<_> = BleTransport::discover(scan_time)
            .await
            .map_err(Error::Discover)?
            .into_iter()
            .filter(|m| m.rssi.is_some() || m.connected)
            .collect();
        if seen.len() != 4 {
            return Err(Error::DiscoverCount { seen });
        }
        let addresses: Vec<String> = seen.into_iter().map(|m| m.address).collect();
        Self::open_ble(&addresses, config).await
    }
}

impl<T: Transport> FourUp<T> {
    /// Opens one meter per source on any transport, pairing each with
    /// its source so errors can say which meter failed. The convenience
    /// constructors use this; call it directly for custom transports.
    ///
    /// Meters are opened one at a time: concurrent LE connection
    /// attempts abort each other in BlueZ
    /// (le-connection-abort-by-local). On failure, meters already
    /// opened are closed before the error returns, so nothing stays
    /// connected.
    ///
    /// A custom transport's `recv` must be cancellation-safe (no data
    /// consumed by a future dropped before completion, as with the
    /// serial and BLE transports): draining races `recv` against a
    /// timeout and drops the loser.
    pub async fn open_with<F, Fut>(sources: &[String], open: F, config: Config) -> Result<Self>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = ut325f_rs::Result<Meter<T>>>,
    {
        config.validate()?;
        check_sources("source", sources, false)?;
        let mut meters: Vec<(String, Meter<T>)> = Vec::with_capacity(sources.len());
        for source_id in sources {
            match open(source_id.clone()).await {
                Ok(meter) => meters.push((source_id.clone(), meter)),
                Err(cause) => {
                    for (_, meter) in meters {
                        let _ = meter.close().await;
                    }
                    return Err(Error::Open {
                        source_id: source_id.clone(),
                        cause,
                    });
                }
            }
        }
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
    ///
    /// Not cancellation-safe: dropping this future mid-flight (e.g.
    /// racing it in `select!` or under an outer timeout) leaves the
    /// meters at uneven stream positions; the next call's drain
    /// usually recovers, but readings in flight are lost.
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
            let readings: Vec<(String, Reading)> = collect_all(maybe_readings)?;

            let timestamps = || readings.iter().map(|(_, r)| r.timestamp);
            let min_timestamp = timestamps().min().expect("four readings");
            let max_timestamp = timestamps().max().expect("four readings");
            let skew = max_timestamp
                .duration_since(min_timestamp)
                .unwrap_or_default();
            if skew > self.config.max_skew {
                self.consecutive_skewed_rows += 1;
                if self.consecutive_skewed_rows > self.config.max_consecutive_skewed_rows {
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
                temps_c: assemble_positions(&readings)?,
            });
        }
    }

    /// Gracefully shuts down all four meters (e.g. disconnecting BLE
    /// devices this session connected). Prefer this over dropping at
    /// the end of a session: cleanup spawned from drop does not
    /// survive runtime shutdown at process exit.
    pub async fn close(self) -> Result<()> {
        let results = futures::future::join_all(self.meters.into_iter().map(
            |(source_id, meter)| async move {
                meter
                    .close()
                    .await
                    .map_err(|cause| Error::Close { source_id, cause })
            },
        ))
        .await;
        collect_all(results).map(|_| ())
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

/// Requires exactly four distinct sources, count checked first so a
/// short list always reports its length regardless of contents.
fn check_sources(kind: &'static str, sources: &[String], ignore_ascii_case: bool) -> Result<()> {
    if sources.len() != 4 {
        return Err(Error::SourceCount(sources.len()));
    }
    check_distinct(kind, sources, ignore_ascii_case)
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

    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A frame with the given current temps (NaN = inactive input),
    /// held temps all inactive, hold type Current.
    fn frame(temps: [f32; 4]) -> Vec<u8> {
        let mut buf = vec![0u8; Reading::N_BYTES];
        buf[..Reading::N_SYNC_BYTES].copy_from_slice(&Reading::SYNC);
        let mut off = Reading::N_SYNC_BYTES;
        for t in temps {
            let value = if t.is_nan() { 0.0 } else { t };
            buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
            off += 4;
        }
        for t in temps {
            buf[off] = u8::from(t.is_nan());
            off += 1;
        }
        for i in 0..4 {
            buf[25 + 16 + i] = 1;
        }
        let sum = buf[..Reading::N_BYTES - 2]
            .iter()
            .fold(0u16, |s, &b| s.wrapping_add(u16::from(b)));
        buf[Reading::N_BYTES - 2..].copy_from_slice(&sum.to_be_bytes());
        buf
    }

    /// Yields each scripted chunk after its delay, then pends forever.
    /// Cancellation-safe: an entry is consumed only once its delay has
    /// fully elapsed within a single `recv` call.
    struct ScriptedTransport {
        script: VecDeque<(Duration, ut325f_rs::Result<Vec<u8>>)>,
    }

    impl Transport for ScriptedTransport {
        async fn recv(&mut self) -> ut325f_rs::Result<Vec<u8>> {
            let Some((delay, _)) = self.script.front() else {
                return std::future::pending().await;
            };
            tokio::time::sleep(*delay).await;
            self.script.pop_front().expect("entry still present").1
        }
    }

    /// One FourUp over scripted transports for sources "m1".."m4".
    async fn fourup_with(
        scripts: [Vec<(Duration, ut325f_rs::Result<Vec<u8>>)>; 4],
        config: Config,
    ) -> FourUp<ScriptedTransport> {
        let sources: Vec<String> = (1..=4).map(|i| format!("m{i}")).collect();
        let transports = Mutex::new(
            scripts
                .into_iter()
                .map(|script| ScriptedTransport {
                    script: script.into(),
                })
                .collect::<VecDeque<_>>(),
        );
        FourUp::open_with(
            &sources,
            |_| {
                let transport = transports
                    .lock()
                    .expect("no poisoning")
                    .pop_front()
                    .expect("four scripts");
                async move { Ok(Meter::new(transport)) }
            },
            config,
        )
        .await
        .expect("open_with succeeds")
    }

    fn now(chunk: Vec<u8>) -> (Duration, ut325f_rs::Result<Vec<u8>>) {
        (Duration::ZERO, Ok(chunk))
    }

    #[tokio::test]
    async fn test_read_row_drains_to_latest() {
        let mut fourup = fourup_with(
            [
                vec![now(frame([10.0, N, N, N])), now(frame([11.0, N, N, N]))],
                vec![now(frame([N, 2.0, N, N]))],
                vec![now(frame([N, N, 3.0, N]))],
                vec![now(frame([N, N, N, 4.0]))],
            ],
            Config::default(),
        )
        .await;
        let row = fourup.read_row().await.expect("row");
        assert_eq!(row.temps_c, [11.0, 2.0, 3.0, 4.0]);
    }

    #[tokio::test]
    async fn test_read_row_names_failed_meter() {
        let mut fourup = fourup_with(
            [
                vec![now(frame([1.0, N, N, N]))],
                vec![now(frame([N, 2.0, N, N]))],
                vec![(Duration::ZERO, Err(ut325f_rs::Error::Disconnected("gone")))],
                vec![now(frame([N, N, N, 4.0]))],
            ],
            Config::default(),
        )
        .await;
        let err = fourup.read_row().await.expect_err("m3 failed").to_string();
        assert!(err.starts_with("m3: "), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_read_row_names_all_failed_meters() {
        let mut fourup = fourup_with(
            [
                vec![now(frame([1.0, N, N, N]))],
                vec![(Duration::ZERO, Err(ut325f_rs::Error::Disconnected("gone")))],
                vec![now(frame([N, N, 3.0, N]))],
                vec![(Duration::ZERO, Err(ut325f_rs::Error::Disconnected("gone")))],
            ],
            Config::default(),
        )
        .await;
        let err = fourup.read_row().await.expect_err("two failed").to_string();
        let lines: Vec<&str> = err.lines().collect();
        assert_eq!(lines.len(), 2, "unexpected error: {err}");
        assert!(lines[0].starts_with("m2: "), "unexpected error: {err}");
        assert!(lines[1].starts_with("m4: "), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_read_row_gives_up_after_misaligned_budget() {
        let config = Config {
            max_skew: Duration::from_millis(50),
            max_consecutive_skewed_rows: 1,
            drain_timeout: Duration::from_millis(10),
            ..Config::default()
        };
        let prompt = |temps| {
            vec![
                now(frame(temps)),
                (Duration::from_millis(200), Ok(frame(temps))),
            ]
        };
        let late = |temps| {
            vec![
                (Duration::from_millis(400), Ok(frame(temps))),
                (Duration::from_millis(400), Ok(frame(temps))),
            ]
        };
        let mut fourup = fourup_with(
            [
                prompt([1.0, N, N, N]),
                prompt([N, 2.0, N, N]),
                prompt([N, N, 3.0, N]),
                late([N, N, N, 4.0]),
            ],
            config,
        )
        .await;
        let err = fourup.read_row().await.expect_err("misaligned");
        assert!(
            matches!(err, Error::Misaligned { rows: 2, .. }),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_collect_all_reports_all_errors() {
        let results: Vec<Result<()>> = vec![
            Err(Error::NoActiveInput {
                source_id: "a".to_owned(),
            }),
            Ok(()),
            Err(Error::MissingPosition { position: 2 }),
        ];
        let err = collect_all(results).unwrap_err().to_string();
        assert_eq!(
            err,
            "Meter a: no active input.\nNo meter reported position 2."
        );
    }

    #[cfg(feature = "serial")]
    async fn no_open(_: String) -> ut325f_rs::Result<Meter<SerialTransport>> {
        unreachable!("open must not be called for an invalid config");
    }

    #[cfg(feature = "serial")]
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
    fn test_check_sources_count_before_duplicates() {
        let sources = ["a".to_owned(), "a".to_owned()];
        assert!(matches!(
            check_sources("port", &sources, false),
            Err(Error::SourceCount(2))
        ));
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
