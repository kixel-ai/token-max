# The README's recording of `token-max doctor`, and the `vibes` it hands off to, on synthetic
# transcripts.
# Regenerate from the repository root after `cargo build --release`:
#   nix shell nixpkgs#vhs nixpkgs#python3 -c vhs assets/demo/walk.tape

Output assets/demo/walk.gif

Set Shell bash
Set FontSize 15
Set Width 1100
Set Height 720
Set Padding 16
Set TypingSpeed 40ms
Set Theme "Catppuccin Mocha"

Hide
# A clean environment, so no gateway or profile from this machine leaks in; the home and
# its transcripts are synthetic, and a first run caches the public prices. Any setup failure
# ends the shell, so the scripted answers never reach this machine's real settings.
Type `{ rm -rf .tmp/demo && python3 assets/demo/home.py .tmp/demo && exec env -i HOME="$PWD/.tmp/demo" PATH="$PWD/target/release:$PATH" TERM=xterm-256color LANG=C.UTF-8 PS1='$ ' bash --norc --noprofile; } || exit 1`
Enter
Sleep 1s
Type "cd && token-max doctor > /dev/null; clear"
Enter
Sleep 3s
Show

Type "token-max doctor"
Sleep 500ms
Enter
Sleep 2s
# How the profile pays: usage credits, so its main agent's default cache is 5m.
Type "a"
Sleep 300ms
Enter
Sleep 4s
# The two measured changes: fast mode off, then the 1h cache.
Type "y"
Sleep 300ms
Enter
Sleep 3s
Type "y"
Sleep 300ms
Enter
Sleep 5s
# Write them.
Type "y"
Sleep 300ms
Enter
Sleep 4s
# The walk is over: show the advice the transcripts can't price.
Type "y"
Sleep 300ms
Enter
Sleep 5s
# Its Bash cap, lowered under the 5m cache the scan saw.
Type "y"
Sleep 300ms
Enter
Sleep 4s
# The maintainer's smaller compaction window, then the write.
Type "y"
Sleep 300ms
Enter
Sleep 4s
Type "y"
Sleep 300ms
Enter
Sleep 8s
