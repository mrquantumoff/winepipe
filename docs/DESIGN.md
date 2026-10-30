# WinePipe design

WinePipe is an ASIO driver for Wine that talks to PipeWire directly. It does
the same job as WineASIO, but there is no JACK layer (or `pw-jack` shim) in
between, and it is built for Wine's current PE/Unix split DLL model (Wine
10.2+ / 11, including WoW64 for 32-bit hosts).

## Goals

1. **Low latency.** In the common case (graph quantum == ASIO buffer size),
   input captured in PipeWire cycle *k* is handed to the host and its output
   goes back into the graph in the **same** cycle *k*. That's the same
   behaviour as JACK/WineASIO. The driver adds no buffering of its own.
2. **Stability.** A Windows application that stalls, crashes or is
   descheduled must never stall the PipeWire graph. Every wait on the
   realtime path has a hard deadline. If the host is late, it gets silence,
   the xrun is counted, and the graph keeps running.
3. **No polling.** The WineASIO Wine 11 port polls with `Sleep(1)`, which
   adds up to a millisecond of jitter per period. WinePipe uses futex
   handoffs (one unix call per period, no sleeps, no busy loops).
4. **Works for 64-bit and 32-bit (WoW64) hosts.** One 64-bit Unix library
   serves both, because the parameter structs are layout-identical on both
   PE architectures.

## Components

```
 Windows host (DAW)                       PipeWire graph
 ───────────────────                      ───────────────────────────────
 IASIO COM object  ─┐                      pw_filter "WinePipe"
  winepipe.dll (PE) │ __wine_unix_call      in_1..in_N  out_1..out_M
  Rust, *-pc-windows-gnu                   (32-bit float DSP ports)
                    │                               ▲
 callback thread ◀──┼── futex handoff ──▶ process() on PipeWire RT data loop
 (calls bufferSwitch)                              │
                    └──▶ winepipe.so (Unix) ───────┘
                          Rust, x86_64-unknown-linux-gnu, links libpipewire
```

* `crates/abi` (`winepipe-abi`, `no_std`): the unix-call function IDs and
  `#[repr(C)]` parameter structs. It uses only fixed-width fields, pointers
  are passed as `u64`, and size asserts run at compile time. The same structs
  work for x86_64 PE, i686 PE and the x86_64 Unix side, which is why the
  WoW64 table (`__wine_unix_call_wow64_funcs`) can reuse the 64-bit entry
  points with no thunking.
* `crates/pe` (`winepipe.dll`): the COM in-proc server. Implements `IASIO`
  (thiscall on i686, win64 ABI on x86_64), the class factory,
  `DllRegisterServer`/`DllUnregisterServer`, registry configuration, and the
  ASIO callback thread. It resolves `__wine_unix_call_dispatcher` from ntdll
  at runtime and loads its Unix half with
  `NtQueryVirtualMemory(MemoryWineLoadUnixLib)`. This is what winecrt0's
  `__wine_init_unix_call` does, so no Wine import libraries are needed.
* `crates/unix` (`winepipe.so`): the PipeWire engine. A `pw_thread_loop`
  runs the control plane (registry, metadata, autoconnect, properties).
  A `pw_filter` with `PW_FILTER_FLAG_RT_PROCESS` provides the data plane.
* `crates/asiotest` (`winepipe-test.exe`): a minimal ASIO host used for
  end-to-end testing and latency/jitter measurement under Wine.
* `crates/settings` (`winepipe-settings`): the libcosmic settings app. It
  reads a prefix's `user.reg` directly and writes through `wine reg import`.
  The driver's ASIO control panel (`CONTROL_PANEL` unix call) spawns it
  detached, with `--prefix` and `--wine` set from the calling process
  (`WINEPREFIX`, and the Wine root derived from `/proc/self/exe`), and with
  Wine's file descriptors closed.

## Realtime path

PipeWire calls `process()` on its realtime data thread. That thread is a
plain pthread, so it can't run Windows code. Host callbacks have to run on
a Wine thread. WinePipe keeps a dedicated **callback thread** on the PE
side. It spends its life blocked inside a single unix call (`WAIT`) on a
futex. The Unix side promotes it to `SCHED_FIFO` through PipeWire's
`ThreadUtils` (module-rt: direct RT scheduling or RTKit).

Two futex words carry the handshake. `req_seq` is written by the RT thread
and `done_seq` by the callback thread.

```
RT thread (process)                         callback thread (PE)
──────────────────                          ────────────────────
copy ports → ASIO in[idx]                    ...blocked in WAIT(last_done)
publish idx/time; req_seq++ ; wake ───────▶  returns SWITCH(seq, idx, time)
wait done_seq == seq (deadline)               host->bufferSwitchTimeInfo(idx)
                                              (host may call outputReady():
                                               done_seq = seq; wake)
              ◀──────────────────────────── WAIT(last_done = seq):
copy ASIO out[idx] → ports                     done_seq = seq; wake; block
```

That's one unix call per period. The RT thread blocks only until the
**cycle deadline**, which is computed from `spa_io_clock.next_nsec` (the
driver's next wakeup) minus a safety margin, and clamped to
`[now + period/8, now + period]`. If the host misses it:

* the RT thread writes silence for that block and counts a late xrun;
* while the host is still busy (`done_seq != req_seq`) no new switch is
  issued, so the host and the RT thread never touch the same half-buffer;
* once the host catches up, the next cycle continues normally.

Stopping sets the state to `STOPPING`, wakes both futexes, and waits (one
period at most) for any in-flight `process()` to leave the session before
buffers are released. So `disposeBuffers()` can never free memory the RT
thread is still using.

## Buffer size vs. graph quantum

`createBuffers(B)` sets `node.force-quantum = B` (plus `node.latency`), the
same as `pw-jack`'s `jack_set_buffer_size`. That way the graph normally runs
at exactly B frames. If something else wins (another node forcing a
quantum, or B outside `clock.min/max-quantum`), the driver adapts:

* **n is a multiple of B** (including n == B): direct path. The cycle is
  split into n/B consecutive bufferSwitches, all inside the same cycle. No
  added latency.
* **otherwise**: FIFO path. Ring buffers, with the output pre-filled by
  `P = B − gcd(n, B)` frames of silence. That's the provably minimal
  pre-fill that never underruns. The extra latency P is added to the
  reported output latency, and the host gets `kAsioLatenciesChanged`.

## Control plane

* **Sample rate**: from the `settings` metadata (`clock.force-rate` or
  `clock.rate`), and from the live `spa_io_position` once running.
  `canSampleRate` accepts the current rate and anything in
  `clock.allowed-rates`. `setSampleRate` sets `node.force-rate`. If the
  graph rate changes while running, the host gets `sampleRateDidChange`.
* **Autoconnect**: on `start()`, outputs are linked to the default sink and
  inputs to the default source. The defaults come from the `default`
  metadata, falling back to the highest `priority.session` sink/source. Links
  go through `link-factory`, don't linger, and are owned by the driver, so
  they disappear with it.
* **Scheduling while unlinked**: `node.always-process = true`, so the host
  keeps getting callbacks even with nothing connected (JACK semantics).
* **Server loss**: if the core reports an error or disconnects, the host
  gets `kAsioResetRequest`. The callback thread never waits forever (it
  wakes every 200 ms to check state).

## Configuration

Read on `init()` from `HKCU\Software\WinePipe` (edited by `winepipe-settings` or `winepipe-config`). Environment variables
(`WINEPIPE_*`) override the registry, which is handy for testing.

| Value                 | Type   | Default   | Meaning                                   |
|-----------------------|--------|-----------|-------------------------------------------|
| `Inputs`              | DWORD  | 2         | number of input channels (0–64)           |
| `Outputs`             | DWORD  | 2         | number of output channels (0–64)          |
| `PreferredBufferSize` | DWORD  | 256       | buffer size suggested to the host         |
| `FixedBufferSize`     | DWORD  | 0         | 1 = only allow `PreferredBufferSize`      |
| `AutoConnect`         | DWORD  | 1         | link to default sink/source on start      |
| `SampleType`          | DWORD  | 0         | 0 = Float32LSB, 1 = Int32LSB              |
| `ClientName`          | SZ     | WinePipe  | PipeWire node name                        |
