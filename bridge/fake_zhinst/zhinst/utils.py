"""Fake `zhinst.utils` used by the bridge tests."""
import numpy as np

CLOCK = 60e6


class FakeModule:
    def __init__(self, kind):
        self.kind, self.params, self.subs, self.calc = kind, {}, [], 0
        self._finished = False

    def set(self, k, v):
        self.params[k] = v
        if k == "calculate":
            self.calc = 0  # advisor finishes immediately

    def getInt(self, k):
        return self.calc

    def get(self, pattern, flat=True):
        # modules expose results of the advisor through get("*", True)
        return self.read(flat)

    def subscribe(self, p):
        self.subs.append(p)

    def unsubscribe(self, p):
        self.subs = [s for s in self.subs if s != p]

    def execute(self):
        self._finished = True

    def progress(self):
        return np.array([1.0])

    def finished(self):
        return self._finished

    def finish(self):
        self._finished = True

    def read(self, flat=True):
        if self.kind == "sweep":
            f = np.linspace(self.params["start"], self.params["stop"], self.params["samplecount"])
            u = (f - 6.0e6) / 60.0
            return {self.subs[0]: [[{"frequency": f, "x": 1e-3 / (1 + u * u), "y": -1e-3 * u / (1 + u * u)}]]}
        if self.kind == "daq":
            cols = self.params["grid/cols"]
            seg = lambda: {"value": np.ones((1, cols)) * 1e-3}
            return {p: [seg() for _ in range(self.params["count"])] for p in self.subs}
        if self.kind == "advisor":
            return {"/pid/p": [np.array([-0.5])], "/pid/i": [np.array([-300.0])], "/pid/d": [np.array([0.0])], "/bw": [np.array([90.0])]}
        return {}


class FakeDaq:
    def __init__(self):
        self.nodes = {}
        self.subscribed = []

    def setDouble(self, n, v):
        self.nodes[n] = float(v)

    def setInt(self, n, v):
        self.nodes[n] = int(v)

    def getInt(self, n):
        if n.endswith("clockbase"):
            return int(CLOCK)
        return int(self.nodes.get(n, 0))

    def getDouble(self, n):
        return float(self.nodes.get(n, 0.0))

    def get(self, n, flat=False):
        v = self.nodes.get(n, 0)
        return {n: {"timestamp": np.array([1]), "value": np.array([v])}}

    def sync(self):
        pass

    def getSample(self, n):
        return {k: np.array([v]) for k, v in dict(timestamp=int(CLOCK), x=1e-3, y=2e-3, frequency=6e6, phase=1.1).items()}

    def subscribe(self, p):
        self.subscribed.append(p)

    def unsubscribe(self, p):
        self.subscribed = [] if p == "*" else [s for s in self.subscribed if s != p]

    def poll(self, window, timeout, flags, flat):
        n = 100
        key = self.subscribed[0]
        return {key: {"timestamp": (np.arange(n) * 600).astype(float), "x": np.ones(n), "y": np.zeros(n), "frequency": np.full(n, 6e6), "phase": np.zeros(n)}}

    def sweep(self):
        return FakeModule("sweep")

    def dataAcquisitionModule(self):
        return FakeModule("daq")

    def pidAdvisor(self):
        return FakeModule("advisor")

    def deviceSettings(self):
        return FakeModule("settings")



DAQ = FakeDaq()


def create_api_session(device, level, server_host="localhost", server_port=8004):
    return DAQ, device, {"devicetype": "UHFLI", "options": "PID\nMF"}
