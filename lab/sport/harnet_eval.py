"""HARNet (OxWearables ssl-wearables) vs handcrafted IMU features on WHOOP 5.0 R21 data.

Inputs (either):
  --dataset DIR   sport dataset exported by the app (whoop-lab/sport): sessions.jsonl + sessions/<id>/imu.csv,
                  labels = sessions.label_type (validated sessions only)
  --sensor-tsv F  Documents/sensor_session.tsv (guided sensor session): labels = scripted gestures
Protocol: IMU resampled 100 -> 30 Hz, 10 s windows (HARNet input: 3 x 300, accel in g), hop 5 s.
Evaluation: leave-one-group-out (group = session, or scripted step), 1-NN cosine and nearest
centroid on (a) HARNet-10 embeddings, (b) handcrafted accel+gyro features, (c) both.
Also reports model size and CPU inference time per window (on-device feasibility).

  python -I harnet_eval.py --sensor-tsv sensor_session.tsv
  python -I harnet_eval.py --dataset ~/Library/Mobile\\ Documents/com~apple~CloudDocs/whoop-lab/sport
"""
import argparse
import csv
import json
import os
import sys
import time

import numpy as np

FS_IN, FS_OUT, WIN_S, HOP_S = 100.0, 30.0, 10.0, 5.0


def r21_from_hex(h):
    b = bytes.fromhex(h)
    if len(b) != 1244 or b[8] not in (43, 47) or b[9] != 21:
        return None
    ts = int.from_bytes(b[15:19], "little") + int.from_bytes(b[19:21], "little") / 32768
    n = min(int.from_bytes(b[24:26], "little"), int.from_bytes(b[630:632], "little"), 100)
    if n == 0:
        return None
    ax = [np.frombuffer(b[o:o + 200], "<i2")[:n].astype(float) / 4096 for o in (28, 228, 428)]
    gy = [np.frombuffer(b[o:o + 200], "<i2")[:n].astype(float) * 2000 / 32768 for o in (640, 840, 1040)]
    for g in gy:  # start-up sentinel
        bad = g <= -1999.9
        if bad.any() and (~bad).any():
            g[bad] = g[~bad][0]
    t = ts + np.arange(n) / FS_IN
    return t, np.vstack(ax), np.vstack(gy)


def load_sensor_tsv(path):
    steps = {}
    for line in open(path).read().splitlines()[1:]:
        c = line.split("\t")
        if len(c) < 4:
            continue
        r = r21_from_hex(c[3])
        if r is None:
            continue
        steps.setdefault((c[1], c[2]), []).append(r)
    names = {("P2_imu_live", "0"): "immobile", ("P2_imu_live", "1"): "rotation poignet", ("P2_imu_live", "2"): "bras leve",
             ("P2_imu_live", "3"): "marche", ("P2_imu_live", "4"): "tapes", ("P2_imu_live", "5"): "secousses",
             ("P2_imu_live", "6"): "immobile", ("P4_all_live", "0"): "immobile", ("P4_all_live", "1"): "marche"}
    out = []
    for k, recs in steps.items():
        if k not in names:
            continue
        recs.sort(key=lambda r: r[0][0])
        out.append(("%s/%s" % k, names[k], np.concatenate([r[0] for r in recs]), np.hstack([r[1] for r in recs]), np.hstack([r[2] for r in recs])))
    return out


def load_dataset(root):
    out = []
    for line in open(os.path.join(root, "sessions.jsonl")):
        s = json.loads(line)
        label = s.get("label_type")
        if s.get("label_status") not in ("confirmed", "corrected", "manual") or not label or label == "aucune":
            continue
        f = os.path.join(root, "sessions", s["id"], "imu.csv")
        if not os.path.exists(f):
            continue
        rows = np.loadtxt(f, delimiter=",", skiprows=1, ndmin=2)
        if len(rows) < FS_IN * WIN_S:
            continue
        out.append((s["id"], label, rows[:, 0], rows[:, 1:4].T, rows[:, 4:7].T))
    return out


def windows(t, a, g):
    """Contiguous 30 Hz windows of 10 s (no window spans a data gap)."""
    res = []
    gaps = np.flatnonzero(np.diff(t) > 0.1)
    bounds = np.split(np.arange(len(t)), gaps + 1)
    for idx in bounds:
        if len(idx) < FS_IN * WIN_S:
            continue
        tt = t[idx]
        grid = np.arange(tt[0], tt[-1] - WIN_S, HOP_S)
        for t0 in grid:
            tw = t0 + np.arange(int(WIN_S * FS_OUT)) / FS_OUT
            aw = np.vstack([np.interp(tw, tt, a[k, idx]) for k in range(3)])
            gw = np.vstack([np.interp(tw, tt, g[k, idx]) for k in range(3)])
            res.append((aw, gw))
    return res


def handcrafted(aw, gw):
    m = np.linalg.norm(aw, axis=0)
    gm = np.linalg.norm(gw, axis=0)
    spec = np.abs(np.fft.rfft(m - m.mean())) ** 2
    f = np.fft.rfftfreq(len(m), 1 / FS_OUT)
    dom = f[np.argmax(spec[1:]) + 1]
    feats = list(aw.mean(1)) + list(aw.std(1)) + [m.mean(), m.std(), np.percentile(m, 95), dom, spec[(f > 0.5) & (f < 3)].sum() / max(spec[1:].sum(), 1e-9)]
    feats += list(gw.std(1)) + [gm.mean(), np.percentile(gm, 95), gm.max()]
    return np.array(feats)


def loo_eval(X, y, groups):
    """Leave-one-group-out 1-NN (cosine) and nearest centroid, on standardised features."""
    res = {}
    for name, fn in (("1nn", "nn"), ("centroid", "c")):
        ok = n = 0
        for gname in sorted(set(groups)):
            te = np.array([g == gname for g in groups])
            tr = ~te
            if len(set(np.array(y)[tr])) < 2:
                continue
            mu, sd = X[tr].mean(0), X[tr].std(0) + 1e-9
            A, B = (X[tr] - mu) / sd, (X[te] - mu) / sd
            ytr = np.array(y)[tr]
            if fn == "nn":
                An = A / np.linalg.norm(A, axis=1, keepdims=True)
                Bn = B / np.linalg.norm(B, axis=1, keepdims=True)
                pred = ytr[np.argmax(Bn @ An.T, axis=1)]
            else:
                cls = sorted(set(ytr))
                C = np.vstack([A[ytr == c].mean(0) for c in cls])
                pred = np.array(cls)[np.argmin(((B[:, None, :] - C[None]) ** 2).sum(-1), axis=1)]
            ok += int((pred == np.array(y)[te]).sum())
            n += int(te.sum())
        res[name] = round(ok / max(n, 1), 3)
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dataset")
    ap.add_argument("--sensor-tsv")
    ap.add_argument("--repo", help="local clone of OxWearables/ssl-wearables (weights included); default: torch.hub from GitHub")
    a = ap.parse_args()
    data = load_dataset(a.dataset) if a.dataset else load_sensor_tsv(a.sensor_tsv)
    W, y, groups = [], [], []
    for gid, label, t, acc, gyr in data:
        for aw, gw in windows(t, acc, gyr):
            W.append((aw, gw))
            y.append(label)
            groups.append(gid)
    print(f"windows: {len(W)}  classes: { {c: y.count(c) for c in sorted(set(y))} }  groups: {len(set(groups))}")
    if len(W) < 4:
        sys.exit("not enough windows")
    H = np.vstack([handcrafted(aw, gw) for aw, gw in W])

    import torch
    if a.repo:
        model = torch.hub.load(a.repo, "harnet10", source="local", class_num=5, pretrained=True)
    else:
        model = torch.hub.load("OxWearables/ssl-wearables", "harnet10", class_num=5, pretrained=True, trust_repo=True)
    model.eval()
    fe = model.feature_extractor
    x = torch.tensor(np.stack([aw for aw, _ in W]), dtype=torch.float32)
    with torch.no_grad():
        t0 = time.time()
        E = fe(x).reshape(len(W), -1).numpy()
        dt = (time.time() - t0) / len(W)
    params = sum(p.numel() for p in fe.parameters())
    print(f"HARNet-10 feature extractor: {params / 1e6:.1f} M parameters, embedding {E.shape[1]}, CPU {dt * 1000:.1f} ms / window")
    for name, X in (("handcrafted (accel+gyro)", H), ("HARNet-10 (accel)", E), ("HARNet-10 + gyro features", np.hstack([E, H]))):
        print(f"  {name:28s} leave-one-group-out: {loo_eval(X, y, groups)}")


if __name__ == "__main__":
    main()
