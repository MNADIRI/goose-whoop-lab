import copy
import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('capture_manifest', Path(__file__).parents[1] / 'research/capture_manifest.py')
manifest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(manifest)

class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / 'imu.csv').write_text('t,x,y,z,valid\n0,0,0,1,1\n')
        self.config = {'schema': 'goose.research.capture.v1', **{k: 'test' for k in
            ('participant_id','session_id','device_pseudonym','firmware','decoder_version','wear_location','timezone')},
            'clock_alignment': {'method':'shared_clock','offset_seconds':0,'drift_ppm':0,'uncertainty_ms':5},
            'intended_tasks':['har_harnet_head'],
            'streams':[{'path':'imu.csv','modality':'accel','sample_rate_hz':100,'rate_source':'inferred',
                'units':'g','timestamp_column':'t','quality_mask_column':'valid','configuration_id':'test','gaps_verified':True,'gaps':[]}],
            'labels':[{'source':'human_activity','reference':True}]}

    def test_hash_and_training_intent_are_preserved(self):
        result = manifest.validate(self.config,self.root)
        self.assertEqual(len(result['streams'][0]['sha256']),64)
        self.assertTrue(result['task_readiness']['har_harnet_head']['independent_label_declared'])
        self.assertFalse(result['task_readiness']['har_harnet_head']['deployment_ready'])

    def test_firmware_is_not_reference(self):
        self.config['labels'][0]['source']='firmware'
        with self.assertRaises(ValueError): manifest.validate(self.config,self.root)

    def test_escape_and_invalid_clock_are_rejected(self):
        c=copy.deepcopy(self.config)
        c['streams'][0]['path']='../outside.csv'
        with self.assertRaises(ValueError): manifest.validate(c,self.root)
        self.config['clock_alignment']['uncertainty_ms']=float('nan')
        with self.assertRaises(ValueError): manifest.validate(self.config,self.root)

    def test_missing_modalities_are_explicit(self):
        self.config['intended_tasks']=['sleep_ppg_head']
        result=manifest.validate(self.config,self.root)
        self.assertEqual(result['task_readiness']['sleep_ppg_head']['missing_modalities'],['ppg'])
        self.assertFalse(result['task_readiness']['sleep_ppg_head']['independent_label_declared'])

if __name__ == '__main__': unittest.main()
