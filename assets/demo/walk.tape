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
Type "cd && token-max doctor --plain < /dev/null > /dev/null; clear"
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
# The two measured changes: the 1h cache, a safe win checked already, and fast mode, a
# tradeoff left for the person to check. The proof under each shows what was replayed.
Type "j"
Sleep 500ms
Right
Sleep 4s
Type "k"
Sleep 500ms
Right
Sleep 3s
Space
Sleep 2s
# The exact edits, then the write.
Enter
Sleep 4s
Type "y"
Sleep 3s
# Then the advice the transcripts can't price, from the same scan: its Bash cap, which asks
# for a fresh run now that the 1h cache it is compared with is written, auto memory off,
# since the sessions ran in a repository with an AGENTS.md, a smaller compaction window,
# and a Fable advisor above the Opus main model.
Type "y"
Sleep 4s
Right
Sleep 3s
Left
Space
Sleep 1s
Type "j"
Space
Sleep 1s
Type "j"
Space
Sleep 1s
Type "j"
Space
Sleep 2s
Enter
Sleep 4s
Type "y"
Sleep 8s
