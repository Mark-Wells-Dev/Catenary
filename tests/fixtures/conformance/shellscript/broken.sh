#!/bin/bash
# Conformance fixture (tui-rework 07).
# Intentional diagnostic: shellcheck (bash-language-server's diagnostic source)
# flags the unassigned, unquoted expansion — SC2154 / SC2086.
echo $undefined_variable
