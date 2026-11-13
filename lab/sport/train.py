"""Personal session classifier from the app's sport dataset (human-validated labels).

Reads whoop-lab/sport/sessions.jsonl, trains a multinomial logistic regression on the engine's
session features (Rust/core/src/sport.rs FEATURES), evaluates it leave-one-session-out against the
rules-v1 predictions stored with each session, and writes models/<version>.json. The app installs
the newest file of that folder at its next dataset export (sport.install_model) and every later
prediction records the model version.

  python -I train.py ~/Library/Mobile\\ Documents/com~apple~CloudDocs/whoop-lab/sport
"""
import json
import os
import sys
import time

import numpy as np

CLASSES = ["course", "tennis", "musculation", "autre"]
FEATURES = ["dur_min", "hrr_mean", "hrr_p90", "steps_pm", "cad_spm", "run_frac", "walk_frac", "dyn_mean", "dyn_cv", "grav_var",
            "active_alt", "imu_cov", "strokes_pm", "sets_per_10min", "periodic_frac", "gyro_p95"]


def fit(X, y, l2=1.0, iters=3000, lr=0.1):
    n, d = X.shape
    k = len(CLASSES)
    W, b = np.zeros((k, d)), np.zeros(k)
    Y = np.eye(k)[y]
    for _ in range(iters):
        Z = X @ W.T + b
        P = np.exp(Z - Z.max(1, keepdims=True))
        P /= P.sum(1, keepdims=True)
        G = (P - Y) / n
        W -= lr * (G.T @ X + l2 * W / n)
        b -= lr * G.sum(0)
    return W, b


def main(root):
    rows = [json.loads(l) for l in open(os.path.join(root, "sessions.jsonl"))]
    rows = [r for r in rows if r.get("label_status") in ("confirmed", "corrected", "manual") and r.get("label_type") in CLASSES and r.get("features_json")]
    if len(rows) < 8 or len({r["label_type"] for r in rows}) < 2:
        sys.exit(f"{len(rows)} labelled sessions: need >= 8 over >= 2 classes")
    X = np.array([[float(r["features_json"].get(f, 0.0)) for f in FEATURES] for r in rows])
    if not np.isfinite(X).all():
        sys.exit("non-finite features: repair the dataset before training")
    y = np.array([CLASSES.index(r["label_type"]) for r in rows])
    rules_ok = np.mean([r.get("predicted_type") == r["label_type"] for r in rows])
    ok = 0
    for i in range(len(rows)):
        tr = np.arange(len(rows)) != i
        mu, sd = X[tr].mean(0), X[tr].std(0) + 1e-6
        W, b = fit((X[tr] - mu) / sd, y[tr])
        ok += int(np.argmax(((X[i] - mu) / sd) @ W.T + b) == y[i])
    loso = ok / len(rows)
    mu, sd = X.mean(0), X.std(0) + 1e-6
    W, b = fit((X - mu) / sd, y)
    version = time.strftime("personal-logreg-%Y%m%d-%H%M")
    model = {"version": version, "kind": "logreg", "params": {"features": FEATURES, "classes": CLASSES, "mean": mu.tolist(), "std": sd.tolist(), "weights": W.tolist(), "bias": b.tolist()},
             "metrics": {"n_sessions": len(rows), "loso_accuracy": round(loso, 3), "rules_v1_accuracy": round(float(rules_ok), 3)}}
    os.makedirs(os.path.join(root, "models"), exist_ok=True)
    out = os.path.join(root, "models", version + ".json")
    json.dump(model, open(out, "w"), indent=1)
    print(f"{len(rows)} sessions | leave-one-session-out {loso:.0%} vs rules-v1 {rules_ok:.0%} -> {out}")
    if loso < rules_ok:
        print("personal model is not better than the rules yet: delete the file to keep rules-v1")


if __name__ == "__main__":
    main(sys.argv[1])
