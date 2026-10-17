# Recorded by .github/workflows/demo.yml: vhs demo.tape
Output docs/demo.gif

Set Shell "bash"
Set FontSize 16
Set Width 1100
Set Height 560
Set Padding 20
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms

Hide
Type "export PATH=$PWD/target/release:$PATH PS1='$ '" Enter
Type "(scripts/demo-log.sh /tmp/demo/app.log &) ; sleep 0.5; clear" Enter
Show

Type "rtail -F -n 5 /tmp/demo/app.log"
Sleep 500ms
Enter
Sleep 10s
Ctrl+C
Sleep 1s
