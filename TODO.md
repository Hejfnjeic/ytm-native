# TODO

What's still open as of 1.0. What the app does is in [README.md](README.md); how it's built, in
[HACKING.md](HACKING.md).

## To check
- [ ] **Windows self-update** on a real machine: the portable copy downloads
      `ytm-native-portable-<version>.zip` and swaps its exe as it closes. Linux's update was
      tested end to end with a fake release; Windows never runs here (friends report).
- [ ] CPU use at 180 Hz on the owner's screen (measured only at 60 Hz, headless).
- [ ] Non-Premium accounts: playback works through the visionOS client, but nothing was checked
      about ads.

## Could be done
- [ ] Translations: several languages (Hindi, Thai, Vietnamese, Chinese) read like machine
      translation in places; a pass by someone who speaks them.
- [ ] ~20 MB less memory: WebKitGTK is linked into the main program only for the lyrics check's
      helper (`ytm-native --turnstile`). A separate helper program would keep it out, at the
      cost of a second file in every package and in updates.
- [ ] Keep yt-dlp's solver (`app/assets/ejs/`) and the stream clients (`innertube.rs`,
      `webmusic.rs`) in step with yt-dlp when YouTube changes.

## Not wanted (owner's decisions)
Downloads, Discord status, scrobbling, Spotify import, listening together, song recognition,
AirPlay, Stream Deck, animated covers, romanized lyrics, any disk cache, more themes, other app
icons, an in-app web login, add-to-playlist, software rendering, Vulkan, an AUR package.
