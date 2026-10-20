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
