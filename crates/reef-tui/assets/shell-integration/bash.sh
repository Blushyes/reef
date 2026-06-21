# reef — auto-SSH on terminal split
#
# See zsh.sh for the full explanation. This is the bash variant.
if [[ "$PWD" == "$HOME/.reef/sessions/"* && -z "$REEF_SESSION_ACTIVE" ]]; then
  _reef_info="$PWD/ssh-info"
  _reef_pid="${PWD##*/}"
  if [[ -f "$_reef_info" ]] && ps -p "$_reef_pid" > /dev/null 2>&1; then
    while IFS='=' read -r _reef_k _reef_v; do
      case "$_reef_k" in
        REEF_HOST|REEF_WORKDIR|REEF_CONTROL_PATH) declare "$_reef_k=$_reef_v" ;;
      esac
    done < "$_reef_info"
    export REEF_SESSION_ACTIVE=1
    _reef_workdir_q=$(printf '%q' "$REEF_WORKDIR")
    unset _reef_info _reef_pid _reef_k _reef_v
    exec ssh -t \
      -o "ControlMaster=auto" \
      -o "ControlPath=$REEF_CONTROL_PATH" \
      "$REEF_HOST" \
      "cd $_reef_workdir_q && exec \$SHELL -l"
  fi
  unset _reef_info _reef_pid
  cd ~
fi
