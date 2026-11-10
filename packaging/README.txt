FramePlayerX — a native VR video player for the Valve Steam Frame
==================================================================

Run: ./frameplayerx.sh            (Steam starts it this way)
     ./frameplayerx.sh VIDEO      (open a file or URL directly)

Files
  frameplayerx        the player
  frameplayerx.sh     launcher (sets up lib/)
  lib/                FFmpeg 7.1.1 + dav1d 1.5.3 (LGPL / BSD), see licenses/
  assets/steam/       Steam library artwork
  VERSION             installed version
  LICENSE, NOTICE     FramePlayerX itself: Apache License 2.0

Your data
  Settings:   ~/.config/frameplayerx/settings.json
  Sources:    ~/.config/frameplayerx/sources.json (passwords; owner-only)
  Videos:     ~/.local/share/frameplayerx/
  Log:        ~/.local/share/frameplayerx/frameplayerx.log (last run: .log.old)

Videos in ~/Videos, ~/Downloads and on microSD cards / USB drives are found
automatically. Haptic scripts (.funscript) next to a video or in
~/.local/share/frameplayerx/Interactive are picked up too.

User guide: https://github.com/frankyfife/frameplayerx_ff/blob/main/docs/USER-GUIDE.md
