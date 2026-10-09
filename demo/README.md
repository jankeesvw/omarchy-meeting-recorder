# Making the screenshots and videos

Everything in `screenshots/` and `media/` was shot in a throwaway Omarchy VM with invented meetings. This guide is the whole process, step by step, so it can be done again without working anything out twice.

## Rules

- **Demo data only.** Never use a real meeting or a real recording in any image or video. The meetings here are written in `script.txt` and `import-script.txt` and voiced by piper.
- **Everything with a window happens in the VM.** Never open the app on your own desktop to take a picture: a new window takes the keyboard focus, and whatever you type ends up in the app.
- **Never play audio on the host.** piper writes WAV files; they are only ever played inside the VM, which has no sound card (its only output is `auto_null`).
- **Look at every file** before it goes anywhere: sharp, nothing private, no stray pointer in a still, nothing cut off.
- **What goes where.** `screenshots/` is tracked in git and holds only what the README shows, as WebP. `media/` is gitignored and holds the full library: `stills/` (window crops per theme and state, plus `context/` with the whole desktop), `closeups/`, `videos/`, `social/`, and `demo-data/` (the demo meeting, the demo tracks and the import file, so a re-shoot does not have to start from zero).

## 1. Prerequisites on the host

- `omavm` with a `fresh` snapshot (see the `vm` skill): a disposable Omarchy VM with SSH, passwordless sudo and autologin.
- `piper-tts` and these English voices from [rhasspy/piper-voices](https://huggingface.co/rhasspy/piper-voices), in one directory (`.onnx` plus `.onnx.json` each):
  - `en_US-amy-medium` (Maya in the recording demo, Anna in the import demo)
  - `en_US-ryan-medium` (Tom)
  - `en_US-hfc_male-medium` (Rob) and `en_US-lessac-medium` (Lena) for the import demo

  ```bash
  B=https://huggingface.co/rhasspy/piper-voices/resolve/main/en
  for v in en_US/amy/medium/en_US-amy-medium en_US/ryan/medium/en_US-ryan-medium \
           en_US/hfc_male/medium/en_US-hfc_male-medium en_US/lessac/medium/en_US-lessac-medium; do
    curl -sSL -o voices/$(basename $v).onnx "$B/$v.onnx"
    curl -sSL -o voices/$(basename $v).onnx.json "$B/$v.onnx.json"
  done
  ```

- `ffmpeg`, ImageMagick (`magick`), Python 3 with Pillow, `jq`, and the Liberation fonts (`/usr/share/fonts/liberation/`).
- The whisper model on the host at `~/.local/share/voxtype/models/ggml-large-v3-turbo.bin` (or `~/.local/share/omarchy-meeting-recorder/models/`), to push into the VM instead of letting it download 1.6 GB.
- A release build, and the animation preview for the per-theme animation shots:

  ```bash
  mise exec -- cargo build --release
  CARGO_TARGET_DIR=target/preview mise exec -- cargo build --release --example transcribe_animation
  ```

## 2. The demo meetings

**The recorded meeting** is `script.txt`: Maya on the microphone and Tom on the computer audio, about five minutes about a product launch, one turn per line (`M|...` or `T|...`). `render.py` voices it into two tracks of the same length, so playing them at the same moment gives a natural conversation:

```bash
demo/render.py <voices-dir> /tmp/demo-audio      # writes maya.wav and tom.wav (48 kHz stereo)
```

**The imported file** is `import-script.txt`: Anna, Rob and Lena in a design review, about 1:40. `render_import.py` makes one mono MP3 of it, as if a phone recorded the room:

```bash
demo/render_import.py <voices-dir> "/tmp/Onboarding design review.mp3"
```

The voices were chosen so that the speaker diarization tells all three apart on Automatic; with two similar piper voices it finds only two speakers. Check it before shooting:

```bash
target/release/omarchy-meeting-recorder transcribe-file "/tmp/Onboarding design review.mp3" --language en
```

**The finished demo meeting** (Launch sync, with the speakers renamed Maya and Tom and six chapters) is kept in `media/demo-data/launch-sync-meeting.tgz`. To make it from scratch: record the demo in the VM (step 5), transcribe it, rename the speakers on the done page, and make the chapters on the host, since the VM has no agent logged in: pull the meeting folder, run the chapter prompt from `src/chapters.rs` through `omarchy-meeting-recorder ask "<prompt>" < lines.txt`, write the result into the `.meeting-recorder` file and the `## Chapters` list in `transcript.md`, and push it back.

## 3. The VM

```bash
omavm boot
omavm hypr eval 'hl.monitor({ output = "Virtual-1", mode = "3840x2160@60", scale = 2 })'
```

The monitor has to be 3840x2160 at scale 2: integer scaling keeps everything sharp, and all coordinates below are logical pixels of that 1920x1080 space. `hyprctl reload` and `omarchy theme set` put the monitor back to its odd default, so make the mode stick by adding it to the guest's monitor config once:

```bash
omavm user 'echo "hl.monitor({ output = \"Virtual-1\", mode = \"3840x2160@60\", position = \"0x0\", scale = 2 })" >> ~/.config/hypr/monitors.lua'
```

### Installing the app in the guest

```bash
omavm push target/release/omarchy-meeting-recorder /usr/local/bin/omarchy-meeting-recorder
omavm push target/preview/release/examples/transcribe_animation /usr/local/bin/transcribe-animation-preview
omavm ssh 'mkdir -p /tmp/stage; chmod 755 /usr/local/bin/omarchy-meeting-recorder /usr/local/bin/transcribe-animation-preview'
for f in data/omarchy-meeting-recorder.xml data/omarchy-meeting-recorder.desktop \
         media/demo-data/maya.wav media/demo-data/tom.wav media/demo-data/launch-sync-meeting.tgz \
         "media/demo-data/Onboarding design review.mp3"; do
  omavm push "$f" /tmp/stage/
done
omavm push ~/.local/share/voxtype/models/ggml-large-v3-turbo.bin /tmp/stage/model.bin
omavm ssh 'chmod -R a+r /tmp/stage'
```

`omavm user` runs a command as the guest user, but without the session bus that GTK apps, `pactl` and `omarchy theme set` need. Every such command starts with `export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus` (`$BUS` in `lib.sh`).

```bash
omavm user 'export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
  mkdir -p ~/.local/share/omarchy-meeting-recorder/models ~/.local/share/mime/packages ~/Documents/Meetings ~/Downloads ~/.local/state/omarchy-meeting-recorder
  cp /tmp/stage/model.bin ~/.local/share/omarchy-meeting-recorder/models/ggml-large-v3-turbo.bin
  cp /tmp/stage/omarchy-meeting-recorder.xml ~/.local/share/mime/packages/ && update-mime-database ~/.local/share/mime
  sed "s|^Exec=omarchy-meeting-recorder|Exec=/usr/local/bin/omarchy-meeting-recorder|" /tmp/stage/omarchy-meeting-recorder.desktop > ~/.local/share/applications/omarchy-meeting-recorder.desktop
  xdg-mime default omarchy-meeting-recorder.desktop application/x-omarchy-meeting
  echo "{\"format\":\"stereo\",\"language\":\"en\",\"your_name\":\"Maya\"}" > ~/.local/state/omarchy-meeting-recorder/settings.json
  cp "/tmp/stage/Onboarding design review.mp3" ~/Downloads/
  cd ~/Documents/Meetings && tar xzf /tmp/stage/launch-sync-meeting.tgz'
omavm plugin plugin      # the bar widget, registered and the shell restarted
```

The Hyprland float rule for the window goes into the guest's `~/.config/hypr/windows.lua` (the README has the three lines), followed by `omavm hypr reload`. Clear the two first-boot notifications with `omarchy-notification-dismiss "Update System"` and `omarchy-notification-dismiss "Learn Keybindings"`.

For the Nautilus shots, two extra folders make the list look lived-in: copies of the Launch sync meeting renamed `202609221000 Design review` and `202609231430 Customer call Acme`, with their dates set with `touch -d`.

For the animation shots in other themes, a 90-second copy of the demo meeting in `~/demo-short` transcribes quickly with Transcribe again (cut `audio.ogg` and both `.tracks` files with `ffmpeg -t 90`).

### The virtual microphone

The VM has no audio device. A null sink with a remapped monitor becomes the microphone, and a second null sink becomes the speakers, whose monitor the app records as the computer audio:

```bash
omavm user 'export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
  pactl load-module module-null-sink sink_name=fakemic
  pactl load-module module-remap-source master=fakemic.monitor source_name=demo_mic
  pactl load-module module-null-sink sink_name=demo_speakers
  pactl set-default-sink demo_speakers
  pactl set-default-source demo_mic
  cd /tmp && ffmpeg -v error -y -t 95 -i /tmp/stage/maya.wav /tmp/maya-90.wav && ffmpeg -v error -y -t 95 -i /tmp/stage/tom.wav /tmp/tom-90.wav'
```

For the device menus above the meters, the ready, recording and paused shots record the computer audio from a third null sink called USB Headset, picked in the settings, while the microphone stays on System default (play Tom into `demo_headset` for those):

```bash
pactl load-module module-null-sink sink_name=demo_headset 'sink_properties=device.description="USB\ Headset"'
# in settings.json: "system_device":"demo_headset.monitor","system_device_label":"USB Headset"
```

Maya plays into the microphone and Tom into the speakers at the same moment (`talk` in `lib.sh`):

```bash
paplay --device=fakemic /tmp/maya-90.wav & paplay --device=demo_speakers /tmp/tom-90.wav &
```

## 4. Driving the app

Source the helpers first; they need `OUT`, the directory the shots go to:

```bash
export OUT=/tmp/meeting-recorder-shoot; source demo/lib.sh
```

- **Commands** over the app's socket: `omavm user 'omarchy-meeting-recorder start'`, and the same with `pause` (toggles), `stop` and `compact` (toggles). Note that `start` from the command line names the meeting "Meeting HH:MM"; to keep a typed name, press the Start recording button.
- **Keys** go through QEMU and reach the focused window: `omavm sendkey ctrl-a`, `omavm sendkey shift-l`, `omavm sendkey ret`, `omavm sendkey spc`, `omavm sendkey esc`.
- **Clicks and drags** go through QEMU's pointer (the VM has a usb-tablet, so positions are absolute): `demo/qmp.py click X Y`, `demo/qmp.py move X Y`, `demo/qmp.py drag X0 Y0 X1 Y1 [hold]` and `demo/qmp.py release`, in logical pixels. `hold` leaves the button down, to photograph the drop overlay mid-drag. `park` puts the pointer in the bottom right corner before a still.
- **Where things are** with the windows centred: the recording page at 720,203 (480x700), the done page at 410,173 (1100x760). Useful points on the done page of the demo meeting: the third chapter at 605,649, the play button at 827,265, New recording at 695,817, Transcribe again at 749,881, a transcript row at 1150,452 with its edit, next-speaker and delete buttons at 1394, 1424 and 1453 on the same height. On the ready page: the name field at 960,290, Start recording at 960,752.
- **Dragging a file in**: open Nautilus on `~/Downloads`, make it float and put it left of the app (`hl.dsp.window.float`, `hl.dsp.window.resize({ x = 640, y = 360, ... })`, `hl.dsp.window.move({ x = 40, y = 360, ... })`), switch it to the list view with `omavm sendkey ctrl-1`, then `demo/qmp.py drag 217 473 960 700`.
- **A window on another workspace.** A newly opened window can land on a workspace you are not looking at, or keep an odd position from a drag. Find it with `omavm hypr clients -j`, then `omavm hypr dispatch 'hl.dsp.focus({ workspace = "N" })'` to go there, or `hl.dsp.window.move({ workspace = "N", window = "address:0x..." })` to bring it over, and `hl.dsp.window.center({ window = "address:0x..." })` to centre it.
- **The model banner**: `omavm user 'mkdir -p ~/.config/omarchy-meeting-recorder; echo "model = \"tiny\"" > ~/.config/omarchy-meeting-recorder/config.toml'`, start the app, shoot, and remove the file again.
- **The recovery dialog**: start recording, `omavm user 'pkill -9 -x omarchy-meeting'` after about 15 seconds, start the app again. Discard the recording afterwards.

## 5. Stills

`shot NAME` takes a full 4K screenshot and logs the window geometry next to it in `$OUT/geom.txt`. `per_theme "Osaka Jade" osaka` shoots every main state in one theme (done page while playing, ready, recording, paused, compact, the animation). Switching themes is `omarchy theme set "Name"` (`theme` in `lib.sh`); the app follows while it runs. Themes used: Tokyo Night, Osaka Jade, Catppuccin Latte, Gruvbox, Rose Pine, Kanagawa and Everforest.

Crop a window out of a shot with a little wallpaper around it:

```bash
demo/crop.sh "$OUT/osaka-done.png" media/stills/osaka-done.png 410 173 1100 760 1600
```

The four numbers are the window's logical `at` and `size` from `geom.txt`. Whole-desktop versions for `media/stills/context/` are just `magick shot.png -resize 1920x`.

## 6. Videos

Record inside the guest with `gpu-screen-recorder` (4K, 30 fps, H.264 on the CPU, since the VM has no GPU): `rec_start NAME yes` (yes shows the pointer, for takes with clicks and drags) and `rec_stop NAME`, which also pulls the file to `$OUT`. `mark LABEL` writes a timestamped line to the take's log, which is how you find the moments to cut. Make contact sheets to check timings:

```bash
ffmpeg -i take.mp4 -vf "fps=1,scale=200:-1,drawtext=text='%{pts\:hms}':x=2:y=2:fontsize=12:fontcolor=white:box=1:boxcolor=black" f/%03d.png
magick montage f/*.png -tile 12x -geometry +2+2 sheet.png
```

The takes behind `media/videos/`:

| Take | What happens |
|---|---|
| `takeA` | Start the demo tracks, open the app, type "Launch sync", Start recording, pause and resume, compact, drag the strip to the top right, back to full, stop |
| `takeB` | On the 90-second meeting, Transcribe again, the whole animation until the done page |
| `takeC` | On the done page: hover a row, edit it inline, next speaker, delete, Undo |
| `takeD` | On the done page: play from a chapter, the highlight moving along |
| `takeE` | Drag the MP3 from Nautilus onto the ready page, the dialog, Import, transcribing, three speakers |
| `switch` | The done page while `omarchy theme set` goes through six themes |

`demo/cut.py` cuts them. `demo/cut-spec.json` lists every video: which takes, from when to when, which framing (a crop of the 4K capture per aspect ratio: `win` around the 480x700 window, `done` around the done page, `screen` for the whole desktop, `import` for Nautilus next to the app) and a speed-up for long stretches. It writes H.264 yuv420p with faststart, 30 fps, in 1920x1080 and, where the spec asks for it, 1080x1080 (`-square`) and 1080x1920 (`-vertical`). Set `root` in the spec to the directory with the takes:

```bash
demo/cut.py demo/cut-spec.json media/videos              # all videos
demo/cut.py demo/cut-spec.json media/videos transitions  # just one
```

The times in the spec belong to the takes of this shoot; after a new take, read them off the contact sheets and the logs again.

The animated WebP in the README is cut straight from `takeB`:

```bash
ffmpeg -ss 100 -t 6 -i takeB.mp4 -vf "crop=1008:1448:1416:382,scale=420:-1:flags=lanczos,fps=15" -c:v libwebp -q:v 65 -loop 0 -an screenshots/transcribing-animation.webp
```

## 7. Close-ups and social images

`demo/details.py` turns a crop of a 4K shot (physical pixels) into a card with rounded corners and a soft shadow on a dark field, with a caption and a subtitle. `--size` sets the canvas (1600x900 by default, 1200x1200 for square ones), `--scale` caps how much the crop may grow (it is never scaled up past it, so it stays sharp), `--logo` adds the Omarchy wordmark bottom right, and `--bg` puts it on another field:

```bash
demo/details.py takeshot.png 852 940 720 500 media/closeups/chapters.png \
  "Chapters by your default agent" "Click one and it plays from there" --radius 20 --scale 1.2
demo/bright_field.py /tmp/bright.png
demo/details.py play.png 820 346 2200 1520 media/social/record-meetings-bright.png \
  "Record meetings on Omarchy" "Two tracks, a local transcript, chapters and a player" \
  --radius 22 --logo --scale 0.6 --bg /tmp/bright.png --subtitle-color '#FFE3EE'
```

`demo/theme_grid.py` puts the same screen from several themes side by side:

```bash
demo/theme_grid.py media/social/six-themes-done.png 3 470 "It wears your Omarchy theme" \
  shots/tokyo-done.png:410,173,1100,760:"Tokyo Night" shots/osaka-done.png:410,173,1100,760:"Osaka Jade" ...
```

## 8. The README set

The README uses a lean selection from `media/`, converted to WebP in `screenshots/`:

```bash
magick media/stills/tokyo-done-playing.png -resize '1600x>' -strip -quality 88 screenshots/done.webp
```

`preview.png` in the repository root is the whole-desktop shot of the done page, 1600 pixels wide.

## 9. Afterwards

`omavm stop`. Never overwrite the `fresh` snapshot. Nothing from the VM needs cleaning up on the host beyond the scratch directory in `$OUT`.
