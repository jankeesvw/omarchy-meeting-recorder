#!/usr/bin/env python3
"""OpenRouter transcription adapter. Private files, timestamped words, no third-party Python packages."""
import argparse
import base64
import hashlib
import json
import math
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request

MODEL = 'microsoft/mai-transcribe-2'
ENDPOINT = 'https://openrouter.ai/api/v1/audio/transcriptions'
RATE = 16000
CHUNK_SECONDS = 300


def atomic_json(path, value):
    path = Path(path)
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as out:
        json.dump(value, out, ensure_ascii=False)
        temporary = Path(out.name)
    temporary.replace(path)


def configuration():
    root = Path(os.environ.get('XDG_CONFIG_HOME', ''))
    if not root.is_absolute():
        root = Path.home() / '.config'
    path = root / 'omarchy-meeting-recorder/config.toml'
    if not path.exists():
        return {}
    with path.open('rb') as f:
        config = tomllib.load(f).get('openrouter', {})
    if not isinstance(config, dict):
        raise ValueError('openrouter must be a TOML table')
    return config


def options(config):
    model = config.get('model', MODEL)
    if not isinstance(model, str) or not model.strip() or any(c.isspace() for c in model):
        raise ValueError('openrouter.model must be a non-empty model ID without whitespace')
    audio_format = config.get('audio_format', 'mp3')
    if audio_format not in ('mp3', 'wav', 'flac'):
        raise ValueError('openrouter.audio_format must be mp3, wav or flac')
    chunk = config.get('chunk_seconds', CHUNK_SECONDS)
    if type(chunk) is not int or not 1 <= chunk <= 3600:
        raise ValueError('openrouter.chunk_seconds must be an integer from 1 to 3600')
    raw = config.get('provider_options', '{}')
    if not isinstance(raw, str):
        raise ValueError('openrouter.provider_options must be a JSON string containing an object')
    try:
        provider = json.loads(raw, parse_constant=lambda _: (_ for _ in ()).throw(ValueError()))
    except ValueError:
        raise ValueError('openrouter.provider_options must contain valid JSON') from None
    try:
        json.dumps(provider, allow_nan=False)
    except ValueError:
        raise ValueError('openrouter.provider_options must contain finite JSON values') from None
    if not isinstance(provider, dict):
        raise ValueError('openrouter.provider_options must contain a JSON object')
    if 'phrases' in config:
        raise ValueError('Replace openrouter.phrases with provider_options; see docs/openrouter.md')
    return {'model': model, 'audio_format': audio_format, 'chunk_seconds': chunk,
            'provider_options': provider}


def encode(source, target, offset, length, audio_format):
    codecs = {'mp3': ['-c:a', 'libmp3lame', '-b:a', '64k'],
              'wav': ['-c:a', 'pcm_s16le'], 'flac': ['-c:a', 'flac']}
    subprocess.run(['ffmpeg', '-nostdin', '-v', 'error', '-y', '-f', 'f32le', '-ar', str(RATE),
                    '-ac', '1', '-ss', str(offset), '-i', str(source), '-t', str(length),
                    *codecs[audio_format], str(target)],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def cache_key(audio, language, settings):
    return hashlib.sha256(audio + json.dumps([settings, language, 'v2'],
                                             sort_keys=True, allow_nan=False).encode()).hexdigest()


def credential():
    key = os.environ.get('OPENROUTER_API_KEY', '').strip()
    if not key:
        try:
            result = subprocess.run(['secret-tool', 'lookup', 'service',
                                     'omarchy-meeting-recorder', 'credential',
                                     'openrouter-api-key'], capture_output=True, text=True, timeout=15)
            key = result.stdout.strip() if result.returncode == 0 else ''
        except (OSError, subprocess.TimeoutExpired):
            pass
    if not key:
        raise ValueError('OpenRouter key missing. Set OPENROUTER_API_KEY or store it in the system keyring; see docs/openrouter.md.')
    if any(c in key for c in '\r\n'):
        raise ValueError('Invalid OpenRouter key')
    return key


def normalize(response, duration):
    """Reject text without real word times: guessed times break the player and speakers."""
    if not isinstance(response, dict) or not isinstance(response.get('text'), str):
        raise ValueError('OpenRouter returned no transcript text')
    raw = response.get('words', [])
    if not isinstance(raw, list):
        raise ValueError('OpenRouter returned invalid word timestamps')
    # Providers may return overlapping utterances in transcript order.
    # Preserve that order: sorting individual words corrupts the sentences.
    words = []
    for word in raw:
        if not isinstance(word, dict):
            raise ValueError('OpenRouter returned an invalid word')
        text = word.get('word', word.get('text'))
        start, end = word.get('start'), word.get('end')
        if (not isinstance(text, str) or not isinstance(start, (float, int))
                or not isinstance(end, (float, int)) or isinstance(start, bool) or isinstance(end, bool)
                or not math.isfinite(start) or not math.isfinite(end)
                or start < 0 or end < start or end > duration + 1):
            raise ValueError('OpenRouter returned invalid word timestamps')
        if text.strip():
            words.append({'text': text.strip(), 'start_ms': round(min(start, duration) * 1000),
                          'end_ms': round(min(end, duration) * 1000)})
    if response['text'].strip() and not words:
        raise ValueError('Selected OpenRouter model/provider returned no word timestamps; choose one supporting verbose_json and word timestamps')
    language = response.get('language')
    if not isinstance(language, str):
        languages = response.get('languages', [])
        language = languages[0] if isinstance(languages, list) and languages and isinstance(languages[0], str) else None
    return {'words': words, 'language': language}


def request(audio, language, settings, key):
    payload = {'model': settings['model'],
               'input_audio': {'data': base64.b64encode(audio).decode(), 'format': settings['audio_format']},
               'response_format': 'verbose_json', 'timestamp_granularities': ['word', 'segment']}
    if settings['provider_options']:
        payload['provider'] = {'options': settings['provider_options']}
    if language != 'auto':
        payload['language'] = language
    body = json.dumps(payload).encode()
    req = urllib.request.Request(ENDPOINT, body, headers={
        'Authorization': 'Bearer ' + key, 'Content-Type': 'application/json'})
    for attempt in range(3):
        try:
            with urllib.request.urlopen(req, timeout=180) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            if error.code in (429, 502, 503, 504) and attempt < 2:
                time.sleep(2 ** (attempt + 1))
                continue
            # Never include a response body which might echo credentials or audio.
            raise ValueError(f'OpenRouter returned HTTP {error.code}; check the key, credit and service availability') from None
    raise ValueError('OpenRouter request failed')


def transcribe(args):
    settings_path = getattr(args, 'settings', None)
    config = json.loads(settings_path.read_text()) if settings_path else configuration()
    if not isinstance(config, dict):
        raise ValueError('openrouter must be a TOML table')
    settings = options(config)
    chunk_seconds = settings['chunk_seconds']
    key = credential()
    size = args.input.stat().st_size
    if size % 4:
        raise ValueError('Invalid f32 audio')
    duration = size / (RATE * 4)
    count = max(1, math.ceil(duration / chunk_seconds))
    cache_root = Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')) / 'omarchy-meeting-recorder/openrouter-responses'
    cache_root.mkdir(parents=True, exist_ok=True, mode=0o700)
    words = []
    detected = None
    with tempfile.TemporaryDirectory(prefix='upload-', dir=args.output.parent) as temporary:
        for index in range(count):
            offset = index * chunk_seconds
            length = min(chunk_seconds, duration - offset)
            atomic_json(args.progress, {'progress': index / count, 'stage': f'OpenRouter: transcribing part {index + 1} of {count}'})
            audio = Path(temporary) / ('audio.' + settings['audio_format'])
            encode(args.input, audio, offset, length, settings['audio_format'])
            encoded = audio.read_bytes()
            digest = cache_key(encoded, args.language, settings)
            cached = cache_root / (digest + '.json')
            response = None
            if cached.exists():
                try:
                    candidate = json.loads(cached.read_text())
                    normalize(candidate, length)
                    response = candidate
                except (ValueError, OSError, TypeError):
                    pass
            if response is None:
                response = request(encoded, args.language, settings, key)
                normalize(response, length)
                atomic_json(cached, response)
            result = normalize(response, length)
            detected = detected or result['language']
            for word in result['words']:
                word['start_ms'] += offset * 1000
                word['end_ms'] += offset * 1000
                words.append(word)
    atomic_json(args.output, {'words': words, 'language': detected})
    atomic_json(args.progress, {'progress': 1.0, 'stage': 'OpenRouter transcription ready'})


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--settings', type=Path, help='Private per-job config snapshot')
    parser.add_argument('--input', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--progress', type=Path, required=True)
    parser.add_argument('--language', default='auto')
    args = parser.parse_args()
    try:
        transcribe(args)
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(str(error) if isinstance(error, ValueError) else 'OpenRouter transcription failed; check network access and ffmpeg', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
