import importlib.util
import json
import tempfile
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location('mai', Path(__file__).parents[1] / 'integrations/openrouter-transcribe.py')
mai = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mai)


def response(text='Hei', start=0.1, end=0.5):
    return {'text': text, 'language': 'no', 'words': [{'word': text, 'start': start, 'end': end}]}


class MaiTests(unittest.TestCase):
    def test_overlapping_utterances_preserve_text_order(self):
        data = {'text':'First sentence. Second sentence.', 'words':[
            {'word':'First','start':1.0,'end':1.3},
            {'word':'sentence.','start':1.3,'end':1.8},
            {'word':'Second','start':0.9,'end':1.2},
            {'word':'sentence.','start':1.2,'end':1.5}]}
        words = mai.normalize(data,3)['words']
        self.assertEqual([w['text'] for w in words],['First','sentence.','Second','sentence.'])
        self.assertEqual([w['start_ms'] for w in words],[1000,1300,900,1200])

    def test_real_times_not_speaker_guesses(self):
        data = response(); data['words'][0]['speaker'] = 5
        self.assertEqual(mai.normalize(data, 1)['words'], [{'text': 'Hei', 'start_ms': 100, 'end_ms': 500}])

    def test_missing_and_invalid_timestamps_fail(self):
        for data in [{'text': 'hei'}, response(start=-1), response(end=float('nan')), response(end=10), response(start=0.7, end=0.5)]:
            with self.subTest(data=data), self.assertRaises(ValueError):mai.normalize(data, 1)

    def test_silence_can_return_empty(self):
        self.assertEqual(mai.normalize({'text': '', 'words': []}, 1)['words'], [])

    def test_invalid_config_has_actionable_error(self):
        with tempfile.TemporaryDirectory() as root:
            path=Path(root)/'omarchy-meeting-recorder/config.toml'
            path.parent.mkdir(); path.write_text('openrouter = "wrong"')
            with patch.dict(mai.os.environ, {'XDG_CONFIG_HOME':root}):
                with self.assertRaisesRegex(ValueError,'TOML table'):mai.configuration()

    def test_keyring_missing_is_actionable(self):
        with patch.dict(mai.os.environ, {'OPENROUTER_API_KEY': ''}), patch.object(mai.subprocess, 'run', side_effect=FileNotFoundError):
            with self.assertRaisesRegex(ValueError, 'key missing'):mai.credential()

    def test_request_is_bounded_and_has_word_times(self):
        class Reply:
            def __enter__(self):return self
            def __exit__(self,*args):pass
            def read(self):return json.dumps(response()).encode()
        with patch.object(mai.urllib.request, 'urlopen', return_value=Reply()) as call:
            mai.request(b'audio', 'no', mai.options({'model':'example/stt', 'audio_format':'flac', 'provider_options':json.dumps({'custom':{'vocabulary':['Digel']}})}), 'test-key')
        req = call.call_args.args[0]
        body = json.loads(req.data)
        self.assertEqual(body['language'], 'no')
        self.assertEqual(body['timestamp_granularities'], ['word', 'segment'])
        self.assertEqual(body['model'], 'example/stt')
        self.assertEqual(body['input_audio']['format'], 'flac')
        self.assertEqual(body['provider']['options'], {'custom':{'vocabulary':['Digel']}})
        self.assertEqual(call.call_args.kwargs['timeout'], 180)

    def test_http_error_does_not_leak_body_or_key(self):
        error = urllib.error.HTTPError(mai.ENDPOINT, 401, 'secret', {}, None)
        with patch.object(mai.urllib.request, 'urlopen', side_effect=error):
            with self.assertRaisesRegex(ValueError, 'HTTP 401') as caught:mai.request(b'audio','auto',mai.options({}),'secret')
        self.assertNotIn('secret',str(caught.exception))

    def test_settings_validation_and_cache_identity(self):
        for config in [{'model':''}, {'model':2}, {'audio_format':'exe'}, {'chunk_seconds':0},
                       {'chunk_seconds':True}, {'chunk_seconds':3601}, {'chunk_seconds':1.5},
                       {'provider_options':'[]'}, {'provider_options':'{bad'},
                       {'provider_options':'{"x":NaN}'}, {'provider_options':{}}, {'phrases':[]}]:
            with self.subTest(config=config), self.assertRaises(ValueError):mai.options(config)
        default = mai.options({})
        self.assertEqual(default['model'], mai.MODEL)
        key = mai.cache_key(b'audio', 'no', default)
        for name, value in [('model','other/stt'), ('audio_format','wav'), ('chunk_seconds',30),
                            ('provider_options',{'custom':{'words':['name']}})]:
            settings = dict(default); settings[name] = value
            self.assertNotEqual(key, mai.cache_key(b'audio', 'no', settings))

    def test_job_snapshot_ignores_later_config_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            snapshot = root/'settings.json'
            snapshot.write_text(json.dumps({'model':'first/stt','chunk_seconds':1}))
            args = SimpleNamespace(input=root/'input.f32', output=root/'out.json', progress=root/'progress.json', language='no', settings=snapshot)
            args.input.write_bytes(b'\0' * (16000*4*2))
            def convert(source, target, offset, length, audio_format):
                target.write_bytes(str(offset).encode())
            def remote(audio, language, settings, key):
                self.assertEqual(settings['model'],'first/stt')
                snapshot.write_text(json.dumps({'model':'second/stt'}))
                return response()
            with patch.dict(mai.os.environ, {'XDG_CACHE_HOME':str(root)}), patch.object(mai,'credential',return_value='key'), patch.object(mai,'configuration',side_effect=AssertionError('must use snapshot')), patch.object(mai,'encode',side_effect=convert), patch.object(mai,'request',side_effect=remote) as request:
                mai.transcribe(args)
                self.assertEqual(request.call_count,2)
                self.assertEqual(len(json.loads(args.output.read_text())['words']),2)

    def test_actual_audio_formats(self):
        import subprocess
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory); source = root/'audio.f32'
            source.write_bytes(b'\0' * (16000*4))
            for fmt, codec in [('mp3','mp3'), ('wav','pcm_s16le'), ('flac','flac')]:
                target = root/('audio.'+fmt)
                mai.encode(source, target, 0, 1, fmt)
                probe = json.loads(subprocess.check_output(['ffprobe','-v','error','-show_streams','-of','json',str(target)]))
                stream = probe['streams'][0]
                self.assertEqual(stream['codec_name'],codec)
                self.assertEqual(stream['sample_rate'],'16000')
                self.assertEqual(stream['channels'],1)

    def test_chunk_offsets_and_retry_cache(self):
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            args = SimpleNamespace(input=root/'input.f32', output=root/'out.json', progress=root/'progress.json', language='no')
            args.input.write_bytes(b'\0' * (16000 * 4 * 2))
            def convert(cmd, **kwargs):Path(cmd[-1]).write_bytes(cmd[cmd.index('-ss')+1].encode())
            with patch.dict(mai.os.environ, {'XDG_CACHE_HOME':str(root)}), patch.object(mai,'CHUNK_SECONDS',1), patch.object(mai,'credential',return_value='key'), patch.object(mai,'configuration',return_value={}), patch.object(mai.subprocess,'run',side_effect=convert), patch.object(mai,'request',return_value=response()) as request:
                mai.transcribe(args)
                self.assertEqual([w['start_ms'] for w in json.loads(args.output.read_text())['words']], [100,1100])
                self.assertEqual(request.call_count,2)
                mai.transcribe(args)
                self.assertEqual(request.call_count,2)
                self.assertEqual(json.loads(args.progress.read_text())['progress'],1)

if __name__ == '__main__':unittest.main()
