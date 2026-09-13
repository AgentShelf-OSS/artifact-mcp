# FrontPocket review

Reviewed upstream at `dfd648b615c4556f1c40c184d2152a5025cf0dcd`:
https://github.com/markd89/FrontPocket/tree/dfd648b615c4556f1c40c184d2152a5025cf0dcd

FrontPocket is a desktop player around Pocket TTS. It provides sentence navigation, configurable lookahead, retained audio for backward navigation, and a persistent local audio output stream. It is a design reference for Artifact MCP, not another speech model to deploy.

Neil chose a Replay sentence action in the expanded native player, without previous/next sentence controls. The implementation uses the current stream's existing decoded audio and Pocket word timestamps. There is no additional cache or generation request. The current chunk's existing 4 MiB framed-stream limit bounds decoded float samples to 8 MiB; Stop, page exit, and advancing to another chunk release them. It does not retain the previous paragraph.

The action is disabled when sentence timing or the start of the sentence is unavailable. It uses the same browser sentence segmentation as Pocket text chunking. Replay from pause starts playback. Existing paragraph navigation and 15-second rewind remain available.

Browser verification with the real Pocket worker checked replay of the second sentence, absence of additional speech requests, replay from pause, a speed change to 1.5×, Stop disabling replay and clearing highlighting, and mobile layout. No page errors were observed.

`build_demo.py` is an unshipped prototype generator from the initial exploration. The native Replay sentence action is the chosen scope.
