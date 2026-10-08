# Animated README demo. Record it with `just record` (needs vhs and ttyd), or
# run the "README recordings" workflow; both prepare assets/demo-home first.
Output assets/img/demo.gif

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
# Series: fuzzy search, open the match, show its extras.
Type "3" Sleep 1s
Type "/meitantei" Sleep 800ms Enter Sleep 500ms
Enter Sleep 1.5s
Type "x" Sleep 1.2s
# Set a status: paused, with a note.
Type "s" Sleep 1s
Down Sleep 600ms Enter Sleep 500ms
Type "catching up later" Sleep 600ms Enter Sleep 1.5s
# Back to the series list, the help, and Up next (paused shows are hidden).
Escape Sleep 800ms
Type "?" Sleep 2s Escape Sleep 500ms
Type "1" Sleep 1.5s
Type "q" Sleep 800ms
Type "y"
