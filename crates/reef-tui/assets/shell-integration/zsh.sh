# reef — auto-SSH on terminal split
#
# When reef is running in SSH mode it emits OSC 7 pointing at an anchor
# directory under ~/.reef/sessions/<pid>. Terminals that honour OSC 7
# (Ghostty, iTerm2, Terminal.app, Alacritty, WezTerm, kitty, …) pass
# that path to every new split / tab spawned from the reef pane. This
# snippet detects the anchor, verifies reef is still alive, and execs
# `ssh` into the same host + workdir — reusing the ControlMaster socket
# for zero re-authentication.
#
# Escape hatch: `cd ~` inside a reef pane before splitting, or open a
# new window (not a split) to get a plain local shell.
if [[ "$PWD" == "$HOME/.reef/sessions/"* && -z "$REEF_SESSION_ACTIVE" ]]; then
  _reef_info="$PWD/ssh-info"
  _reef_pid="${PWD##*/}"
  if [[ -f "$_reef_info" ]] && ps -p "$_reef_pid" > /dev/null 2>&1; then
    # Parse the plain KEY=VALUE file manually — no `source`, so values
    # are never shell-expanded and may contain spaces / apostrophes /
    # dollar signs literally.
    while IFS='=' read -r _reef_k _reef_v; do
      case "$_reef_k" in
        REEF_HOST|REEF_WORKDIR|REEF_CONTROL_PATH) typeset -g "$_reef_k"="$_reef_v" ;;
      esac
    done < "$_reef_info"
    export REEF_SESSION_ACTIVE=1
    unset _reef_info _reef_pid _reef_k _reef_v
    exec ssh -t \
      -o "ControlMaster=auto" \
      -o "ControlPath=$REEF_CONTROL_PATH" \
      "$REEF_HOST" \
      "cd ${(q)REEF_WORKDIR} && exec \$SHELL -l"
  fi
  unset _reef_info _reef_pid
  cd ~  # reef died or metadata is corrupt — fall through to a plain shell
fi
