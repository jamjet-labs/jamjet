# demo-unsafe-tool-call.tape
# Generate with: vhs demo-unsafe-tool-call.tape
#
# The GIF shows only the demo running. The README explains `pip install jamjet`
# in text right above the GIF, so the GIF stays focused on the safety story.
# We activate the local editable venv silently before the visible recording starts
# and override the shell prompt so the visible terminal looks generic ($).
Output demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1100
Set Height 700
Set Theme "Dracula"
Set TypingSpeed 50ms
Set Padding 20

Hide
Type "source /Users/sunilp/Development/sunil-ws/jamjet/sdk/python/.venv/bin/activate"
Enter
Sleep 300ms
Type "cd /tmp && rm -rf jamjet-demo-gif && mkdir jamjet-demo-gif && cd jamjet-demo-gif"
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
