# Put the chosen command on the command line. Does not run it.
cmd() {
  local line
  line="$(command cmd --print "$@")" || return $?
  print -z -- "$line"
}
