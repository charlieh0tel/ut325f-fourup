# ut325f-fourup

Supports running four Uni-T UT325F 4-channel temperature meters
simultaneously.

Each meter should use one thermocouple.  The input position for each
meter determines the column used in the CSV output.  The positions must
be unique.  The port order does not matter.

(These are four channel meters but the inputs are not galvanically
isolated, hence one meter per thermocouple.)

## Usage

Serial (four USB serial ports):

```
ut325f-fourup /dev/ttyUSB0 /dev/ttyUSB1 /dev/ttyUSB2 /dev/ttyUSB3
```

Bluetooth LE, by address:

```
ut325f-fourup --ble E8:26:CF:F1:23:61 E8:26:CF:F1:23:62 E8:26:CF:F1:23:63 E8:26:CF:F1:23:64
```

Bluetooth LE, by discovery (requires that the scan see exactly four
meters; paired-but-absent meters are ignored):

```
ut325f-fourup --ble
```

List meters visible over Bluetooth LE and exit:

```
ut325f-fourup --discover
```

`--scan-time SECONDS` (default 8) sets the scan duration for
`--discover` and `--ble` without addresses.

## Output

One CSV row per synchronized sample set, about three per second, on
stdout:

```
timestamp,temp1,temp2,temp3,temp4
```

- `timestamp` — seconds, three decimal places.  By default this is
  absolute UNIX time (wall clock) of the earliest reading in the row.
  With `--relative-timestamps`/`-z` it is elapsed time since the first
  row, from a monotonic clock (starts at ~0, never goes backward).
- `temp1`..`temp4` — degrees Celsius from the meter whose thermocouple
  occupies that input position.
- There is no header row.

The four readings in a row are the freshest frame from each meter and
are guaranteed to lie within one second of one another; misaligned
sample sets are discarded and re-read.

The program exits with an error naming the offending meter if one
disconnects, times out, has no or several active inputs, or if two
meters use the same input position.  Output ends cleanly when the
consumer closes the pipe (e.g. `... | head`).

## Bluetooth prerequisites

BLE uses the btleplug backend.  On Linux this talks to BlueZ over
D-Bus; the Bluetooth adapter must be powered.  Meters do not need
prior pairing to be discovered, but must be on and in range.
