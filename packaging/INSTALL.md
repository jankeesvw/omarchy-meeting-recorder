# Installing the prebuilt binary

This is a build for @ARCH@ Linux. It needs GTK 4, libadwaita 1.6 or newer, PipeWire with `parec` and `pacat` (from `libpulse`), `ffmpeg` with libopus, and the Vulkan loader (`vulkan-icd-loader`). With a Vulkan driver for your GPU the speech is transcribed on the GPU, otherwise on the CPU. whisper.cpp and ONNX Runtime are built into the binary.

From this folder:

```bash
install -Dm755 omarchy-meeting-recorder ~/.local/bin/omarchy-meeting-recorder
install -Dm644 data/omarchy-meeting-recorder.desktop ~/.local/share/applications/omarchy-meeting-recorder.desktop
install -Dm644 data/omarchy-meeting-recorder.xml ~/.local/share/mime/packages/omarchy-meeting-recorder.xml
update-mime-database ~/.local/share/mime
xdg-mime default omarchy-meeting-recorder.desktop application/x-omarchy-meeting
```

The bar widget and the Hyprland window rule are described in README.md. The speech model (about 1.6 GB) is downloaded on first use, or taken from voxtype when it already has it. The speaker model (about 120 MB) is downloaded the first time speakers are told apart.

Actions, your own scripts from the done page, are explained on https://github.com/jankeesvw/omarchy-meeting-recorder/blob/main/docs/actions.md; two examples are in examples/actions.
