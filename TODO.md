# TODO

- Decide how a long-running session should handle meter read failures.
  Today a single transient read error (timeout, USB glitch, BLE dropout)
  ends the entire run. Options considered:
  - Retry rows only (~10 lines): tolerate N consecutive failed rows while
    the transport survives; a disconnect is still fatal.
  - Retry + reconnect (~50-70 lines): reopen a failed meter by its source
    (port/address); requires passing sources and an open function into
    `run()`, plus backoff/attempt policy. Overlaps with the "errors should
    name the failing meter" fix, which needs the same source plumbing.
    Matters most for unattended BLE sessions, where disconnects are the
    common failure.
