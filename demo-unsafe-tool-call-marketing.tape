# demo-unsafe-tool-call-marketing.tape — Marketing MP4
# Generate with: vhs demo-unsafe-tool-call-marketing.tape
#
# Marketing-quality MP4 (1280x720, 18pt, slower paced) intended as source
# for a polished product demo. Pair with Screen Studio (or similar) to add:
#   - 5-7s opening title: "JamJet blocks unsafe tool calls before they execute."
#   - Zoom/callouts on: agent request, policy check, decision, audit event
#   - 5s closing screen: "Frameworks call tools. JamJet governs them."
#
# Three export targets from the edited version:
#   - GitHub README: 10-15s GIF (use demo-unsafe-tool-call.tape instead)
#   - jamjet.dev landing: 20-30s MP4 with title + captions
#   - LinkedIn / X / Dev.to: 30-45s MP4 with hook + narration
Output demo-unsafe-tool-call.mp4

Set Shell "bash"
Set FontSize 18
Set Width 1280
Set Height 720
Set Theme "Dracula"
Set TypingSpeed 35ms
Set Padding 28

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
Sleep 500ms
Enter
# Give the viewer time to read scenario, policy check, decision, audit
Sleep 7s
