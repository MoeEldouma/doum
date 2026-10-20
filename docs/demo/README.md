# The demo GIF

`docs/demo.gif` is recorded from a throwaway doum daemon running harmless CPU jobs (no GPU is used):
`render.sh` stands in for a ComfyUI render, `train.sh` for a training run that saves a checkpoint when paused and
resumes from it, `writer.sh` for an LLM someone is waiting for.

To record it again (needs [agg](https://github.com/asciinema/agg) to turn the recording into a GIF):

```sh
cd docs/demo
mkdir -p bin state && ln -sf "$(command -v doum)" bin/doum
DOUM_CONFIG=$PWD/doum.toml DOUM_STATE_DIR=$PWD/state bin/doum daemon &   # listens on 127.0.0.1:8499
python3 rec.py demo.cast "$PWD/state"
agg --font-size 15 --theme monokai --idle-time-limit 2 --speed 1.3 --last-frame-duration 4 demo.cast ../demo.gif
```

Stop the demo daemon afterwards, and delete `state/`, `bin/` and `train.ckpt`.

# The web panel GIF

`docs/media/panel.gif` is the web panel recorded during a scripted scenario (`panelrec.py`): `work.sh` stands in for
renders, batches and agent runs that report progress, `train2.sh` for a training run that checkpoints on pause. It
uses its own config (`panel.toml`, port 8497) and needs Python Playwright with Chromium.

```sh
cd docs/demo
mkdir -p bin st && ln -sf "$(command -v doum)" bin/doum
DOUM_CONFIG=$PWD/panel.toml DOUM_STATE_DIR=$PWD/st bin/doum daemon &      # 127.0.0.1:8497
sleep 25                                                                   # a few chart samples first
python3 panelrec.py "$PWD/st" "$PWD/frames"                                # ~150 s, one frame every 0.75 s
ffmpeg -framerate 8 -i frames/f%04d.png -vf "scale=1000:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=4:diff_mode=rectangle" -loop 0 ../media/panel.gif
```

# The phone alerts images

`docs/media/phone-alerts.{gif,png}` are rendered from `phone.html` (an illustration using doum's real alert text):
open it in Playwright at 1200x860, call `all()` for the still or `play(1300)` and capture frames for the GIF.
