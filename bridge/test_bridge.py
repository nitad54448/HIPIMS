"""Offline test of uhf_bridge.py against a fake `zhinst` API (no LabOne needed).

Run:  python bridge/test_bridge.py      (needs numpy)

This validates the bridge's own logic (argument handling, data shaping, JSON
safety, error mapping). It does NOT replace a test on the real instrument: the
fake only mimics the documented return shapes of the zhinst.core API.
"""
import importlib.util
import json
import os
import sys
import types

import numpy as np

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "fake_zhinst"))
import zhinst.utils as fake_utils  # noqa: E402  (the fake package)

CLOCK = 60e6


def install_fakes():
    return fake_utils.DAQ


def load_bridge():
    here = os.path.dirname(os.path.abspath(__file__))
    spec = importlib.util.spec_from_file_location("uhf_bridge", os.path.join(here, "uhf_bridge.py"))
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def main():
    daq = install_fakes()
    b = load_bridge()
    ops = b.OPS

    info = ops["connect"]({"device": "dev2091", "host": "localhost", "port": 8004})
    assert info["devtype"] == "UHFLI" and info["options"] == ["PID", "MF"], info
    json.dumps(info, allow_nan=False)

    ops["set"]({"node": "/dev2091/oscs/0/freq", "type": "d", "value": 6e6})
    assert daq.nodes["/dev2091/oscs/0/freq"] == 6e6
    ops["set"]({"node": "/dev2091/pids/0/enable", "type": "i", "value": 1})
    g = ops["get"]({"node": "/dev2091/pids/0/enable"})
    assert g == {"type": "d", "value": 1.0} or g["value"] == 1.0, g
    s = ops["read_sample"]({"demod": 0})
    assert abs(s["t"] - 1.0) < 1e-12 and s["phase"] == 1.1, s

    ops["sweep_start"]({"demod": 1, "osc": 0, "start_hz": 5.99e6, "stop_hz": 6.01e6, "points": 50, "bandwidth_hz": 50.0, "settle_inaccuracy": 0.001, "averaging_tc": 5.0})
    p = ops["sweep_progress"]({})
    assert p["finished"] is True
    r = ops["sweep_read"]({})
    assert len(r["freq"]) == len(r["x"]) == len(r["y"]) == 50
    json.dumps(r, allow_nan=False)

    ops["stream_start"]({"demod": 0, "rate": 1000.0})
    sp = ops["stream_poll"]({"window_s": 0.05})
    assert len(sp["t"]) == 100 and abs(sp["t"][1] - sp["t"][0] - 600 / CLOCK) < 1e-12
    ops["stream_stop"]({})

    ops["gated_start"]({"demod": 1, "trigger_signal": "TrigIn1", "window_s": 3e-4, "delay_s": 0.0, "pulses": 20, "points": 30, "timeout_s": 10})
    assert ops["gated_progress"]({})["finished"]
    gr = ops["gated_read"]({})
    assert gr["pulses"] == 20 and len(gr["t"]) == 30 and abs(gr["x"][0] - 1e-3) < 1e-12, gr

    ops["advisor_start"]({"pid_index": 0, "target_bw_hz": 90.0, "dut_source": 3, "slope_deg_per_hz": 0.5, "q_factor": 5e4, "f0_hz": 6e6})
    assert ops["advisor_progress"]({})["finished"]
    ad = ops["advisor_read"]({})
    assert ad["p"] == -0.5 and ad["bw"] == 90.0, ad

    ops["load_settings"]({"path": os.path.join(os.getcwd(), "x.xml")})
    ops["save_settings"]({"path": os.path.join(os.getcwd(), "x.xml")})

    # NaN must be refused, never serialised.
    try:
        b.clean(np.array([1.0, float("nan")]))
        raise SystemExit("NaN accepted")
    except b.BridgeError:
        pass
    # Calls before connect are refused with a clear message.
    b.S["daq"] = None
    try:
        ops["set"]({"node": "/x", "value": 1})
        raise SystemExit("set accepted without a connection")
    except b.BridgeError as exc:
        assert "not connected" in str(exc)
    print("bridge test OK")


if __name__ == "__main__":
    main()
