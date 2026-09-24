"""Exports Nemotron 3 Diarization as two stateless ONNX graphs plus the numbers the Rust side needs.

frontend.onnx: power spectrum (1, T, 257) + number of valid frames -> encoder input embeddings (1, ceil(T/8), H)
step.onnx:     embeddings of one step (1, L, H)                    -> speaker logits (1, 8L, 8)
nemotron.json: streaming config, and the learned silence embedding
"""
import json, sys
import numpy as np, torch, librosa
from torch import nn
from transformers import AutoModelForAudioFrameClassification

out = sys.argv[1]
model = AutoModelForAudioFrameClassification.from_pretrained("nvidia/Nemotron-3-Diarization").eval()
cfg = model.config
mel = torch.from_numpy(librosa.filters.mel(sr=16000, n_fft=512, n_mels=128, fmin=0.0, fmax=8000, norm="slaney")).float()


class Frontend(nn.Module):
    def __init__(self):
        super().__init__()
        self.register_buffer("mel", mel)
        self.embedder = model.model.audio_tower.embedder

    def forward(self, power, valid):  # power: (1, T, 257), T a multiple of 8; valid: (1,) int64
        x = torch.log(power @ self.mel.T + 2**-24)
        keep = (torch.arange(x.shape[1])[None, :] < valid[:, None]).to(x.dtype)
        x = x * keep[..., None]
        b, t, m = x.shape
        return self.embedder.projection(x.reshape(b, t // 8, m * 8))


class Step(nn.Module):
    def __init__(self):
        super().__init__()
        self.model = model.model
        self.classifier = model.classifier

    def forward(self, embeds):
        pos = torch.arange(embeds.shape[1])[None, :]
        hidden = self.model(inputs_embeds=embeds, position_ids=pos).last_hidden_state
        return self.classifier(hidden)


with torch.no_grad():
    t = 64
    torch.onnx.export(Frontend(), (torch.rand(1, t, 257), torch.tensor([t])), f"{out}/frontend.onnx",
                      input_names=["power", "valid"], output_names=["embeds"],
                      dynamic_axes={"power": {1: "frames"}, "embeds": {1: "steps"}}, opset_version=17, dynamo=False)
    h = cfg.audio_config.hidden_size
    torch.onnx.export(Step(), (torch.rand(1, 50, h),), f"{out}/step.onnx",
                      input_names=["embeds"], output_names=["logits"],
                      dynamic_axes={"embeds": {1: "steps"}, "logits": {1: "frames"}}, opset_version=17, dynamo=False)

s = cfg.streaming_config
info = {
    "hidden_size": h,
    "subsampling_factor": cfg.audio_config.subsampling_factor,
    "num_speakers": s.num_speakers,
    "chunk_length": cfg.chunk_length,
    "chunk_right_context": cfg.chunk_right_context,
    "fifo_length": cfg.fifo_length,
    "speaker_cache_update_period": cfg.speaker_cache_update_period,
    "speaker_cache_length": s.speaker_cache_length,
    "silence_frames_per_speaker": s.speaker_cache_silence_frames_per_speaker,
    "prediction_score_threshold": s.prediction_score_threshold,
    "latest_frames_score_boost": s.latest_frames_score_boost,
    "min_positive_scores_rate": s.min_positive_scores_rate,
    "strong_boost_rate": s.strong_boost_rate,
    "weak_boost_rate": s.weak_boost_rate,
    "silence_embeds": model.silence_embeds.detach().float().tolist(),
}
json.dump(info, open(f"{out}/nemotron.json", "w"))
print({k: v for k, v in info.items() if k != "silence_embeds"})
