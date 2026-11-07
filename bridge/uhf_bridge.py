#!/usr/bin/env python3
"""JSON-lines bridge between the hipims-qcm Rust service and a Zurich Instruments UHF.

Requirements (on the lab PC, next to LabOne):   pip install zhinst numpy

Protocol (one JSON object per line on stdin / stdout; logs go to stderr):
    -> {"id": 7, "op": "set", "args": {...}}
    <- {"id": 7, "ok": true, "result": ...}   |   {"id": 7, "ok": false, "error": "..."}

Long operations (sweeper, DAQ module, PID advisor) are split into
<name>_start / <name>_progress / <name>_read / <name>_stop so the Rust side can
cancel them and keep strict per-call timeouts.

All API usage follows the official examples of the LabOne API
(github.com/zhinst/labone-api-examples: example_sweeper, example_poll,
example_data_acquisition_edge, example_pid_advisor_pll).
"""
import json
import os
import sys
import time
import traceback

# Keep the protocol channel clean: anything libraries print goes to stderr.
_proto = sys.stdout
sys.stdout = sys.stderr


def send(obj):
    _proto.write(json.dumps(obj, allow_nan=False) + "\n")
    _proto.flush()


def log(msg):
    sys.stderr.write(str(msg) + "\n")
    sys.stderr.flush()


try:
    import numpy as np
except Exception as exc:  # pragma: no cover
    np = None
    _np_error = exc

S = {
    "daq": None,
    "dev": None,
    "clockbase": 60e6,
    "stream_path": None,
    "sweeper": None,
    "sweep_path": None,
    "gated": None,
    "gated_paths": None,
    "gated_window": 0.0,
    "advisor": None,
}


class BridgeError(Exception):
    pass


def need_daq():
    if S["daq"] is None:
        raise BridgeError("not connected (call connect first)")
    return S["daq"]


def clean(arr):
    """numpy array -> list of floats; refuse NaN/inf (JSON cannot carry them)."""
    a = np.asarray(arr, dtype=float).ravel()
    if not np.all(np.isfinite(a)):
        raise BridgeError("non-finite values in instrument data")
    return a.tolist()


# --------------------------------------------------------------------- connect


def op_connect(a):
    if np is None:
        raise BridgeError("numpy is not installed: pip install numpy (%s)" % _np_error)
    try:
        import zhinst.utils as zu
    except Exception as exc:
        raise BridgeError("zhinst is not installed: pip install zhinst (%s)" % exc)
    # API level 6 is the level used by all UHF examples.
    daq, device, props = zu.create_api_session(
        a["device"], 6, server_host=a.get("host", "localhost"), server_port=int(a.get("port", 8004))
    )
    S["daq"], S["dev"] = daq, device
    try:
        S["clockbase"] = float(daq.getInt("/%s/clockbase" % device))
    except Exception:
        S["clockbase"] = 60e6
    opts = props.get("options", "")
    if isinstance(opts, str):
        opts = [o for o in opts.replace("\n", "/").split("/") if o]
    fw = ""
    try:
        fw = str(daq.getInt("/%s/system/fwrevision" % device))
    except Exception:
        pass
    return {
        "device": device,
        "devtype": str(props.get("devicetype", "")),
        "serial": device,
        "options": [str(o) for o in opts],
        "firmware": fw,
    }


# --------------------------------------------------------------------- nodes


def op_set(a):
    daq = need_daq()
    node, value = a["node"], a["value"]
    if a.get("type") == "i":
        daq.setInt(node, int(value))
    else:
        daq.setDouble(node, float(value))
    return None


def op_get(a):
    daq = need_daq()
    node = a["node"]
    # daq.get returns the dtype of the node (int64 vs float64) -> tells int from double.
    try:
        d = daq.get(node, True)
        key = next(iter(d))
        entry = d[key]
        val = entry["value"] if isinstance(entry, dict) else entry
        v = np.asarray(val).ravel()[0]
        if np.issubdtype(np.asarray(val).dtype, np.integer):
            return {"type": "i", "value": float(v)}
        return {"type": "d", "value": float(v)}
    except Exception as exc:
        log("get via daq.get failed (%s); falling back to getDouble" % exc)
        return {"type": "d", "value": float(daq.getDouble(node))}


def op_sync(_a):
    need_daq().sync()
    return None


def op_read_sample(a):
    daq = need_daq()
    s = daq.getSample("/%s/demods/%d/sample" % (S["dev"], int(a["demod"])))
    g = lambda k: float(np.asarray(s[k]).ravel()[0])
    return {
        "t": g("timestamp") / S["clockbase"],
        "x": g("x"),
        "y": g("y"),
        "frequency": g("frequency"),
        "phase": g("phase"),
    }


# --------------------------------------------------------------------- sweeper


def op_sweep_start(a):
    daq = need_daq()
    dev = S["dev"]
    sw = daq.sweep()
    sw.set("device", dev)
    sw.set("gridnode", "oscs/%d/freq" % int(a["osc"]))
    sw.set("start", float(a["start_hz"]))
    sw.set("stop", float(a["stop_hz"]))
    sw.set("samplecount", int(a["points"]))
    sw.set("xmapping", 0)  # linear
    sw.set("scan", 0)  # sequential
    sw.set("loopcount", 1)
    sw.set("bandwidthcontrol", 1)  # fixed bandwidth
    sw.set("bandwidth", float(a["bandwidth_hz"]))
    sw.set("bandwidthoverlap", 0)
    sw.set("order", 4)
    sw.set("maxbandwidth", max(1250.0, float(a["bandwidth_hz"]) * 2))
    sw.set("settling/time", 0)
    sw.set("settling/inaccuracy", float(a["settle_inaccuracy"]))
    sw.set("averaging/tc", float(a["averaging_tc"]))
    sw.set("averaging/sample", 1)
    sw.set("phaseunwrap", 0)
    path = "/%s/demods/%d/sample" % (dev, int(a["demod"]))
    sw.subscribe(path)
    sw.execute()
    S["sweeper"], S["sweep_path"] = sw, path
    return None


def op_sweep_progress(_a):
    sw = S["sweeper"]
    if sw is None:
        raise BridgeError("no sweep running")
    return {"progress": float(np.asarray(sw.progress()).ravel()[0]), "finished": bool(sw.finished())}


def op_sweep_read(_a):
    sw, path = S["sweeper"], S["sweep_path"]
    if sw is None:
        raise BridgeError("no sweep to read")
    data = sw.read(True)
    try:
        sw.unsubscribe(path)
    except Exception:
        pass
    S["sweeper"] = None
    if not data or path not in data:
        raise BridgeError("the sweeper returned no data for %s (demodulator disabled / rate 0?)" % path)
    s0 = data[path][0][0]
    return {"freq": clean(s0["frequency"]), "x": clean(s0["x"]), "y": clean(s0["y"])}


def op_sweep_stop(_a):
    sw = S["sweeper"]
    if sw is not None:
        try:
            sw.finish()
            sw.unsubscribe(S["sweep_path"])
        except Exception as exc:
            log("sweep stop: %s" % exc)
    S["sweeper"] = None
    return None


# --------------------------------------------------------------------- streaming


def op_stream_start(a):
    daq = need_daq()
    dev, d = S["dev"], int(a["demod"])
    daq.setDouble("/%s/demods/%d/rate" % (dev, d), float(a["rate"]))
    daq.setInt("/%s/demods/%d/enable" % (dev, d), 1)
    daq.unsubscribe("*")
    daq.sync()
    path = "/%s/demods/%d/sample" % (dev, d)
    daq.subscribe(path)
    S["stream_path"] = path
    return None


def op_stream_poll(a):
    daq = need_daq()
    path = S["stream_path"]
    if path is None:
        raise BridgeError("stream not started")
    window = float(a["window_s"])
    data = daq.poll(window, 500, 0, True)
    key = None
    for k in (data or {}):
        if k.lower() == path.lower():
            key = k
    if key is None:
        return {"t": [], "x": [], "y": [], "frequency": [], "phase": []}
    s = data[key]
    t = np.asarray(s["timestamp"], dtype=float) / S["clockbase"]
    return {
        "t": clean(t),
        "x": clean(s["x"]),
        "y": clean(s["y"]),
        "frequency": clean(s["frequency"]),
        "phase": clean(s["phase"]),
    }


def op_stream_stop(_a):
    daq = need_daq()
    if S["stream_path"] is not None:
        try:
            daq.unsubscribe(S["stream_path"])
        except Exception as exc:
            log("unsubscribe: %s" % exc)
    S["stream_path"] = None
    return None


# --------------------------------------------------------------------- gated DAQ


def op_gated_start(a):
    """Hardware-triggered Data Acquisition module: the HiPIMS measurement window is wired
    to a trigger input; each rising edge records `window_s` of demodulator X/Y."""
    daq = need_daq()
    dev, d = S["dev"], int(a["demod"])
    m = daq.dataAcquisitionModule()
    m.set("device", dev)
    m.set("type", 6)  # 6 = hardware trigger
    m.set("triggernode", "/%s/demods/%d/sample.%s" % (dev, d, a["trigger_signal"]))
    m.set("edge", 1)  # rising
    m.set("delay", float(a["delay_s"]))
    m.set("duration", float(a["window_s"]))
    m.set("grid/mode", 2)  # linear interpolation onto the grid
    m.set("grid/cols", int(a["points"]))
    m.set("grid/rows", 1)
    m.set("grid/repetitions", 1)
    m.set("count", int(a["pulses"]))
    m.set("holdoff/count", 0)
    m.set("holdoff/time", 0.0)
    m.set("endless", 0)
    paths = ["/%s/demods/%d/sample.x" % (dev, d), "/%s/demods/%d/sample.y" % (dev, d)]
    for p in paths:
        m.subscribe(p)
    m.execute()
    S["gated"], S["gated_paths"], S["gated_window"] = m, paths, float(a["window_s"])
    return None


def op_gated_progress(_a):
    m = S["gated"]
    if m is None:
        raise BridgeError("no gated acquisition running")
    return {"progress": float(np.asarray(m.progress()).ravel()[0]), "finished": bool(m.finished())}


def _stack(segments):
    rows = []
    for seg in segments:
        v = np.asarray(seg["value"], dtype=float)
        rows.append(v.reshape(-1, v.shape[-1]) if v.ndim >= 2 else v.reshape(1, -1))
    return np.concatenate(rows, axis=0)


def op_gated_read(_a):
    m, paths = S["gated"], S["gated_paths"]
    if m is None:
        raise BridgeError("no gated acquisition to read")
    data = m.read(True)
    for p in paths:
        try:
            m.unsubscribe(p)
        except Exception:
            pass
    S["gated"] = None
    out = {}
    for k, v in (data or {}).items():
        kl = k.lower()
        if kl.endswith("sample.x") and v:
            out["x"] = _stack(v)
        elif kl.endswith("sample.y") and v:
            out["y"] = _stack(v)
    if "x" not in out or "y" not in out:
        raise BridgeError("no triggered data received: is the measurement window connected to the trigger input?")
    n = min(out["x"].shape[0], out["y"].shape[0])
    X, Y = out["x"][:n], out["y"][:n]
    cols = X.shape[1]
    t = np.arange(cols) * (S["gated_window"] / cols)
    return {"t": clean(t), "x": clean(X.mean(axis=0)), "y": clean(Y.mean(axis=0)), "pulses": int(n)}


def op_gated_stop(_a):
    m = S["gated"]
    if m is not None:
        try:
            m.finish()
            for p in S["gated_paths"] or []:
                m.unsubscribe(p)
        except Exception as exc:
            log("gated stop: %s" % exc)
    S["gated"] = None
    return None


# --------------------------------------------------------------------- PID advisor


def op_advisor_start(a):
    daq = need_daq()
    adv = daq.pidAdvisor()
    adv.set("device", S["dev"])
    adv.set("auto", False)
    adv.set("pid/targetbw", float(a["target_bw_hz"]))
    adv.set("pid/mode", 7)  # tune P, I and D
    adv.set("index", int(a["pid_index"]))
    src = int(a["dut_source"])
    adv.set("dut/source", src)
    adv.set("dut/delay", 0.0)
    if src == 3:  # resonator frequency model
        adv.set("dut/fcenter", float(a["f0_hz"]))
        adv.set("dut/q", float(a["q_factor"]))
    adv.set("pid/p", 0)
    adv.set("pid/i", 0)
    adv.set("pid/d", 0)
    adv.execute()
    adv.set("calculate", 1)
    S["advisor"] = adv
    return None


def op_advisor_progress(_a):
    adv = S["advisor"]
    if adv is None:
        raise BridgeError("no advisor run")
    calc = adv.getInt("calculate")
    return {"progress": float(np.asarray(adv.progress()).ravel()[0]), "finished": calc == 0}


def op_advisor_read(_a):
    adv = S["advisor"]
    if adv is None:
        raise BridgeError("no advisor result")
    res = adv.get("*", True)
    if not res:
        raise BridgeError("the PID advisor returned an empty result")
    g = lambda k: float(np.asarray(res[k][0]).ravel()[0])
    out = {"p": g("/pid/p"), "i": g("/pid/i"), "d": g("/pid/d"), "bw": g("/bw")}
    try:
        adv.finish()
    except Exception:
        pass
    S["advisor"] = None
    return out


def op_advisor_stop(_a):
    adv = S["advisor"]
    if adv is not None:
        try:
            adv.finish()
        except Exception as exc:
            log("advisor stop: %s" % exc)
    S["advisor"] = None
    return None


# --------------------------------------------------------------------- settings (XML)


def _settings(a, command):
    daq = need_daq()
    path = os.path.abspath(a["path"])
    folder, name = os.path.split(path)
    stem = name[:-4] if name.lower().endswith(".xml") else name
    st = daq.deviceSettings()
    st.set("device", S["dev"])
    st.set("command", command)
    st.set("filename", stem)
    st.set("path", folder)
    st.execute()
    t0 = time.time()
    while not st.finished():
        if time.time() - t0 > 25:
            raise BridgeError("device settings %s timed out" % command)
        time.sleep(0.1)
    return None


def op_load_settings(a):
    return _settings(a, "load")


def op_save_settings(a):
    return _settings(a, "save")


def op_close(_a):
    for k in ("sweeper", "gated", "advisor"):
        S[k] = None
    return None


OPS = {name[3:]: fn for name, fn in list(globals().items()) if name.startswith("op_")}


def main():
    log("uhf_bridge ready (python %s)" % sys.version.split()[0])
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
        except Exception as exc:
            log("bad request: %s" % exc)
            continue
        rid = msg.get("id")
        try:
            fn = OPS.get(msg.get("op"))
            if fn is None:
                raise BridgeError("unknown op %r" % msg.get("op"))
            send({"id": rid, "ok": True, "result": fn(msg.get("args") or {})})
        except Exception as exc:
            log(traceback.format_exc())
            send({"id": rid, "ok": False, "error": "%s: %s" % (type(exc).__name__, exc)})
    log("stdin closed, bridge exiting")


if __name__ == "__main__":
    main()
