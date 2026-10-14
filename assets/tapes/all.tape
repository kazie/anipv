# Animated README demo. Record it with `just record` (needs vhs and ttyd), or
# run the "README recordings" workflow; both prepare assets/demo-home first.
# all.txt is the same recording as text, the screen after each step: the GIF
# is never byte-identical between recordings, so `git diff` on all.txt shows
# whether anything but timing or colors changed.
Output assets/img/demo.gif
Output assets/tapes/all.txt

Set Shell "bash"
Set FontSize 16
Set Width 1300
Set Height 640
Set Padding 16
Set Theme "Catppuccin Mocha"
Set TypingSpeed 80ms

Env ANIPV_HOME "assets/demo-home"

Hide
Type "anipv" Enter
Sleep 1.5s
Show

# Up next: move down two rows, queue the next episode of two shows.
Sleep 1.5s
Down Sleep 400ms Down Sleep 400ms
Space Sleep 500ms Space Sleep 800ms
# Queue: move the first entry down.
Type "5" Sleep 1.5s
Type "J" Sleep 800ms
# Series: fuzzy search, open the match.
Type "3" Sleep 1s
Type "/meitantei" Sleep 800ms Enter Sleep 500ms
Enter Sleep 1.5s
# Play the episode under the cursor. The demo's player is `anipv fake-mpv
# --demo` (10x speed); keys now go to it (remote mode), as they would to mpv:
# skip ahead 5 s twice and a minute once, pause and resume.
Enter Sleep 2.5s
Right Sleep 700ms Right Sleep 700ms
Up Sleep 1.2s
Space Sleep 1.8s
Space Sleep 1.2s
# ctrl-c: the keys are anipv's again while it plays on; v hands them back.
Ctrl+C Sleep 1.2s
Down Sleep 500ms Up Sleep 800ms
Type "v" Sleep 1s
# q quits the player: anipv remembers where it stopped.
Type "q" Sleep 2.5s
# Show the extras.
Type "x" Sleep 1.2s
# Set a status: paused, with a note.
Type "s" Sleep 1s
Down Sleep 600ms Enter Sleep 500ms
Type "catching up later" Sleep 600ms Enter Sleep 1.5s
# Back to the series list, the help, and Up next (paused shows are hidden).
Escape Sleep 800ms
# Files as folders (4 again): the roots, into Anime, then the Monogatari Series
# franchise folder; space queues an entry's unwatched episodes, in order.
Type "4" Sleep 1s
Type "4" Sleep 1.2s
Down Sleep 400ms Enter Sleep 1.2s
Down Sleep 300ms Down Sleep 300ms Down Sleep 500ms Enter Sleep 1.5s
Down Sleep 400ms Down Sleep 600ms Space Sleep 1.8s
Type "5" Sleep 1.8s
# The help scrolls; the remote-mode keys are at its end.
Type "?" Sleep 1.2s PageDown Sleep 700ms PageDown Sleep 700ms PageDown Sleep 2s Escape Sleep 500ms
Type "1" Sleep 1.5s
Type "q" Sleep 800ms
Type "y"
