# TODO

- Decide how a long-running session should handle meter read failures.
  Today a single transient read error (timeout, USB glitch, BLE dropout)
  ends the entire run. Options considered:
  - Retry rows only (~10 lines): tolerate N consecutive failed rows while
    the transport survives; a disconnect is still fatal.
  - Retry + reconnect (~50-70 lines): reopen a failed meter by its source
    (port/address). Each meter is now paired with its source and `main`
    passes an open function into `open_all`, so the remaining work is
    threading that open function into `run()` and choosing a
    backoff/attempt policy. Matters most for unattended BLE sessions,
    where disconnects are the common failure.
