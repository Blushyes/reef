# reef — auto-SSH on terminal split (fish variant)
#
# See zsh.sh for the full explanation. The ssh-info file is plain
# KEY=VALUE (no quoting), so values with spaces / apostrophes / dollar
# signs are preserved verbatim.
if string match -q "$HOME/.reef/sessions/*" -- "$PWD"; and test -z "$REEF_SESSION_ACTIVE"
    set -l _reef_info "$PWD/ssh-info"
    set -l _reef_pid (basename "$PWD")
    if test -f "$_reef_info"; and ps -p $_reef_pid > /dev/null 2>&1
        while read -l _reef_line
            set -l _reef_kv (string split -m1 '=' -- $_reef_line)
            if test (count $_reef_kv) -eq 2
                switch $_reef_kv[1]
                    case REEF_HOST REEF_WORKDIR REEF_CONTROL_PATH
                        set -gx $_reef_kv[1] $_reef_kv[2]
                end
            end
        end < "$_reef_info"
        set -gx REEF_SESSION_ACTIVE 1
        exec ssh -t \
            -o "ControlMaster=auto" \
            -o "ControlPath=$REEF_CONTROL_PATH" \
            "$REEF_HOST" \
            "cd "(string escape -- "$REEF_WORKDIR")" && exec \$SHELL -l"
    end
    cd ~
end
