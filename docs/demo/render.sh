#!/bin/sh
for i in 1 2 3 4; do doum progress "clip $i/4" --eta "$(( (5 - i) * 4 ))s"; sleep 4; done
