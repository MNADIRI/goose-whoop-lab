"""Validate a research capture sidecar and hash its local data files. No model inference.

python lab/research/capture_manifest.py capture.json --output manifest.json
Only structural eligibility is checked: output is NEVER a clinical validation or training approval.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path

TASKS = {
    'har_harnet_head': {
        'model': 'OxWearables/ssl-wearables HARNet-10',
        'head': 'activity classification; separate segmentation and repetition heads',
        'signals': {'accel'}, 'labels': {'human_activity', 'synchronized_video'},
    },
    'sleep_ppg_head': {
        'model': 'SleepPPG-Net family (checkpoint/preprocessing to be verified before training)',
        'head': '30-second wake/light/deep/REM classification adapted to wrist WHOOP PPG',
        'signals': {'ppg'}, 'labels': {'psg', 'eeg_reviewed'},
    },
    'ppg_quality_head': {
        'model': 'Nokia-Bell-Labs/PaPaGei encoder',
        'head': 'task-specific PPG quality classifier (HR / PRV / morphology)',
        'signals': {'ppg'}, 'labels': {'expert_signal_quality', 'ecg'},
    },
    'ppg_respiration_head': {
        'model': 'Nokia-Bell-Labs/PaPaGei encoder (experimental transfer)',
        'head': 'respiratory rate regression with uncertainty',
        'signals': {'ppg'}, 'labels': {'respiratory_belt', 'capnography'},
    },
}


def finite_number(v):
    return isinstance(v, (float, int)) and not isinstance(v, bool) and math.isfinite(v)


def validate(config, root):
    """Return a reproducible inventory or raise ValueError; never modify source data."""
    if config.get('schema') != 'goose.research.capture.v1':
        raise ValueError('unsupported capture schema')
    for key in ('participant_id', 'session_id', 'device_pseudonym', 'firmware', 'decoder_version', 'wear_location', 'timezone'):
        if not isinstance(config.get(key), str) or not config[key].strip():
            raise ValueError(f'missing metadata: {key}')
    clock = config.get('clock_alignment', {})
    if clock.get('method') not in ('shared_clock', 'sync_markers', 'estimated_offset_drift'):
        raise ValueError('clock alignment method required')
    for key in ('offset_seconds', 'drift_ppm', 'uncertainty_ms'):
        if not finite_number(clock.get(key)):
            raise ValueError(f'clock {key} must be finite')
    if clock['uncertainty_ms'] < 0:
        raise ValueError('negative clock uncertainty')
    tasks = config.get('intended_tasks', [])
    if not tasks or any(t not in TASKS for t in tasks):
        raise ValueError('unknown or missing intended task')
    root = root.resolve()
    inventory = []
    modalities = set()
    streams = config.get('streams', [])
    if not streams:
        raise ValueError('at least one stream required')
    for stream in streams:
        path = (root / stream['path']).resolve()
        if not path.is_relative_to(root) or not path.is_file():
            raise ValueError('stream path must be an existing file inside the capture directory')
        if not finite_number(stream.get('sample_rate_hz')) or stream['sample_rate_hz'] <= 0:
            raise ValueError('positive measured/declared sample rate required')
        if stream.get('rate_source') not in ('measured', 'firmware_declared', 'inferred'):
            raise ValueError('sampling rate provenance required')
        for field in ('units', 'timestamp_column', 'quality_mask_column', 'configuration_id'):
            if not isinstance(stream.get(field), str) or not stream[field]:
                raise ValueError(f'stream requires {field}')
        if stream.get('modality') == 'ppg':
            if not isinstance(stream.get('wavelength'), str) or not stream['wavelength']:
                raise ValueError('PPG wavelength must be explicit, including unknown')
            for field in ('led_source', 'photodiode', 'gain', 'offset'):
                if field not in stream:
                    raise ValueError(f'PPG needs {field}; use null when unresolved')
        if stream.get('gaps_verified') is not True:
            raise ValueError('inspect timestamps and list gaps before sealing a capture')
        for gap in stream.get('gaps', []):
            if not all(finite_number(gap.get(k)) for k in ('start', 'end')) or gap['end'] <= gap['start']:
                raise ValueError('invalid gap interval')
        h = hashlib.sha256()
        with path.open('rb') as f:
            for block in iter(lambda: f.read(1024 * 1024), b''):
                h.update(block)
        inventory.append({**stream, 'sha256': h.hexdigest(), 'bytes': path.stat().st_size})
        modalities.add(stream.get('modality'))
    labels = config.get('labels', [])
    for label in labels:
        if label.get('source') in ('firmware', 'model_prediction') and label.get('reference') is True:
            raise ValueError('predictions/firmware cannot be independent reference labels')
    readiness = {}
    for task in tasks:
        target = TASKS[task]
        readiness[task] = {
            'model': target['model'], 'head_to_train': target['head'],
            'missing_modalities': sorted(target['signals'] - modalities),
            'independent_label_declared': any(l.get('reference') is True and l.get('source') in target['labels'] for l in labels),
            'deployment_ready': False,
            'note': 'Metadata declaration only; validate signal contents, labels and timing before training.',
        }
    return {**config, 'streams': inventory, 'task_readiness': readiness, 'validation_scope': 'metadata_and_file_integrity_only'}


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('capture', type=Path)
    p.add_argument('--output', required=True, type=Path)
    args = p.parse_args()
    if args.output.resolve() == args.capture.resolve():
        p.error('output must not overwrite the capture config')
    config = json.loads(args.capture.read_text())
    result = validate(config, args.capture.parent)
    sources = {(args.capture.parent / stream['path']).resolve() for stream in config['streams']}
    if args.output.resolve() in sources or args.output.exists():
        p.error('output must be a new file, distinct from every source')
    with args.output.open('x') as f:
        json.dump(result, f, indent=2, allow_nan=False)
        f.write('\n')
    print(f'Capture manifest written: {args.output}; no model has been trained or enabled.')

if __name__ == '__main__':
    main()
