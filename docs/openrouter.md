# Optional OpenRouter transcription

Whisper remains the default. To use cloud transcription, configure `~/.config/omarchy-meeting-recorder/config.toml`:

```toml
# Root settings must precede tables.
backend = "openrouter"
model = "large-v3-turbo" # Used only by the local Whisper backend.

[openrouter]
model = "microsoft/mai-transcribe-2"
audio_format = "mp3"
chunk_seconds = 300
provider_options = '''
{
  "azure": {
    "diarization": {"enabled": false},
    "phraseList": {"phrases": ["Acme", "Project Atlas"]}
  }
}
'''
```

Only `backend` is required: the OpenRouter defaults are MAI-Transcribe-2, MP3, 300 seconds and no provider options. Use `backend = "whisper"` for local transcription. The legacy value `mai` remains an alias for `openrouter`. Settings are read once at the start of each transcription and shared by both tracks. New recordings, imported audio and retranscription all use the config; an old meeting's model metadata does not override it. There is no backend selector in the UI.

The CLI uses the same config; `--backend whisper` or `--backend openrouter` overrides it. `--model` continues to select the local Whisper model only:

```sh
omarchy-meeting-recorder transcribe-file recording.wav --backend openrouter --language no --speakers 1
```

## Compatible models and providers

Choose a speech-to-text model from OpenRouter's `/api/v1/audio/transcriptions` endpoint, not a general chat model. The selected model/provider must return real word timestamps via `verbose_json` and `timestamp_granularities`. Text-only responses produce a clear error: the app needs word times for the player and local speaker assignment. Compatibility varies by model and provider; only MAI has been verified with a live request in this integration.

`provider_options` is an optional JSON string containing an object. Its content is sent unchanged as `provider.options`, keyed by provider slug. It can contain vocabulary hints and any other options supported by that provider. No Azure options are injected automatically. These options do not force provider routing. See the [OpenRouter speech-to-text guide](https://openrouter.ai/docs/guides/overview/multimodal/stt) for model discovery, formats and provider-specific settings.

The old `phrases` field is replaced by the JSON structure in the example. Use the provider-specific vocabulary structure shown above.

## Audio and chunking

`audio_format` accepts `mp3`, `wav` or `flac`, controlling both FFmpeg encoding and the request's format field. The source recording is preserved. All uploads are 16 kHz mono: MP3 uses 64 kbit/s, WAV uses 16-bit PCM, and FLAC uses lossless compression. Provider format support varies.

`chunk_seconds` is an integer from 1 to 3600, default 300. This is a client-side bound, not a guarantee the provider accepts that duration. Shorter chunks can help with provider processing timeouts and file-size limits. Encoding cannot restore information lost in the original recording. Chunking uses consecutive non-overlapping pieces; words at boundaries may be affected.

## Credentials and operation

Cloud transcription sends audio to OpenRouter and the serving provider and incurs API charges. Speaker identification remains local. Python 3.11+ and FFmpeg are required. Store the key in the system keyring using `secret-tool`:

```sh
secret-tool store --label='Meeting Recorder OpenRouter' service omarchy-meeting-recorder credential openrouter-api-key
```

Paste the key at the prompt. Alternatively set `OPENROUTER_API_KEY`. Keys are not put in meeting files or process arguments.

Requests have a 180-second client timeout and retry transient HTTP failures up to three attempts. Provider timeouts can be shorter. Successful responses are cached privately under `~/.cache/omarchy-meeting-recorder/openrouter-responses`. Audio, model, language, format, chunk length and provider options participate in cache identity. Old cache entries are not reused for the new request schema.

Temporary audio is removed after success, failure or cancellation. Cancellation cannot recall an upload already accepted by a provider. Errors do not silently switch providers or overwrite the previous transcript. Saved audio remains available for retry. OpenRouter skips Whisper model downloads, but local speaker models may still download on first use.
