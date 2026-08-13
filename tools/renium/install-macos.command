#!/bin/sh
cd "$(dirname "$0")" || exit 1
printf '%s\n\n' "Installing Renium..."
./install.sh --interactive
result=$?
printf '\n'
if [ "$result" -eq 3 ]; then
  printf '%s\n' "Installation cancelled."
  result=0
elif [ "$result" -eq 0 ]; then
  printf '%s\n' "Installation complete. Restart your editor and Roblox Studio."
else
  printf '%s\n' "Installation failed. The error is shown above."
fi
printf '%s' "Press Return to close..."
read -r _
exit "$result"
