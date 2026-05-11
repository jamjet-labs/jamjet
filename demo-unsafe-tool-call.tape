# demo-unsafe-tool-call.tape — README GIF
# Generate with: vhs demo-unsafe-tool-call.tape
#
# README hero GIF. Smaller dimensions (1100x700), 16pt, ~7s. The README
# explains `pip install jamjet` in text right above this GIF, so the GIF
# shows only the safety demo running for maximum focus.
#
# For the marketing-quality MP4 (1280x720, 18pt, slower paced for narration
# overlays), see demo-unsafe-tool-call-marketing.tape.
Output demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1100
Set Height 700
Set Theme "Dracula"
Set TypingSpeed 50ms
Set Padding 20

Hide
Type "source ${JAMJET_DEMO_VENV:-./sdk/python/.venv/bin/activate}"
Enter
Sleep 300ms
Type "cd /tmp && rm -rf jamjet-demo && mkdir jamjet-demo && cd jamjet-demo"
Enter
Sleep 300ms
Type "export PS1='$ '"
Enter
Sleep 300ms
Type "clear"
Enter
Sleep 800ms
Show

Type "jamjet demo unsafe-tool-call"
Sleep 700ms
Enter
Sleep 5s
