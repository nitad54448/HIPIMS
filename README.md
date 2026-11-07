# HIPIMS: QCM tracking, Q-control and HiPIMS-gated acquisition (Zurich Instruments UHF)

A Rust service plus browser UI that replaces the LabVIEW *QCM_PLL* program: resonance
sweep with Lorentz / circle fit, PLL lock with PID Advisor, active Q-control, hardware-gated
HiPIMS acquisition, Sauerbrey / Z-match analysis, projects and reports.

```text
browser UI (static HTML/JS)  <--HTTP/WebSocket, localhost + token-->  Rust service
                                                                        |  typed messages
                                                                  instrument actor (1 thread)
                                                                        |  trait NodeIo
                                          +-----------------------------+---------------------+
                                     PyBridge (JSON lines)                                  SimUhf
                                     bridge/uhf_bridge.py                          (physics simulation)
                                     zhinst.core -> LabOne Data Server :8004
```

## Quick start

Double-click **`start.bat`** (or `start_sim.bat` for the simulator).

* `start.bat --sim` / `start_sim.bat`: built-in simulator, no instrument and no LabOne needed.
* `start.bat`: real UHF. On the first start it creates `config.toml` from `config/default.toml`, builds
  the executable if Rust is installed and no `hipims-qcm.exe` is next to it, and installs the Python
  prerequisites (`pip install zhinst numpy`) if they are missing. Any option is passed on
  (`start.bat --port 9000 --no-browser`).

On the lab PC (LabOne installed, same PC as the instrument):

1. Edit `config.toml`: set `device.id` (e.g. `dev2091`) and, if needed, `bridge.python` (`py` on some
   Windows installs).
2. **Verify the PID wiring** in `[pid]` / `[routing]` against the LabOne node tree
   (see *Not verified on hardware* below).
3. Run `start.bat`, open the printed URL (it carries the session token; the browser opens by itself).
   The **Exit** button in the top bar stops the service safely; Ctrl-C does the same.
   If the window was closed improperly, `stop.bat` ends a leftover service (a forced stop - see
   the note in the file about the outputs) and frees the port for the next start.

The executable is self-contained: web UI and the Python bridge are embedded. Unknown or stale keys in
`config.toml` are rejected at start-up.

## Workflow (tabs)

| Tab | What it does |
|---|---|
| 1 Experiment | connect / disconnect, load/save LabOne `.xml` settings, project (sample ID, operator, notes, save/load) |
| 2 Sweep | Sweeper-module scan around ~6 MHz (50 Ω divider compensation), **Lorentz** fit (R) and **circle** fit (Nyquist); you choose which one feeds the loops; manual override; slope polarity (normal / inverted) |
| 3 PLL lock | PID Advisor (resonator model) or local model; gains get the **sign from the measured polarity** (normal ⇒ negative P, I; inverted ⇒ positive); lock detection (phase error + rail check); continuous **data streaming** to a CSV track |
| 4 Q control | Q-control of the Zurich Instruments blog (X and Y of the response fed back at 0° / 90°, `P = −gain`): calibration by sweeps of the *effective* resonance at several gains, lasing guard, gain for a target Q, PLL gain rescaling by `Q_native/Q_target` and the blog's rule *PLL bandwidth > Q-control bandwidth*, optional AGC |
| 5 HiPIMS | gated acquisition per window position (principle of `Principe_mesureQCM-ZI.pdf`); cumulative Δf vs overlap with the pulse |
| 6 Analysis | Sauerbrey / Z-match thickness, rate, mass between two cursors on a recorded track; ion-flux fraction; append-only revisions |
| 7 Log | timestamped session log (copy/export), tracing level switch (instrument traffic at `debug`), project history |

Top bar (always visible): connection, busy/idle + progress + **Cancel**, PLL lock LED, saved/unsaved,
last message, **Arm outputs** (anything that drives hardware needs it; auto-disarms after 60 s),
**STOP** (emergency stop), theme, **Exit**.

## Safety model

* The **service owns the hardware**. Limits (`[limits]`) are validated in the driver before every
  write; the UI only mirrors them.
* `Uhf::safe_state()` switches off both outputs, all PIDs, zeroes the Q-feedback gain and stops
  streams; every step is attempted even if one fails, then the output state is **read back**. It runs on
  `emergency_stop()` (STOP button / priority channel / flag polled by every long job), on Exit/Ctrl-C,
  on thread unwind (`Drop`), and from the panic hook.
* Every bridge call has a strict timeout; a timeout poisons the session (fail fast afterwards).
* Q-calibration skips gains beyond 90 % of the lasing threshold, aborts if the response saturates (no
  resonance peak) or the effective bandwidth collapses, refuses to run while the PLL is on, and always
  returns to gain 0 / Q loops off / feedback channels 0. The PID outputs of the Q loops are limited to
  `q_amp_max_v`.
* Localhost only, `Host`/`Origin` checks, random session token (never stored in projects).

## Data and provenance

* `data/<project>/project.json` (atomic write; a failed save keeps the *unsaved* state) with software
  version, git hash (`HQCM_GIT_HASH` at build time), instrument identity, the full TOML configuration,
  all sweeps, calibrations, HiPIMS steps, analyses (append-only revisions) and history.
* `data/<project>/tracks/track_*.csv`: `# key: value` header (sample, operator, device, resonance, PLL
  gains, rates), then `t_s` (instrument clock, monotonic), `wall_unix_s`, `freq_hz`, `x_v`, `y_v`,
  `r_v`, `phase_deg`. Flushed after every batch; tracks not closed properly are flagged *partial*.
* `GET /api/report`: printable HTML report with inline SVG plots (Print → Save as PDF).
* The reference *Sauerbrey* tool reads the 2-column form; `t_s` and `freq_hz` are columns 1 and 3 here,
  the loader of this program accepts both.

## Physics used

* Resonator: `R(f) = c + A / sqrt(1 + ((f - f0)/γ)²)`, `BW = 2γ`, `Q = f0 / BW`.
* Q-control (blog): the feedback adds the response back in phase, so a Lorentzian only changes its width,
  `BW_eff = BW_native (1 − k·g)`. With `Γ = π·BW` the calibration fits `Γ(g) = Γ_native + m·g` with the
  **signed slope `m` fitted** (no sign is assumed): `g_target = (π f0/Q_target − Γ_native)/m`,
  lasing at `g_las = −Γ_native/m`. Cross-check with the blog formula `Q = f0/2 · dθ/df` (column
  *Q (phase slope)*). The feedback demodulator filter (`q_demod_tc_s`, order) sets the Q-control
  bandwidth `sqrt(2^(1/n) − 1)/(2π·TC)`; the PLL must be faster.
* HiPIMS: drive held at `f0`, `δf0 = ± γ tan(Δθ)`, `γ = f0/(2Q)` (valid for a Lorentzian resonance;
  |Δθ| > 80° is refused). Overlap = intersection of the window with the pulse `[0, W]`.
* Thickness: Sauerbrey and Z-match (Lu & Lewis) exactly as in `sauerbrey.html`
  (`ρq = 2.648 g/cm³`, `μq = 2.947e11 g/(cm s²)`, Z-match limit 40 %); IFF = `1 − v_neutral / v_total`.

## LabVIEW → Rust mapping

| LabVIEW | here |
|---|---|
| producer/consumer loops, queues, notifiers | instrument actor + `flume` channels + `broadcast` events |
| state machine / sequencer | request enum handled by the actor loop; `LockState`, typed `Request`/`Reply` |
| parallel TDMS logger | incremental CSV writer (open format) flushed per batch |
| front panel | browser UI (tabs, arm-then-fire, status strip) |
| Quit button | **Exit** (`POST /api/shutdown`): safe state, final save, process exit |

## Tests

```bash
cargo test                      # 60 unit + 5 HTTP + bridge-contract tests (simulator, virtual time)
cargo clippy --all-targets -- -D warnings
python bridge/test_bridge.py    # bridge logic against a fake zhinst (needs numpy)
HQCM_TEST_PYTHON=python cargo test --test bridge_script   # real script <-> real Rust driver, fake zhinst
cargo build && node tests/ui_e2e.mjs --shots shots        # real service + headless Edge/Chrome, all tabs
```

## Not verified on hardware (read before the first real run)

Developed and tested here **without a UHF and without LabOne**. Verified: all maths against synthetic
data, the driver/actor/server against the simulator, the Rust↔Python JSON contract and the bridge logic
against a *fake* `zhinst`, and the UI in a real browser. **Not verified**, because it needs the instrument:

1. `[pid]` enumerations and `pids/n/*` node names (`input`, `output`, `center`, `limitlower/upper`,
   `phaseunwrap`), whether `/pids/n/mode` must be set for PLL operation (`pll_mode_value`), and that for a
   signal-output-amplitude PID `outputchannel` is the amplitude channel (as in the blog's script); the
   optional PID-internal `pids/n/demod/*` nodes (written best-effort) may not exist on the UHF.
   The blog script was written for an MFLI: the UHF amplitude-channel / demodulator correspondence
   (drive = channel 1, Q-X = 2, Q-Y = 3, demods 3 and 4 at 0° / 90°) must match your instrument.
2. The DAQ hardware trigger: `type = 6`, `triggernode = /dev/demods/N/sample.<gate_trigger_signal>`
   (`TrigIn1` is a guess), grid/duration semantics, and the shape of `daqModule.read()` for triggered
   segments.
3. PID-Advisor parameter names for the resonator model (`dut/fcenter`, `dut/q`) and the sign convention
   of the returned gains (the code uses magnitudes and applies the polarity sign itself).
4. Sweeper bandwidth settings for a ~120 Hz-wide resonance and `settling/inaccuracy`.
5. Real timing of `pid_settle_s`, stream rates, the stability of the Q loops with a real crystal
   (gain range, `q_demod_tc_s`), and the quality of the effective-resonance fits near lasing.
6. The `getSample` / `poll` field names (`timestamp`, `x`, `y`, `frequency`, `phase`) follow the official
   examples; a firmware that returns another layout will fail with an explicit error, not wrong data.

Start with the simulator, then: connect → `drive 0.05 V` sweep → inspect the node tree in LabOne next to the
service log (`Tracing level = debug` shows every node write) → PLL lock → only then Q-control and HiPIMS.

## Design decisions

* **Python bridge instead of Rust FFI to ziAPI**: there is no official Rust binding, the streaming C
  structs are version-dependent and cannot be validated without hardware, and Sweeper / DAQ / PID Advisor
  modules are exposed by `zhinst.core`. The `NodeIo` trait isolates the choice: a native FFI transport can
  replace `PyBridge` later without touching routines, actor, UI or tests.
* **Single actor thread with message passing** rather than `Arc<Mutex<Uhf>>`; the only shared state is the
  project (one async mutex) and the UI event bus.
* **Virtual-time simulator** so the whole chain (sweep → fit → PLL → Q → HiPIMS) runs in CI in seconds.
