### The short notices at the bottom of the window.

# After unliking several songs at once; Undo sits beside it.
likes-removed = { $count ->
    [one] Removed { $count } like
   *[other] Removed { $count } likes
}

# An AirPlay speaker picked as the output device wouldn't play; the music stays on this
# computer. { $reason } is one of the airplay- lines below, starting in lower case.
# "sysctl …" and "CAP_NET_BIND_SERVICE" are a command and a Linux name: keep them as they are.
# The AirPlay speaker playing hung up (turned off, or another device took it over).

## Recognize song: the card by the player bar while it listens to what the computer
## plays, and with what Shazam heard.

# The card's window title. Experimental: it uses an unofficial Shazam service.
# The command: in the tray menu, the shortcuts, the palette, and on the search box.
# Buttons on the card.
